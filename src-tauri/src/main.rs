// CLI Deck — 멀티 에이전트(Claude/Codex/Gemini) 세션 사이드바 + 임베디드 PTY 터미널 데스크톱 앱.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base64::Engine;
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        LazyLock, Mutex,
    },
    time::UNIX_EPOCH,
};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_notification::NotificationExt;

mod agents;
mod conpty;
mod diag;
mod endsession;
mod links;
mod paste;
mod paths;
mod search;
mod statusline;
mod activity;
mod pty;
mod sessions;
mod update;
mod usage;
use diag::*;
use statusline::*;
use activity::*;
use pty::*;
use sessions::*;
use usage::*;

/// 열려 있는 PTY 자식 프로세스를 모두 끝낸다 (창 닫기, 세션 종료 때).
fn kill_all_ptys(app: &AppHandle) {
    let state = app.state::<PtyState>();
    let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
    for (_, mut p) in map.drain() {
        let _ = p.killer.kill();
    }
}

/// 세션 프로젝트 폴더를 탐색기로 연다
#[tauri::command(async)]
fn open_path(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.is_dir() {
        return Err(format!("폴더가 존재하지 않습니다: {}", path));
    }
    std::process::Command::new("explorer.exe")
        .arg(&p)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 세션 로그 파일(JSONL)을 기본 연결 프로그램으로 연다.
/// 연결 프로그램이 없으면 Windows가 "연결 프로그램 선택" 창을 띄운다 —
/// spawn 자체는 성공하므로 프런트의 catch로는 그 경우를 알 수 없다.
#[tauri::command(async)]
fn open_log_file(file: String) -> Result<(), String> {
    let p = session_file_in_store(&file)?;
    std::process::Command::new("explorer.exe")
        .arg(&p)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 알려진 세션 저장소 안의 세션 파일인지 검사하고 정규화된 경로를 돌려준다.
/// ".." 같은 성분이 섞이면 starts_with 검사를 그냥 통과하므로 반드시 정규화 후 비교.
pub(crate) fn session_file_in_store(file: &str) -> Result<PathBuf, String> {
    let p = fs::canonicalize(file).map_err(|e| e.to_string())?;
    let home = crate::paths::home_dir().ok_or("no home dir")?;
    let allowed = [
        home.join(".claude").join("projects"),
        home.join(".codex").join("sessions"),
        home.join(".gemini").join("tmp"),
    ];
    let in_store = allowed
        .iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .any(|root| p.starts_with(&root));
    let is_session = p
        .extension()
        .map(|e| e == "jsonl" || e == "json")
        .unwrap_or(false);
    if !in_store || !is_session {
        return Err("invalid session file path".into());
    }
    Ok(p)
}

#[tauri::command(async)]
fn delete_session(file: String) -> Result<(), String> {
    // 알려진 세션 저장소 안의 세션 파일만 삭제 허용
    let p = session_file_in_store(&file)?;
    fs::remove_file(&p).map_err(|e| e.to_string())?;
    sessions::remove_title_sidecar(&p);
    // 캐시 키는 스캔 당시의 원본 경로 문자열 (canonicalize한 \\?\ 형태가 아님)
    META_CACHE.lock().unwrap_or_else(|e| e.into_inner()).remove(&file);
    Ok(())
}

/// WebView2가 한글 입력기와의 연결을 놓치면 조합창이 창 왼쪽 위에 뜨고 글자가 하나씩
/// 따로 확정된다(가끔 두 번 들어간다). 다른 창에 갔다 오면 풀리는데, 그게 하는 일이
/// 이것이다 — 포커스를 WebView 안에서 한 번 옮겼다 되돌려 입력기를 다시 붙인다.
fn rebind_webview_focus(w: &tauri::WebviewWindow) {
    let _ = w.with_webview(|webview| unsafe {
        use webview2_com::Microsoft::Web::WebView2::Win32::{
            COREWEBVIEW2_MOVE_FOCUS_REASON_NEXT, COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC,
        };
        let controller = webview.controller();
        let _ = controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_NEXT);
        let _ = controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
    });
}

/// 화면이 시작할 때 한 번 묻는다: 어느 판인지, 어느 ConPTY가 붙었는지, 설치가 온전한지.
#[derive(Serialize)]
struct AppStatus {
    version: String,
    /// 격리 실행 중(paths.rs) — 화면은 창을 앞으로 가져오지 않는다
    sandbox: bool,
    conpty: conpty::ConptyStatus,
    /// 설정에서 고른 값. 지금 붙은 것(conpty.bundled)과 다르면 다시 시작해야 적용된다
    conpty_wanted: bool,
    /// 설치 폴더에서 망가진 파일. 비어 있지 않으면 화면이 "다시 설치"를 권한다
    damaged: Vec<String>,
    /// 앱에 든 것을 안 쓰는데(끔·손상) 다른 conpty.dll이 떠 있으면 그 경로. portable-pty는 이름만으로
    /// 찾기 때문에 PATH에 있는 다른 프로그램의 것(WezTerm 설치 폴더 등)이 걸려 올 수 있다 — 그때
    /// "윈도우 내장"이라고 말하면 틀린다. 첫 탭을 연 뒤에야 알 수 있다(그때 불려 온다).
    conpty_stray: Option<String>,
}

#[tauri::command(async)]
fn app_status(app: AppHandle) -> AppStatus {
    let conpty = conpty::ensure().clone();
    AppStatus {
        version: app.package_info().version.to_string(),
        sandbox: paths::sandbox().is_some(),
        conpty_wanted: !conpty::disabled_by_user(),
        damaged: conpty::install_damage(&conpty),
        conpty_stray: if conpty.bundled { None } else { conpty::loaded_module() },
        conpty,
    }
}

/// 함께 배포한 ConPTY를 쓸지. 불러오는 건 프로세스에 한 번이라 다음 실행부터 적용된다.
#[tauri::command(async)]
fn set_bundled_conpty(enabled: bool) -> Result<(), String> {
    conpty::set_enabled(enabled)
}

/// 격리 실행의 창: 보이게 하되 맨 뒤에 두고 포커스를 가져오지 않는다. 쓰던 앱 옆에서 도는
/// 시험이 치고 있던 글자를 빼앗거나 화면을 가리면 안 된다.
fn show_in_background(w: &tauri::WebviewWindow) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    };
    if let Ok(hwnd) = w.hwnd() {
        unsafe {
            SetWindowPos(hwnd.0 as _, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW);
        }
    }
}

