// 세션 스캔 (Claude / Codex / Gemini) + 백그라운드 잡

use super::*;

// ---------- 세션 스캔 ----------

#[derive(Serialize, Clone, Default)]
pub(crate) struct SessionMeta {
    pub(crate) session_id: String,
    pub(crate) agent: String, // "claude" | "codex" | "gemini"
    pub(crate) cwd: String,
    pub(crate) summary: Option<String>,
    pub(crate) first_prompt: Option<String>,
    /// recent와 마찬가지로 미리보기 전용 — 목록 응답에서는 제외한다
    #[serde(skip_serializing)]
    pub(crate) last_text: Option<String>,
    pub(crate) message_count: u32,
    pub(crate) mtime: f64,
    pub(crate) file: String,
    /// 마지막으로 프롬프트 캐시가 읽히거나 새로 쓰인 시각 (epoch seconds)
    pub(crate) cache_last_ts: Option<f64>,
    /// 해당 캐시 항목의 TTL (초) — 5분(300) 또는 1시간(3600)
    pub(crate) cache_ttl_secs: Option<u32>,
    /// 마지막 assistant 응답 시점의 컨텍스트 토큰 수 (사이드바 게이지용)
    pub(crate) ctx_tokens: Option<u64>,
    /// codex는 파일에 컨텍스트 윈도우가 직접 기록됨 (claude는 프런트에서 모델명으로 추정)
    pub(crate) ctx_window: Option<u64>,
    /// 마지막으로 관측된 모델명
    pub(crate) model: Option<String>,
    /// 백그라운드 에이전트 상태 (working/blocked/failed) — 아니면 None
    pub(crate) bg_state: Option<String>,
    /// 백그라운드 에이전트가 지금 뭘 하고 있는지 한 줄
    pub(crate) bg_detail: Option<String>,
    /// 데몬 로스터에 살아 있는가 (죽은 bg 세션과 구분)
    pub(crate) bg_running: bool,
    /// 이 세션이 포크돼 나온 원본 세션 ID. 로스터(실행 중)보다 jobs/<short>/state.json이
    /// 우선 — 워커가 거둬진 뒤에도 남아 있어서 계보가 사라지지 않는다.
    pub(crate) parent_id: Option<String>,
    /// Claude Code가 세션에 붙인 이름 (agent-name/ai-title 레코드). 포크·복제본이면
    /// "⑂", "(2)" 같은 표식이 이미 붙어 있어 요약보다 구분이 잘 된다.
    pub(crate) title: Option<String>,
    /// 백그라운드 잡의 8자리 short id — `claude attach <short>`에 쓴다
    pub(crate) bg_short: Option<String>,
    /// 호버 미리보기용 최근 대화 (최대 3턴 = 6개). 오래된 것부터 순서대로.
    /// 목록에는 싣지 않는다 — 세션 수 × 3KB가 20초마다 IPC로 넘어가는데
    /// 프런트는 호버할 때만 쓰므로 session_preview로 그때 가져간다.
    #[serde(skip_serializing)]
    pub(crate) recent: Vec<RecentMsg>,
}

impl SessionMeta {
    /// 파서 공통 뼈대. 파일에서 아직 아무것도 읽지 않은 상태 — 나머지 필드는 기본값.
    fn new(session_id: impl Into<String>, agent: &str, path: &std::path::Path) -> Self {
        SessionMeta {
            session_id: session_id.into(),
            agent: agent.into(),
            mtime: file_mtime(path),
            file: path.to_string_lossy().to_string(),
            ..Default::default()
        }
    }
}

/// 호버 미리보기 전용 페이로드 (목록에서 제외한 무거운 필드만)
#[derive(Serialize)]
pub(crate) struct SessionPreview {
    pub(crate) last_text: Option<String>,
    pub(crate) recent: Vec<RecentMsg>,
}

/// 사이드바 목록용 경량 사본 — 미리보기 전용 필드(last_text, recent)는 비운다.
pub(crate) fn light_meta(m: &SessionMeta) -> SessionMeta {
    SessionMeta { last_text: None, recent: Vec::new(), ..m.clone() }
}

/// 파일 경로로 파서를 고른다 (claude/codex/gemini 저장소 구조가 서로 다름)
pub(crate) fn parser_for(path: &std::path::Path) -> fn(&PathBuf) -> Option<SessionMeta> {
    let s = path.to_string_lossy();
    if s.contains(".codex") {
        read_codex_meta
    } else if s.contains(".gemini") {
        read_gemini_meta
    } else {
        read_meta
    }
}

