// CLI Deck — 멀티 에이전트(Claude/Codex/Gemini) 세션 사이드바 + 임베디드 PTY 터미널 데스크톱 앱.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base64::Engine;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::PathBuf,
    sync::{LazyLock, Mutex},
    time::UNIX_EPOCH,
};
use tauri::{AppHandle, Emitter, Manager, State};

// ---------- 크래시 진단 ----------
// windows_subsystem="windows"(릴리스 빌드)는 콘솔이 없어 패닉 메시지(stderr)가
// 그냥 사라진다 — "가끔 팅긴다"는 게 이거였을 가능성이 높음. 패닉 시 로그 파일에
// 기록하고 네이티브 팝업을 띄워 최소한 원인을 알 수 있게 한다.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info.to_string();
        // WebView2도 같은 폴더(%LOCALAPPDATA%\<identifier>)에 프로필을 두므로
        // 별도 폴더를 새로 만들지 않고 거기에 합쳐서 — 삭제/관리 지점을 하나로 유지한다.
        if let Some(dir) = dirs::data_local_dir() {
            let log_dir = dir.join("com.user.cli-deck");
            if fs::create_dir_all(&log_dir).is_ok() {
                use std::io::Write as _;
                if let Ok(mut f) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log_dir.join("crash.log"))
                {
                    let _ = writeln!(f, "[{}] {}", chrono_now_iso(), msg);
                }
            }
        }
        rfd::MessageDialog::new()
            .set_title("CLI Deck 오류")
            .set_description(format!(
                "예기치 않은 오류가 발생했습니다:\n\n{}\n\n로그: %LOCALAPPDATA%\\com.user.cli-deck\\crash.log",
                msg
            ))
            .set_level(rfd::MessageLevel::Error)
            .show();
        default_hook(info);
    }));
}

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
    let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
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
        state.0.lock().unwrap_or_else(|e| e.into_inner()).remove(&id2);
        let _ = app2.emit("pty-exit", PtyExit { id: id2.clone() });
    });

    map.insert(id, PtyInstance { master: pair.master, writer, child });
    Ok(())
}

