// Claude Deck — 세션 사이드바 + 임베디드 PTY 터미널로 claude CLI를 그대로 구동하는 데스크톱 앱.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base64::Engine;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::PathBuf,
    sync::Mutex,
    time::UNIX_EPOCH,
};
use tauri::{AppHandle, Emitter, Manager, State};

// ---------- PTY 관리 ----------

struct PtyInstance {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
}

#[derive(Default)]
struct PtyState(Mutex<HashMap<String, PtyInstance>>);

#[derive(Clone, Serialize)]
struct PtyOutput {
    id: String,
    data: String, // base64
}

#[derive(Clone, Serialize)]
struct PtyExit {
    id: String,
}

#[tauri::command]
fn spawn_pty(
    app: AppHandle,
    state: State<PtyState>,
    id: String,
    cwd: String,
    command: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let mut map = state.0.lock().unwrap();
    if map.contains_key(&id) {
        return Ok(()); // 이미 실행 중
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| e.to_string())?;

    // 최종 명령은 프런트에서 합성됨: [래퍼 접두사] + 에이전트 명령 + [--resume <세션ID>]
    let claude_cmd = if command.trim().is_empty() { "claude".to_string() } else { command };
    let mut cmd = CommandBuilder::new("cmd.exe");
    cmd.args(["/c", &claude_cmd]);
    let workdir = if PathBuf::from(&cwd).is_dir() {
        cwd.clone()
    } else {
        dirs::home_dir().unwrap_or_default().to_string_lossy().to_string()
    };
    cmd.cwd(&workdir);

    let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;

    // 출력 스트리밍 스레드
    let app2 = app.clone();
    let id2 = id.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let data = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
                    let _ = app2.emit("pty-output", PtyOutput { id: id2.clone(), data });
                }
            }
        }
        // 자연 종료 시 맵에서 제거해 같은 id로 재시작 가능하게 함
        let state = app2.state::<PtyState>();
        state.0.lock().unwrap().remove(&id2);
        let _ = app2.emit("pty-exit", PtyExit { id: id2.clone() });
    });

    map.insert(id, PtyInstance { master: pair.master, writer, child });
    Ok(())
}

#[tauri::command]
fn write_pty(state: State<PtyState>, id: String, data: String) -> Result<(), String> {
    let mut map = state.0.lock().unwrap();
    if let Some(p) = map.get_mut(&id) {
        p.writer.write_all(data.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn resize_pty(state: State<PtyState>, id: String, cols: u16, rows: u16) -> Result<(), String> {
    let map = state.0.lock().unwrap();
    if let Some(p) = map.get(&id) {
        p.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn kill_pty(state: State<PtyState>, id: String) -> Result<(), String> {
    let mut map = state.0.lock().unwrap();
    if let Some(mut p) = map.remove(&id) {
        let _ = p.child.kill();
    }
    Ok(())
}

// ---------- 세션 스캔 ----------

#[derive(Serialize)]
struct SessionMeta {
    session_id: String,
    cwd: String,
    summary: Option<String>,
    first_prompt: Option<String>,
    last_text: Option<String>,
    message_count: u32,
    mtime: f64,
    file: String,
}

fn extract_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn read_meta(path: &PathBuf) -> Option<SessionMeta> {
    let stat = fs::metadata(path).ok()?;
    let mtime = stat.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs_f64();
    let text = fs::read_to_string(path).ok()?;

    let mut meta = SessionMeta {
        session_id: path.file_stem()?.to_string_lossy().to_string(),
        cwd: String::new(),
        summary: None,
        first_prompt: None,
        last_text: None,
        message_count: 0,
        mtime,
        file: path.to_string_lossy().to_string(),
    };

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if meta.cwd.is_empty() {
            if let Some(c) = obj["cwd"].as_str() {
                meta.cwd = c.to_string();
            }
        }
        if obj["type"] == "summary" {
            if let Some(s) = obj["summary"].as_str() {
                meta.summary = Some(s.to_string());
            }
        }
        let t = obj["type"].as_str().unwrap_or("");
        if t == "user" || t == "assistant" {
            meta.message_count += 1;
            if meta.first_prompt.is_none() && t == "user" && obj["isMeta"] != true {
                let txt = extract_text(&obj["message"]["content"]);
                let txt = txt.trim();
                if !txt.is_empty() && !txt.starts_with('<') && !txt.starts_with("Caveat:") {
                    meta.first_prompt = Some(txt.chars().take(120).collect());
                }
            }
            if t == "assistant" {
                let txt = extract_text(&obj["message"]["content"]);
                let txt = txt.trim();
                if !txt.is_empty() {
                    meta.last_text = Some(txt.chars().take(300).collect());
                }
            }
        }
    }
    Some(meta)
}

#[tauri::command]
fn list_sessions() -> Vec<SessionMeta> {
    let mut out = Vec::new();
    let Some(home) = dirs::home_dir() else { return out };
    let projects = home.join(".claude").join("projects");
    let Ok(dirs_iter) = fs::read_dir(&projects) else { return out };

    for proj in dirs_iter.flatten() {
        let Ok(files) = fs::read_dir(proj.path()) else { continue };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                if let Some(mut meta) = read_meta(&path) {
                    if meta.cwd.is_empty() {
                        // 폴더명(C--workspace-foo)에서 경로 근사 복원
                        let name = proj.file_name().to_string_lossy().to_string();
                        if name.len() > 3 && &name[1..3] == "--" {
                            meta.cwd = format!("{}:\\{}", &name[0..1], name[3..].replace('-', "\\"));
                        } else {
                            meta.cwd = name;
                        }
                    }
                    out.push(meta);
                }
            }
        }
    }
    out.sort_by(|a, b| b.mtime.partial_cmp(&a.mtime).unwrap_or(std::cmp::Ordering::Equal));
    out
}

#[tauri::command]
fn delete_session(file: String) -> Result<(), String> {
    let p = PathBuf::from(&file);
    let projects = dirs::home_dir().ok_or("no home dir")?.join(".claude").join("projects");
    // 세션 저장소 밖의 파일 삭제 방지
    if !p.starts_with(&projects) || p.extension().map(|e| e != "jsonl").unwrap_or(true) {
        return Err("invalid session file path".into());
    }
    fs::remove_file(&p).map_err(|e| e.to_string())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .manage(PtyState::default())
        .invoke_handler(tauri::generate_handler![
            spawn_pty, write_pty, resize_pty, kill_pty, list_sessions, delete_session
        ])
        .setup(|app| {
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

            let show = MenuItem::with_id(app, "show", "열기", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "종료", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Claude Deck")
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
            Ok(())
        })
        .on_window_event(|window, event| {
            // X 버튼 = 트레이로 (세션 유지). 완전 종료는 트레이 메뉴의 "종료"
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running claude-deck");
}