/// 호버 시점에만 호출 — 대개 목록 스캔이 이미 채워둔 캐시에서 바로 나온다.
#[tauri::command(async)]
pub(crate) fn session_preview(file: String) -> Option<SessionPreview> {
    let p = PathBuf::from(&file);
    let m = cached_meta(&p, parser_for(&p))?;
    Some(SessionPreview { last_text: m.last_text, recent: m.recent })
}

#[derive(Serialize, Clone)]
pub(crate) struct RecentMsg {
    pub(crate) role: String, // "user" | "assistant"
    pub(crate) text: String,
}

pub(crate) const RECENT_MAX: usize = 6;

pub(crate) fn push_recent(recent: &mut Vec<RecentMsg>, role: &str, text: &str) {
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

pub(crate) fn file_mtime(path: &std::path::Path) -> f64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 큰 파일은 head+tail만 읽는다 (codex rollout은 시스템 프롬프트 포함으로 수 MB 가능)
pub(crate) fn read_head_tail(path: &std::path::Path, limit: u64) -> Option<String> {
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
pub(crate) static META_CACHE: LazyLock<Mutex<HashMap<String, (f64, Option<SessionMeta>)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn cached_meta_with(
    path: &PathBuf,
    parse: fn(&PathBuf) -> Option<SessionMeta>,
    pick: fn(&SessionMeta) -> SessionMeta,
) -> Option<SessionMeta> {
    let mtime = file_mtime(path);
    let key = path.to_string_lossy().to_string();
    if let Some((t, m)) = META_CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        if *t == mtime {
            return m.as_ref().map(pick);
        }
    }
    let meta = parse(path);
    let picked = meta.as_ref().map(pick);
    META_CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (mtime, meta));
    picked
}

pub(crate) fn cached_meta(path: &PathBuf, parse: fn(&PathBuf) -> Option<SessionMeta>) -> Option<SessionMeta> {
    cached_meta_with(path, parse, |m| m.clone())
}

/// 목록 스캔용 — 캐시에서 경량 필드만 복사한다
pub(crate) fn cached_meta_light(path: &PathBuf, parse: fn(&PathBuf) -> Option<SessionMeta>) -> Option<SessionMeta> {
    cached_meta_with(path, parse, light_meta)
}

/// 사라진 세션 파일의 캐시 항목 정리 (없으면 무한히 쌓임)
pub(crate) fn evict_stale_cache() {
    META_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|k, _| std::path::Path::new(k).exists());
}