#[tauri::command]
fn write_pty(state: State<PtyState>, id: String, data: String) -> Result<(), String> {
    let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = map.get_mut(&id) {
        p.writer.write_all(data.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn resize_pty(state: State<PtyState>, id: String, cols: u16, rows: u16) -> Result<(), String> {
    let map = state.0.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = map.get(&id) {
        p.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn kill_pty(state: State<PtyState>, id: String) -> Result<(), String> {
    let mut map = state.0.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut p) = map.remove(&id) {
        let _ = p.child.kill();
    }
    Ok(())
}

// ---------- 세션 스캔 ----------

#[derive(Serialize, Clone)]
struct SessionMeta {
    session_id: String,
    agent: String, // "claude" | "codex" | "gemini"
    cwd: String,
    summary: Option<String>,
    first_prompt: Option<String>,
    last_text: Option<String>,
    message_count: u32,
    mtime: f64,
    file: String,
    /// 마지막으로 프롬프트 캐시가 읽히거나 새로 쓰인 시각 (epoch seconds)
    cache_last_ts: Option<f64>,
    /// 해당 캐시 항목의 TTL (초) — 5분(300) 또는 1시간(3600)
    cache_ttl_secs: Option<u32>,
    /// 마지막 assistant 응답 시점의 컨텍스트 토큰 수 (사이드바 게이지용)
    ctx_tokens: Option<u64>,
    /// codex는 파일에 컨텍스트 윈도우가 직접 기록됨 (claude는 프런트에서 모델명으로 추정)
    ctx_window: Option<u64>,
    /// 마지막으로 관측된 모델명
    model: Option<String>,
    /// 호버 미리보기용 최근 대화 (최대 3턴 = 6개). 오래된 것부터 순서대로.
    recent: Vec<RecentMsg>,
}

#[derive(Serialize, Clone)]
struct RecentMsg {
    role: String, // "user" | "assistant"
    text: String,
}

const RECENT_MAX: usize = 6;

fn push_recent(recent: &mut Vec<RecentMsg>, role: &str, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    recent.push(RecentMsg {
        role: role.to_string(),
        text: text.chars().take(400).collect(),
    });
    if recent.len() > RECENT_MAX {
        recent.remove(0);
    }
}

fn file_mtime(path: &std::path::Path) -> f64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 큰 파일은 head+tail만 읽는다 (codex rollout은 시스템 프롬프트 포함으로 수 MB 가능)
fn read_head_tail(path: &std::path::Path, limit: u64) -> Option<String> {
    use std::io::{Read as _, Seek, SeekFrom};
    let size = fs::metadata(path).ok()?.len();
    if size <= limit {
        return fs::read_to_string(path).ok();
    }
    let mut f = fs::File::open(path).ok()?;
    let half = limit / 2;
    let mut head = vec![0u8; half as usize];
    f.read_exact(&mut head).ok()?;
    f.seek(SeekFrom::End(-(half as i64))).ok()?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).ok()?;
    Some(format!(
        "{}\n{}",
        String::from_utf8_lossy(&head),
        String::from_utf8_lossy(&tail)
    ))
}

/// 세션 메타 캐시 — 20초 폴링마다 전체 jsonl을 재파싱하지 않도록 mtime이 같으면 재사용.
/// 파싱 실패(None)도 캐시해 손상 파일을 매번 다시 읽지 않는다.
static META_CACHE: LazyLock<Mutex<HashMap<String, (f64, Option<SessionMeta>)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cached_meta(path: &PathBuf, parse: fn(&PathBuf) -> Option<SessionMeta>) -> Option<SessionMeta> {
    let mtime = file_mtime(path);
    let key = path.to_string_lossy().to_string();
    if let Some((t, m)) = META_CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        if *t == mtime {
            return m.clone();
        }
    }
    let meta = parse(path);
    META_CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (mtime, meta.clone()));
    meta
}

/// "YYYY-MM-DDTHH:MM:SS.sssZ" (Claude jsonl의 고정 포맷) → epoch seconds.
/// 외부 크레이트 없이 Howard Hinnant의 civil_from_days 역산 공식을 사용.
fn parse_iso_ts(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let mo: i64 = s.get(5..7)?.parse().ok()?;
    let d: i64 = s.get(8..10)?.parse().ok()?;
    let h: i64 = s.get(11..13)?.parse().ok()?;
    let mi: i64 = s.get(14..16)?.parse().ok()?;
    let se: i64 = s.get(17..19)?.parse().ok()?;
    let ms: f64 = s.get(20..23).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);

    let yy = if mo <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;

    Some((days * 86400 + h * 3600 + mi * 60 + se) as f64 + ms / 1000.0)
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
    let mtime = file_mtime(path);
    // 큰 세션 파일(장기 세션)은 codex와 동일하게 head+tail만 읽어 폴링 부하를 낮춘다.
    // first_prompt는 head, last_text/캐시 TTL/summary는 tail에서 나오므로 손실 없음
    // (중간 구간의 message_count만 근사치가 됨).
    let text = read_head_tail(path, 512 * 1024)?;

    let mut meta = SessionMeta {
        session_id: path.file_stem()?.to_string_lossy().to_string(),
        agent: "claude".into(),
        cwd: String::new(),
        summary: None,
        first_prompt: None,
        last_text: None,
        message_count: 0,
        mtime,
        file: path.to_string_lossy().to_string(),
        cache_last_ts: None,
        cache_ttl_secs: None,
        ctx_tokens: None,
        ctx_window: None,
        model: None,
        recent: Vec::new(),
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
            if t == "user" && obj["isMeta"] != true {
                let txt = extract_text(&obj["message"]["content"]);
                let txt = txt.trim();
                if !txt.is_empty() && !txt.starts_with('<') && !txt.starts_with("Caveat:") {
                    if meta.first_prompt.is_none() {
                        meta.first_prompt = Some(txt.chars().take(120).collect());
                    }
                    push_recent(&mut meta.recent, "user", txt);
                }
            }
            if t == "assistant" {
                let txt = extract_text(&obj["message"]["content"]);
                let txt = txt.trim();
                if !txt.is_empty() {
                    meta.last_text = Some(txt.chars().take(1200).collect());
                    push_recent(&mut meta.recent, "assistant", txt);
                }

                // 프롬프트 캐시 TTL 추적: 이 레코드가 캐시를 읽었거나 새로 썼으면
                // 해당 시각부터 TTL이 (재)시작된 것으로 본다. 5분/1시간 중 실제
                // 쓰기가 발생한 티어를 우선하고, 읽기만 있었다면 이전에 관찰된
                // 티어를 유지한다(Anthropic 캐시는 5분 기본, 세션 내 1시간 명시 가능).
                let u = &obj["message"]["usage"];
                let read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                let w1h = u["cache_creation"]["ephemeral_1h_input_tokens"].as_u64().unwrap_or(0);
                let w5m = u["cache_creation"]["ephemeral_5m_input_tokens"].as_u64().unwrap_or(0);
                if read > 0 || w1h > 0 || w5m > 0 {
                    if let Some(ts) = obj["timestamp"].as_str().and_then(parse_iso_ts) {
                        meta.cache_last_ts = Some(ts);
                        if w1h > 0 {
                            meta.cache_ttl_secs = Some(3600);
                        } else if meta.cache_ttl_secs.is_none() {
                            meta.cache_ttl_secs = Some(300); // 5분 쓰기 또는 티어 미관찰(읽기만) 시 기본값
                        }
                    }
                }

                // 사이드바 컨텍스트 게이지용: 마지막 assistant 응답의 컨텍스트 크기
                // (session_usage와 동일한 계산이지만 이미 읽어둔 tail을 재사용해 추가 I/O 없음)
                let ctx = u["input_tokens"].as_u64().unwrap_or(0) + read
                    + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                if ctx > 0 {
                    meta.ctx_tokens = Some(ctx);
                    meta.model = obj["message"]["model"].as_str().map(|s| s.to_string());
                }
            }
        }
    }
    Some(meta)
}

