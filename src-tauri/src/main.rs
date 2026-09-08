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

mod diag;
mod statusline;
mod activity;
mod pty;
mod sessions;
mod usage;
use diag::*;
use statusline::*;
use activity::*;
use pty::*;
use sessions::*;
use usage::*;

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
fn session_file_in_store(file: &str) -> Result<PathBuf, String> {
    let p = fs::canonicalize(file).map_err(|e| e.to_string())?;
    let home = dirs::home_dir().ok_or("no home dir")?;
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
    // 캐시 키는 스캔 당시의 원본 경로 문자열 (canonicalize한 \\?\ 형태가 아님)
    META_CACHE.lock().unwrap_or_else(|e| e.into_inner()).remove(&file);
    Ok(())
}

fn main() {
    // 상태줄 명령으로 불린 경우 GUI를 띄우지 않고 stdin만 처리한다
    if std::env::args().any(|a| a == STATUSLINE_FLAG) {
        run_statusline_tap();
        return;
    }
    install_panic_hook();
    init_trace();
    tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(
            // VISIBLE 플래그 제외: 창 표시는 WebView 로드 후 프런트에서 수행 (IME 초기화 버그 회피)
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::all()
                        - tauri_plugin_window_state::StateFlags::VISIBLE,
                )
                .build(),
        )
        .manage(PtyState::default())
        .invoke_handler(tauri::generate_handler![
            pty::spawn_pty,
            pty::write_pty,
            pty::resize_pty,
            pty::kill_pty,
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
            usage::session_turns,
            usage::subscription_state,
            usage::codex_state,
            open_log_file,
            open_path,
        ])
        .setup(|app| {
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

            // WebView2 초기 IME 바인딩 버그 우회: 시작 직후 포커스를 프로그램적으로
            // 재이동시켜 "다른 창 갔다 오기"와 동일한 재바인딩을 강제한다.
            // 이게 없으면 첫 입력에서 한글 조합이 중복되고 조합창이 화면 구석에 뜬다.
            if let Some(w) = app.get_webview_window("main") {
                let w2 = w.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(700));
                    let _ = w2.with_webview(|webview| unsafe {
                        use webview2_com::Microsoft::Web::WebView2::Win32::{
                            COREWEBVIEW2_MOVE_FOCUS_REASON_NEXT,
                            COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC,
                        };
                        let controller = webview.controller();
                        let _ = controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_NEXT);
                        let _ = controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
                    });
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
                let state = window.app_handle().state::<PtyState>();
                let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
                for (_, mut p) in map.drain() {
                    let _ = p.killer.kill();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running cli-deck");
}
