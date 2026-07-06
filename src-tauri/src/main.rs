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
    agent: String, // "claude" | "codex" | "gemini"
    cwd: String,
    summary: Option<String>,
    first_prompt: Option<String>,
    last_text: Option<String>,
    message_count: u32,
    mtime: f64,
    file: String,
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
        agent: "claude".into(),
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
                    if meta.first_prompt.is_none() {
                        if let Some(m) = obj["payload"]["message"].as_str() {
                            let m = m.trim();
                            if !m.is_empty() {
                                meta.first_prompt = Some(m.chars().take(120).collect());
                            }
                        }
                    }
                }
                "agent_message" => {
                    meta.message_count += 1;
                    if let Some(m) = obj["payload"]["message"].as_str() {
                        let m = m.trim();
                        if !m.is_empty() {
                            meta.last_text = Some(m.chars().take(300).collect());
                        }
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
                if let Some(m) = read_codex_meta(&p) {
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

fn scan_gemini(out: &mut Vec<SessionMeta>) {
    let Some(home) = dirs::home_dir() else { return };
    let proj_map = gemini_project_paths(&home);
    let root = home.join(".gemini").join("tmp");
    let Ok(projects) = fs::read_dir(&root) else { return };
    for proj in projects.flatten() {
        let name = proj.file_name().to_string_lossy().to_string();
        let chats = proj.path().join("chats");
        let Ok(files) = fs::read_dir(&chats) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().map(|x| x == "json").unwrap_or(false) {
                let Ok(text) = fs::read_to_string(&p) else { continue };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
                let Some(sid) = v["sessionId"].as_str() else { continue };
                let msgs = v["messages"].as_array().cloned().unwrap_or_default();
                let first = msgs.iter().find(|m| m["type"] == "user").and_then(|m| {
                    m["content"].as_array().and_then(|c| c.iter().find_map(|p| p["text"].as_str()))
                });
                let last = msgs.iter().rev().find(|m| m["type"] != "user").and_then(|m| {
                    m["content"].as_array().and_then(|c| c.iter().find_map(|p| p["text"].as_str()))
                });
                out.push(SessionMeta {
                    session_id: sid.to_string(),
                    agent: "gemini".into(),
                    cwd: proj_map.get(&name).cloned().unwrap_or_else(|| name.clone()),
                    summary: None,
                    first_prompt: first.map(|s| s.trim().chars().take(120).collect()),
                    last_text: last.map(|s| s.trim().chars().take(300).collect()),
                    message_count: msgs.len() as u32,
                    mtime: file_mtime(&p),
                    file: p.to_string_lossy().to_string(),
                });
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

/// 60초 캐시 — 프런트가 자주 불러도 API를 과도하게 치지 않음
static USAGE_CACHE: Mutex<Option<(std::time::Instant, serde_json::Value)>> = Mutex::new(None);

#[tauri::command]
fn subscription_state() -> Option<serde_json::Value> {
    {
        let cache = USAGE_CACHE.lock().unwrap();
        if let Some((t, v)) = cache.as_ref() {
            if t.elapsed().as_secs() < 60 {
                return Some(v.clone());
            }
        }
    }
    let result = fetch_usage_direct().or_else(usage_from_headroom)?;
    *USAGE_CACHE.lock().unwrap() = Some((std::time::Instant::now(), result.clone()));
    Some(result)
}

/// headroom이 설치되어 있으면 절감 통계 반환 (없으면 None — 대시보드에서 섹션 생략)
#[tauri::command]
fn headroom_stats() -> Option<serde_json::Value> {
    let p = dirs::home_dir()?.join(".headroom").join("proxy_savings.json");
    let text = fs::read_to_string(p).ok()?;
    serde_json::from_str(&text).ok()
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
    fs::remove_file(&p).map_err(|e| e.to_string())
}

fn main() {
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
            session_usage, usage_stats, headroom_stats, subscription_state, codex_state
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
            // X 버튼 = 트레이로 (세션 유지). 완전 종료는 트레이 메뉴의 "종료"
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running claude-deck");
}