// ---------- Codex 세션 (~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl) ----------

fn read_codex_meta(path: &PathBuf) -> Option<SessionMeta> {
    let text = read_head_tail(path, 512 * 1024)?;
    let mut meta = SessionMeta {
        session_id: String::new(),
        agent: "codex".into(),
        cwd: String::new(),
        summary: None,
        first_prompt: None,
        last_text: None,
        message_count: 0,
        mtime: file_mtime(path),
        file: path.to_string_lossy().to_string(),
        cache_last_ts: None,
        cache_ttl_secs: None,
        ctx_tokens: None,
        ctx_window: None,
        model: None,
        recent: Vec::new(),
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        match obj["type"].as_str().unwrap_or("") {
            "session_meta" => {
                if let Some(id) = obj["payload"]["id"].as_str() {
                    meta.session_id = id.to_string();
                }
                if let Some(c) = obj["payload"]["cwd"].as_str() {
                    meta.cwd = c.to_string();
                }
            }
            "event_msg" => match obj["payload"]["type"].as_str().unwrap_or("") {
                "user_message" => {
                    meta.message_count += 1;
                    if let Some(m) = obj["payload"]["message"].as_str() {
                        let m = m.trim();
                        if !m.is_empty() {
                            if meta.first_prompt.is_none() {
                                meta.first_prompt = Some(m.chars().take(120).collect());
                            }
                            push_recent(&mut meta.recent, "user", m);
                        }
                    }
                }
                "agent_message" => {
                    meta.message_count += 1;
                    if let Some(m) = obj["payload"]["message"].as_str() {
                        let m = m.trim();
                        if !m.is_empty() {
                            meta.last_text = Some(m.chars().take(1200).collect());
                            push_recent(&mut meta.recent, "assistant", m);
                        }
                    }
                }
                "token_count" => {
                    let info = &obj["payload"]["info"];
                    if let Some(tot) = info["total_token_usage"]["total_tokens"].as_u64() {
                        meta.ctx_tokens = Some(tot);
                    }
                    if let Some(w) = info["model_context_window"].as_u64() {
                        meta.ctx_window = Some(w);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    if meta.session_id.is_empty() {
        return None;
    }
    Some(meta)
}

fn scan_codex(out: &mut Vec<SessionMeta>) {
    let Some(home) = dirs::home_dir() else { return };
    let root = home.join(".codex").join("sessions");
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                if let Some(m) = cached_meta(&p, read_codex_meta) {
                    out.push(m);
                }
            }
        }
    }
}

// ---------- Gemini 세션 (~/.gemini/tmp/<proj>/chats/session-*.json) ----------

fn gemini_project_paths(home: &std::path::Path) -> std::collections::HashMap<String, String> {
    // projects.json: { "projects": { "c:\\workspace\\foo": "foo", ... } } — 폴더명 → 실제 경로 역매핑
    let mut map = std::collections::HashMap::new();
    let Ok(text) = fs::read_to_string(home.join(".gemini").join("projects.json")) else { return map };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return map };
    if let Some(obj) = v["projects"].as_object() {
        for (path, name) in obj {
            if let Some(n) = name.as_str() {
                map.insert(n.to_string(), path.clone());
            }
        }
    }
    map
}

/// gemini json 파싱 — cwd에는 프로젝트 폴더명(원시)을 임시로 넣어 두고,
/// 호출부에서 projects.json 매핑을 거쳐 실제 경로로 치환한다
/// (cached_meta가 요구하는 fn(&PathBuf) -> Option<SessionMeta> 시그니처는 캡처를 허용하지 않음).
fn read_gemini_meta(path: &PathBuf) -> Option<SessionMeta> {
    let text = fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let sid = v["sessionId"].as_str()?;
    let msgs = v["messages"].as_array().cloned().unwrap_or_default();
    let first = msgs.iter().find(|m| m["type"] == "user").and_then(|m| {
        m["content"].as_array().and_then(|c| c.iter().find_map(|p| p["text"].as_str()))
    });
    let last = msgs.iter().rev().find(|m| m["type"] != "user").and_then(|m| {
        m["content"].as_array().and_then(|c| c.iter().find_map(|p| p["text"].as_str()))
    });
    let name = path.parent()?.parent()?.file_name()?.to_string_lossy().to_string();

    let mut recent = Vec::new();
    for m in msgs.iter().rev().take(RECENT_MAX) {
        let role = if m["type"] == "user" { "user" } else { "assistant" };
        let txt = m["content"].as_array().and_then(|c| c.iter().find_map(|p| p["text"].as_str())).unwrap_or("");
        push_recent(&mut recent, role, txt);
    }
    recent.reverse();

    Some(SessionMeta {
        session_id: sid.to_string(),
        agent: "gemini".into(),
        cwd: name,
        summary: None,
        first_prompt: first.map(|s| s.trim().chars().take(120).collect()),
        last_text: last.map(|s| s.trim().chars().take(1200).collect()),
        message_count: msgs.len() as u32,
        mtime: file_mtime(path),
        file: path.to_string_lossy().to_string(),
        cache_last_ts: None,
        cache_ttl_secs: None,
        ctx_tokens: None,
        ctx_window: None,
        model: None,
        recent,
    })
}

fn scan_gemini(out: &mut Vec<SessionMeta>) {
    let Some(home) = dirs::home_dir() else { return };
    let proj_map = gemini_project_paths(&home);
    let root = home.join(".gemini").join("tmp");
    let Ok(projects) = fs::read_dir(&root) else { return };
    for proj in projects.flatten() {
        let chats = proj.path().join("chats");
        let Ok(files) = fs::read_dir(&chats) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().map(|x| x == "json").unwrap_or(false) {
                if let Some(mut meta) = cached_meta(&p, read_gemini_meta) {
                    if let Some(real) = proj_map.get(&meta.cwd) {
                        meta.cwd = real.clone();
                    }
                    out.push(meta);
                }
            }
        }
    }
}