/// "YYYY-MM-DDTHH:MM:SS.sssZ" (Claude jsonl의 고정 포맷) → epoch seconds.
/// 외부 크레이트 없이 Howard Hinnant의 civil_from_days 역산 공식을 사용.
pub(crate) fn parse_iso_ts(s: &str) -> Option<f64> {
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

pub(crate) fn extract_text(content: &serde_json::Value) -> String {
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

pub(crate) fn read_meta(path: &PathBuf) -> Option<SessionMeta> {
    // 큰 세션 파일(장기 세션)은 codex와 동일하게 head+tail만 읽어 폴링 부하를 낮춘다.
    // first_prompt는 head, last_text/캐시 TTL/summary는 tail에서 나오므로 손실 없음
    // (중간 구간의 message_count만 근사치가 됨).
    let text = read_head_tail(path, 512 * 1024)?;

    let mut meta = SessionMeta::new(path.file_stem()?.to_string_lossy(), "claude", path);

    let mut named = false; // agent-name을 봤으면 ai-title은 무시
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
        // 세션 이름: agent-name(사용자/피어가 정한 이름, 중복이면 "(2)")이 ai-title보다 우선.
        // 둘 다 뒤에 나온 레코드가 최신이다.
        if obj["type"] == "agent-name" {
            if let Some(s) = obj["agentName"].as_str() {
                let s = s.trim();
                if !s.is_empty() {
                    meta.title = Some(s.to_string());
                    named = true;
                }
            }
        } else if obj["type"] == "ai-title" && !named {
            if let Some(s) = obj["aiTitle"].as_str() {
                let s = s.trim();
                if !s.is_empty() {
                    meta.title = Some(s.to_string());
                }
            }
        }
        let t = obj["type"].as_str().unwrap_or("");
        if t == "user" || t == "assistant" {
            meta.message_count += 1;
            if t == "user" && obj["isMeta"] != true {
                let txt = extract_text(&obj["message"]["content"]);
                let txt = txt.trim();
                // "[Request interrupted by user]"는 사용자가 친 프롬프트가 아니라
                // 중단 마커라서 제목/미리보기에 뜨면 안 된다
                if !txt.is_empty()
                    && !txt.starts_with('<')
                    && !txt.starts_with("Caveat:")
                    && !txt.starts_with("[Request interrupted")
                {
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
                // (이미 읽어둔 tail을 재사용하므로 추가 I/O 없음)
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

pub(crate) fn read_codex_meta(path: &PathBuf) -> Option<SessionMeta> {
    let text = read_head_tail(path, 512 * 1024)?;
    let mut meta = SessionMeta::new("", "codex", path);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        match obj["type"].as_str().unwrap_or("") {
            // 모델은 rollout 안에 그대로 있다. 상태 DB를 따로 열 필요가 없다.
            // 세션 중간에 모델을 바꿀 수 있으므로 마지막 값이 이긴다.
            "turn_context" => {
                if let Some(m) = obj["payload"]["model"].as_str() {
                    let eff = obj["payload"]["reasoning_effort"].as_str().unwrap_or("");
                    meta.model = Some(if eff.is_empty() {
                        m.to_string()
                    } else {
                        format!("{m} ({eff})")
                    });
                }
            }
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
                    // 컨텍스트 게이지는 "지금 창을 얼마나 차지했나"이므로 마지막 요청의
                    // 입력 토큰을 쓴다. total_token_usage는 세션 전체 누적이라 창 크기를
                    // 훌쩍 넘는다 (실측: 25.8만 창에 누적 1215만 → 게이지 4702%).
                    if let Some(used) = info["last_token_usage"]["input_tokens"].as_u64() {
                        if used > 0 {
                            meta.ctx_tokens = Some(used);
                        }
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

pub(crate) fn scan_codex(out: &mut Vec<SessionMeta>) {
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
                if let Some(m) = cached_meta_light(&p, read_codex_meta) {
                    out.push(m);
                }
            }
        }
    }
}

// ---------- Gemini 세션 (~/.gemini/tmp/<proj>/chats/session-*.json) ----------

pub(crate) fn gemini_project_paths(home: &std::path::Path) -> std::collections::HashMap<String, String> {
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
pub(crate) fn read_gemini_meta(path: &PathBuf) -> Option<SessionMeta> {
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
        cwd: name,
        first_prompt: first.map(|s| s.trim().chars().take(120).collect()),
        last_text: last.map(|s| s.trim().chars().take(1200).collect()),
        message_count: msgs.len() as u32,
        recent,
        ..SessionMeta::new(sid, "gemini", path)
    })
}

pub(crate) fn scan_gemini(out: &mut Vec<SessionMeta>) {
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
                if let Some(mut meta) = cached_meta_light(&p, read_gemini_meta) {
                    if let Some(real) = proj_map.get(&meta.cwd) {
                        meta.cwd = real.clone();
                    }
                    out.push(meta);
                }
            }
        }
    }
}

/// Claude Code 백그라운드 에이전트 정보. `~/.claude/jobs/<8자리>/state.json` 규칙에 따라
/// 세션 ID 앞 8자리를 키로 쓴다.
pub(crate) struct BgInfo {
    pub(crate) state: String,
    pub(crate) detail: String,
    pub(crate) running: bool,
    pub(crate) parent_id: Option<String>,
    /// state.json의 name — jsonl에 이름 레코드가 없을 때의 폴백
    pub(crate) name: Option<String>,
}

pub(crate) fn scan_bg_jobs() -> HashMap<String, BgInfo> {
    let mut out: HashMap<String, BgInfo> = HashMap::new();
    let Some(home) = dirs::home_dir() else { return out };

    // 살아 있는 워커만 로스터에 남는다. 포크 출처(원본 세션)도 여기서만 알 수 있다.
    // launch.sessionId는 fork=true일 때만 원본이다. 데몬이 자기 세션을 다시 띄운
    // 경우(resume, fork 없음)에도 같은 필드에 자기 ID가 들어 있어서, 구분 없이 쓰면
    // 세션이 자기 자신의 자식이 되어 목록에서 사라진다.
    let mut running: HashMap<String, Option<String>> = HashMap::new();
    if let Ok(text) = fs::read_to_string(home.join(".claude").join("daemon").join("roster.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(ws) = v["workers"].as_object() {
                for (short, w) in ws {
                    let launch = &w["dispatch"]["launch"];
                    let parent = if launch["fork"] == true {
                        launch["sessionId"].as_str().and_then(|p| {
                            std::path::Path::new(p)
                                .file_stem()
                                .map(|s| s.to_string_lossy().to_string())
                        })
                    } else {
                        None
                    };
                    running.insert(short.clone(), parent);
                }
            }
        }
    }

    let Ok(entries) = fs::read_dir(home.join(".claude").join("jobs")) else { return out };
    for e in entries.flatten() {
        let short = e.file_name().to_string_lossy().to_string();
        let Ok(text) = fs::read_to_string(e.path().join("state.json")) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let run = running.get(&short);
        // 원본 세션은 state.json이 먼저다 — 워커가 거둬져 로스터에서 빠져도 남는다.
        let own = v["sessionId"].as_str().unwrap_or("");
        let parent_id = v["forkParentSessionId"]
            .as_str()
            .map(|p| p.to_string())
            .or_else(|| run.and_then(|p| p.clone()))
            .filter(|p| !p.is_empty() && p != own);
        out.insert(
            short,
            BgInfo {
                state: v["state"].as_str().unwrap_or("unknown").to_string(),
                detail: v["detail"].as_str().unwrap_or("").to_string(),
                running: run.is_some(),
                parent_id,
                name: v["name"].as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            },
        );
    }
    out
}


#[tauri::command(async)]
pub(crate) fn list_sessions() -> Vec<SessionMeta> {
    evict_stale_cache();
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
                if let Some(mut meta) = cached_meta_light(&path, read_meta) {
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
    // 상태줄이 실제 컨텍스트 윈도우를 알려준다 — 모델명으로 추측하던 걸 대체한다
    for m in out.iter_mut() {
        let Some(v) = read_status(&m.session_id) else { continue };
        if let Some(size) = v["context_window"]["context_window_size"].as_u64() {
            m.ctx_window = Some(size);
        }
        if let Some(used) = v["context_window"]["total_input_tokens"].as_u64() {
            if used > 0 {
                m.ctx_tokens = Some(used);
            }
        }
    }

    // 백그라운드 에이전트 상태 붙이기 (세션 ID 앞 8자리로 매칭)
    let bg = scan_bg_jobs();
    for m in out.iter_mut() {
        let Some(short) = m.session_id.get(..8) else { continue };
        let Some(info) = bg.get(short) else { continue };
        m.bg_state = Some(info.state.clone());
        m.bg_running = info.running;
        m.bg_short = Some(short.to_string());
        if !info.detail.is_empty() {
            m.bg_detail = Some(info.detail.clone());
        }
        m.parent_id = info.parent_id.clone();
        if m.title.is_none() {
            m.title = info.name.clone();
        }
    }


    out.sort_by(|a, b| b.mtime.partial_cmp(&a.mtime).unwrap_or(std::cmp::Ordering::Equal));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 실제 홈 디렉터리를 훑어 폴링 한 번의 비용을 잰다 (`cargo test -- --ignored scan_cost --nocapture`)
    #[test]
    #[ignore]
    fn scan_cost() {
        for round in 0..3 {
            let t0 = std::time::Instant::now();
            let v = list_sessions();
            eprintln!("round {round}: {} sessions in {:?}", v.len(), t0.elapsed());
        }
    }

    #[test]
    fn iso_ts_matches_known_epoch() {
        let got = parse_iso_ts("2026-07-04T17:22:51.651Z").unwrap();
        assert!((got - 1783185771.651).abs() < 0.001);
    }
}

#[cfg(test)]
mod codex_dump {
    /// 실제 rollout에서 모델이 뽑히는지 눈으로 확인 — `cargo test -- --ignored codex_model`
    #[test]
    #[ignore]
    fn codex_model() {
        let home = dirs::home_dir().unwrap();
        let root = home.join(".codex").join("sessions");
        let mut files = Vec::new();
        let mut stack = vec![root];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                    files.push(p);
                }
            }
        }
        files.sort();
        let mut with = 0;
        for p in &files {
            if let Some(m) = super::read_codex_meta(p) {
                if m.model.is_some() {
                    with += 1;
                }
                eprintln!("{:<10} {}", m.model.unwrap_or_else(|| "-".into()), m.session_id);
            }
        }
        eprintln!("{with}/{} rollouts carry a model", files.len());
    }
}

#[cfg(test)]
mod codex_ctx_tests {
    /// 컨텍스트 게이지는 창 점유율이다. codex의 total_token_usage는 세션 누적이라
    /// 그대로 쓰면 게이지가 100%를 한참 넘는다 (실측 4702%).
    #[test]
    fn codex_ctx_uses_the_last_request_not_the_running_total() {
        let dir = std::env::temp_dir().join(format!("deck-cctx-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("rollout.jsonl");
        let text = [
            r#"{"type":"session_meta","payload":{"id":"s1","cwd":"C:/w"}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"model_context_window":258400,"total_token_usage":{"total_tokens":12150416},"last_token_usage":{"input_tokens":138146}}}}"#,
        ]
        .join("\n");
        std::fs::write(&f, text).unwrap();
        let m = super::read_codex_meta(&f).expect("meta");
        assert_eq!(m.ctx_window, Some(258400));
        assert_eq!(m.ctx_tokens, Some(138146), "누적이 아니라 마지막 요청의 입력");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