/// 화면이 "입력기가 떨어졌다"고 판단했을 때 부른다 (조합 없이 한글이 들어옴).
/// with_webview는 메인 스레드로 넘겨 실행하므로 여기서는 작업 스레드에서 부른다.
#[tauri::command(async)]
fn rebind_ime(window: tauri::WebviewWindow) {
    trace_always("app", "", "ime", "rebind");
    rebind_webview_focus(&window);
}

fn main() {
    // 상태줄 명령으로 불린 경우 GUI를 띄우지 않고 stdin만 처리한다
    if std::env::args().any(|a| a == STATUSLINE_FLAG) {
        run_statusline_tap();
        return;
    }
    // 격리해 달라고 했는데 못 하면(상대 경로 등) 평소처럼 띄우지 않고 끝낸다 — 격리된 줄 알고
    // 띄운 시험이 쓰던 앱의 탭을 되살리면 안 된다.
    if std::env::var_os("CLI_DECK_SANDBOX").is_some() && paths::sandbox().is_none() {
        eprintln!("CLI_DECK_SANDBOX는 전체 경로여야 합니다");
        std::process::exit(2);
    }
    if let Some(dir) = paths::sandbox() {
        // 격리 실행: WebView2 프로필(탭·설정이 든 localStorage)도 그 폴더로 옮긴다. WebView2는 이
        // 환경 변수를 앱이 넘긴 프로필 경로보다 앞세운다. 안 옮기면 쓰던 앱의 탭 목록을 읽는다.
        let _ = fs::create_dir_all(dir.join("home"));
        let _ = fs::create_dir_all(dir.join("data"));
        std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", dir.join("data").join("EBWebView"));
        // 이 둘은 홈보다 앞서 읽는다(statusline.rs, usage.rs) — 남아 있으면 진짜 설정과 로그인 정보를
        // 읽고 사용량 API까지 부른다. 여기서 지우면 탭의 에이전트에게도 넘어가지 않는다.
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
    }
    install_panic_hook();
    init_trace();
    // 함께 배포한 ConPTY를 미리 불러 둔다 — 파일 확인과 디스크 밀어내기가 몇 ms 걸려 창 띄우기를
    // 막지 않게 따로 돌린다. 그보다 먼저 탭이 열리면 spawn_pty가 끝나기를 기다린다.
    std::thread::spawn(|| {
        conpty::ensure();
    });
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init());
    if paths::sandbox().is_some() {
        // 화면이 스크립트를 돌리기 전에 알 수 있게 — 창을 앞으로 가져오지 않는다(app.js)
        builder = builder.append_invoke_initialization_script("window.__DECK_SANDBOX__ = true;");
    }
    // 격리 실행은 쓰던 앱의 창 위치를 읽지도 덮어쓰지도 않는다
    if paths::sandbox().is_none() {
        builder = builder.plugin(
            // VISIBLE 플래그 제외: 창 표시는 WebView 로드 후 프런트에서 수행 (IME 초기화 버그 회피)
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::all()
                        - tauri_plugin_window_state::StateFlags::VISIBLE,
                )
                .build(),
        );
    }
    builder
        .manage(PtyState::default())
        .manage(update::PendingUpdate::default())
        .invoke_handler(tauri::generate_handler![
            pty::spawn_pty,
            pty::write_pty,
            sessions::rename_session,
            pty::resize_pty,
            pty::kill_pty,
            pty::dir_exists,
            pty::create_dir,
            pty::kill_session_owner,
            agents::agent_versions,
            agents::update_agent,
            agents::agent_changelog,
            agents::translate_changelog,
            sessions::list_sessions,
            delete_session,
            sessions::session_preview,
            diag::trace_enabled,
            diag::set_trace,
            diag::clear_diagnostics,
            activity::set_keepalive,
            statusline::statusline_settings_path,
            diag::trace_ui,
            usage::usage_stats,
            activity::bind_session,
            usage::session_turns,
            usage::subscription_state,
            usage::codex_state,
            open_log_file,
            open_path,
            links::resolve_links,
            links::open_link,
            links::list_editors,
            search::search_sessions,
            paste::save_clipboard_image,
            rebind_ime,
            app_status,
            set_bundled_conpty,
            update::check_update,
            update::install_update,
        ])
        .setup(|app| {
            paste::cleanup_on_start();
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

            let show = MenuItem::with_id(app, "show", "열기", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("CLI Deck")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| match e.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        if let Some(w) = tray.app_handle().get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                })
                .build(app)?;

            spawn_state_monitor(app.handle().clone());
            // 로그오프/종료 때 tao가 패닉하지 않게 (endsession.rs 참고)
            endsession::install(app.handle());

            // WebView2 초기 IME 바인딩 버그 우회: 시작 직후 포커스를 프로그램적으로
            // 재이동시켜 "다른 창 갔다 오기"와 동일한 재바인딩을 강제한다.
            // 이게 없으면 첫 입력에서 한글 조합이 중복되고 조합창이 화면 구석에 뜬다.
            if let Some(w) = app.get_webview_window("main").filter(|_| paths::sandbox().is_some()) {
                // 격리 실행: 포커스를 건드리는 아래 두 가지 대신 뒤에서 조용히 띄운다
                show_in_background(&w);
            } else if let Some(w) = app.get_webview_window("main") {
                let w2 = w.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(700));
                    rebind_webview_focus(&w2);
                });

                // 안전망: 창 표시를 프런트에만 맡기면 JS가 거기까지 못 가는 순간
                // 앱이 트레이에서만 열리는 유령이 된다(실제로 그렇게 됐다).
                // 프런트가 먼저 띄우면 이 스레드는 아무것도 하지 않는다.
                let w3 = w.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(2500));
                    if !matches!(w3.is_visible(), Ok(true)) {
                        trace_always("app", "", "window", "fallback-show");
                        let _ = w3.show();
                        let _ = w3.set_focus();
                    }
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // X 버튼 = 완전 종료. 창을 닫기 전에 열려 있는 PTY 자식 프로세스를
            // 먼저 정리해 고아 프로세스로 남지 않게 한다.
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                kill_all_ptys(window.app_handle());
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running cli-deck");
}