#[tauri::command]
fn list_sessions() -> Vec<SessionMeta> {
    let mut out = Vec::new();
    scan_codex(&mut out);
    scan_gemini(&mut out);
    let Some(home) = dirs::home_dir() else { return out };
    let projects = home.join(".claude").join("projects");
    let Ok(dirs_iter) = fs::read_dir(&projects) else { return out };

    for proj in dirs_iter.flatten() {
        let Ok(files) = fs::read_dir(proj.path()) else { continue };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                if let Some(mut meta) = cached_meta(&path, read_meta) {
                    if meta.cwd.is_empty() {
                        // 폴더명(C--workspace-foo)에서 경로 근사 복원 (비ASCII 폴더명 바이트 경계 패닉 방지)
                        let name = proj.file_name().to_string_lossy().to_string();
                        meta.cwd = match (name.get(0..1), name.get(1..3), name.get(3..)) {
                            (Some(d), Some("--"), Some(rest)) if !rest.is_empty() => {
                                format!("{}:\\{}", d, rest.replace('-', "\\"))
                            }
                            _ => name,
                        };
                    }
                    out.push(meta);
                }
            }
        }
    }
    out.sort_by(|a, b| b.mtime.partial_cmp(&a.mtime).unwrap_or(std::cmp::Ordering::Equal));
    out
}

// ---------- 사용량 통계 (세션 jsonl의 usage 레코드 기반 — 프록시 불필요) ----------

#[derive(Serialize)]
struct SessionUsage {
    context_tokens: u64,
    output_tokens: u64,
    model: String,
    window: Option<u64>, // codex는 파일에 컨텍스트 윈도우가 직접 기록됨
}

/// 열린 탭의 컨텍스트 게이지용: 세션 파일의 마지막 usage (claude=assistant usage, codex=token_count)
#[tauri::command]
fn session_usage(file: String) -> Option<SessionUsage> {
    let p = PathBuf::from(&file);
    let text = read_head_tail(&p, 512 * 1024)?;

    // Codex rollout: token_count 이벤트의 total_token_usage + model_context_window
    if file.contains(".codex") {
        let mut last: Option<SessionUsage> = None;
        for line in text.lines() {
            let Ok(o) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
            if o["type"] == "event_msg" && o["payload"]["type"] == "token_count" {
                let info = &o["payload"]["info"];
                if info.is_null() {
                    continue;
                }
                let tot = &info["total_token_usage"];
                last = Some(SessionUsage {
                    context_tokens: tot["total_tokens"].as_u64().unwrap_or(0),
                    output_tokens: tot["output_tokens"].as_u64().unwrap_or(0),
                    model: "codex".into(),
                    window: info["model_context_window"].as_u64(),
                });
            }
        }
        return last;
    }

    let mut last: Option<SessionUsage> = None;
    for line in text.lines() {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
        if obj["type"] != "assistant" {
            continue;
        }
        let u = &obj["message"]["usage"];
        if u.is_null() {
            continue;
        }
        let ctx = u["input_tokens"].as_u64().unwrap_or(0)
            + u["cache_read_input_tokens"].as_u64().unwrap_or(0)
            + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        if ctx == 0 {
            continue;
        }
        last = Some(SessionUsage {
            context_tokens: ctx,
            output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
            model: obj["message"]["model"].as_str().unwrap_or("").to_string(),
            window: None,
        });
    }
    last
}

#[derive(Serialize, Default, Clone)]
struct UsageRow {
    date: String,
    model: String,
    cwd: String,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_5m: u64,
    cache_1h: u64,
    requests: u64,
}

/// 대시보드용: 최근 N일간 (날짜, 모델, 프로젝트)별 토큰 집계
#[tauri::command]
fn usage_stats(days: u32) -> Vec<UsageRow> {
    use std::collections::HashMap;
    let mut map: HashMap<(String, String, String), UsageRow> = HashMap::new();
    let Some(home) = dirs::home_dir() else { return vec![] };
    let projects = home.join(".claude").join("projects");
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(days as u64 * 86400 + 86400);
    let Ok(dirs_iter) = fs::read_dir(&projects) else { return vec![] };

    for proj in dirs_iter.flatten() {
        let Ok(files) = fs::read_dir(proj.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if !p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                continue;
            }
            // 추가 기록은 mtime을 갱신하므로 오래된 파일은 통째로 건너뜀
            if fs::metadata(&p)
                .and_then(|m| m.modified())
                .map(|t| t < cutoff)
                .unwrap_or(true)
            {
                continue;
            }
            let Ok(text) = fs::read_to_string(&p) else { continue };
            let mut cwd = String::new();
            for line in text.lines() {
                let Ok(obj) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
                if cwd.is_empty() {
                    if let Some(c) = obj["cwd"].as_str() {
                        cwd = c.to_string();
                    }
                }
                if obj["type"] != "assistant" {
                    continue;
                }
                let u = &obj["message"]["usage"];
                if u.is_null() {
                    continue;
                }
                let ts = obj["timestamp"].as_str().unwrap_or("");
                if ts.len() < 10 {
                    continue;
                }
                let date = ts[..10].to_string();
                let model = obj["message"]["model"].as_str().unwrap_or("?").to_string();
                let key = (date.clone(), model.clone(), cwd.clone());
                let row = map.entry(key).or_insert_with(|| UsageRow {
                    date,
                    model,
                    cwd: cwd.clone(),
                    ..Default::default()
                });
                row.input += u["input_tokens"].as_u64().unwrap_or(0);
                row.output += u["output_tokens"].as_u64().unwrap_or(0);
                row.cache_read += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                row.cache_5m += u["cache_creation"]["ephemeral_5m_input_tokens"].as_u64().unwrap_or(0);
                row.cache_1h += u["cache_creation"]["ephemeral_1h_input_tokens"].as_u64().unwrap_or(0);
                row.requests += 1;
            }
        }
    }
    map.into_values().collect()
}

// ---------- 요금제 한도 (5시간/주간 사용률 + 리셋 시각) ----------
// 기본: Claude Code OAuth 토큰으로 사용량 API 직접 조회 (headroom 불필요)
// 폴백: headroom이 폴링해둔 subscription_state.json

fn oauth_token() -> Option<String> {
    if let Ok(t) = std::env::var("CLAUDE_CODE_OAUTH_TOKEN") {
        if !t.trim().is_empty() {
            return Some(t.trim().to_string());
        }
    }
    let base = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".claude"));
    let creds: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(base.join(".credentials.json")).ok()?).ok()?;
    let oauth = &creds["claudeAiOauth"];
    let token = oauth["accessToken"].as_str()?.to_string();
    // 만료 확인 (ms 단위)
    if let Some(exp) = oauth["expiresAt"].as_f64() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis() as f64;
        if now_ms >= exp - 60_000.0 {
            return None;
        }
    }
    Some(token)
}

fn fetch_usage_direct() -> Option<serde_json::Value> {
    let token = oauth_token()?;
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {}", token))
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().ok()?;
    let map_win = |w: &serde_json::Value| {
        serde_json::json!({
            "utilization_pct": w["utilization"],
            "resets_at": w["resets_at"],
        })
    };
    Some(serde_json::json!({
        "source": "direct",
        "five_hour": map_win(&v["five_hour"]),
        "seven_day": map_win(&v["seven_day"]),
        "limits": v["limits"],
        "polled_at": chrono_now_iso(),
    }))
}

fn chrono_now_iso() -> String {
    // 의존성 없이 대략적인 ISO 시각 (frontend는 상대시간 계산에 resets_at만 사용)
    let secs = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("@{}", secs)
}

fn usage_from_headroom() -> Option<serde_json::Value> {
    let p = dirs::home_dir()?.join(".headroom").join("subscription_state.json");
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(p).ok()?).ok()?;
    if v["latest"].is_null() {
        return None;
    }
    let mut latest = v["latest"].clone();
    latest["source"] = serde_json::json!("headroom");
    Some(latest)
}

// ---------- Codex 상태 (rollout 파일의 token_count 이벤트에서 로컬로 추출) ----------

fn codex_rollouts_by_mtime() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else { return vec![] };
    let mut files: Vec<(f64, PathBuf)> = Vec::new();
    let mut stack = vec![home.join(".codex").join("sessions")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                files.push((file_mtime(&p), p));
            }
        }
    }
    files.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    files.into_iter().map(|(_, p)| p).collect()
}

/// 가장 최근 codex 세션의 마지막 token_count 이벤트에서 rate limit 추출
#[tauri::command]
fn codex_state() -> Option<serde_json::Value> {
    for p in codex_rollouts_by_mtime().into_iter().take(3) {
        let Some(text) = read_head_tail(&p, 256 * 1024) else { continue };
        let mut last: Option<(String, serde_json::Value)> = None;
        for line in text.lines() {
            let Ok(o) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
            if o["type"] == "event_msg" && o["payload"]["type"] == "token_count" {
                let rl = &o["payload"]["rate_limits"];
                // primary가 채워진 이벤트만 유효 (간헐적으로 null로 기록됨)
                if !rl.is_null() && !rl["primary"].is_null() {
                    last = Some((
                        o["timestamp"].as_str().unwrap_or("").to_string(),
                        rl.clone(),
                    ));
                }
            }
        }
        if let Some((ts, rl)) = last {
            return Some(serde_json::json!({ "rate_limits": rl, "polled_at": ts }));
        }
    }
    None
}

/// 3분 캐시 — 프런트가 자주 불러도 API를 과도하게 치지 않음 (이 엔드포인트는
/// 짧은 간격으로 두드리면 429가 나기 쉬움).
static USAGE_CACHE: Mutex<Option<(std::time::Instant, serde_json::Value)>> = Mutex::new(None);

#[tauri::command]
fn subscription_state(force: bool) -> Option<serde_json::Value> {
    if !force {
        let cache = USAGE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, v)) = cache.as_ref() {
            if t.elapsed().as_secs() < 180 {
                return Some(v.clone());
            }
        }
    }
    if let Some(direct) = fetch_usage_direct() {
        *USAGE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((std::time::Instant::now(), direct.clone()));
        return Some(direct);
    }
    // direct 호출 실패(429 등) 시, headroom의 오래됐을 수 있는 파일보다는
    // 직전에 성공했던 direct 응답(캐시 TTL을 넘겼더라도)을 우선한다 —
    // headroom 프로세스가 꺼져 있으면 그 파일이 며칠씩 묵어 있을 수 있음.
    {
        let cache = USAGE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, v)) = cache.as_ref() {
            return Some(v.clone());
        }
    }
    usage_from_headroom()
}

/// headroom이 설치되어 있으면 절감 통계 반환 (없으면 None — 대시보드에서 섹션 생략)
#[tauri::command]
fn headroom_stats() -> Option<serde_json::Value> {
    let p = dirs::home_dir()?.join(".headroom").join("proxy_savings.json");
    let text = fs::read_to_string(p).ok()?;
    serde_json::from_str(&text).ok()
}

/// 세션 프로젝트 폴더를 탐색기로 연다
#[tauri::command]
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

#[tauri::command]
fn delete_session(file: String) -> Result<(), String> {
    let p = PathBuf::from(&file);
    let home = dirs::home_dir().ok_or("no home dir")?;
    // 알려진 세션 저장소 안의 세션 파일만 삭제 허용
    let allowed = [
        home.join(".claude").join("projects"),
        home.join(".codex").join("sessions"),
        home.join(".gemini").join("tmp"),
    ];
    let in_store = allowed.iter().any(|root| p.starts_with(root));
    let is_session = p
        .extension()
        .map(|e| e == "jsonl" || e == "json")
        .unwrap_or(false);
    if !in_store || !is_session {
        return Err("invalid session file path".into());
    }
    fs::remove_file(&p).map_err(|e| e.to_string())?;
    META_CACHE.lock().unwrap_or_else(|e| e.into_inner()).remove(&p.to_string_lossy().to_string());
    Ok(())
}

fn main() {
    install_panic_hook();
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
            spawn_pty, write_pty, resize_pty, kill_pty, list_sessions, delete_session,
            session_usage, usage_stats, headroom_stats, subscription_state, codex_state,
            open_path
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
                    let _ = p.child.kill();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running cli-deck");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_ts_matches_known_epoch() {
        let got = parse_iso_ts("2026-07-04T17:22:51.651Z").unwrap();
        assert!((got - 1783185771.651).abs() < 0.001);
    }
}
