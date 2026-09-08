// 사용량 통계와 요금제 한도

use super::*;

// ---------- 사용량 통계 (세션 jsonl의 usage 레코드 기반 — 프록시 불필요) ----------

// 열린 탭의 컨텍스트 게이지는 list_sessions가 이미 내려주는 ctx_tokens/ctx_window로
// 프런트에서 계산한다 (예전 session_usage 커맨드는 같은 계산을 위해 탭마다 8초 주기로
// 세션 파일을 다시 읽고 있었다).

#[derive(Serialize, Default, Clone)]
pub(crate) struct UsageRow {
    pub(crate) date: String,
    pub(crate) model: String,
    pub(crate) cwd: String,
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_5m: u64,
    pub(crate) cache_1h: u64,
    pub(crate) requests: u64,
}

pub(crate) type UsageKey = (String, String, String); // (날짜, 모델, 프로젝트)

/// 세션 파일 하나를 (날짜, 모델, 프로젝트)별로 집계
pub(crate) fn usage_rows_of_file(path: &PathBuf) -> Vec<UsageRow> {
    let Ok(text) = fs::read_to_string(path) else { return vec![] };
    let mut map: HashMap<UsageKey, UsageRow> = HashMap::new();
    let mut cwd = String::new();
    // 한 응답이 텍스트 블록과 도구 호출 블록으로 나뉘면 Claude Code는 같은 message.id와
    // 같은 usage를 가진 assistant 줄을 여러 개 쓴다. 줄마다 더하면 같은 토큰을 여러 번
    // 세게 된다 (실측: 한 세션에서 비용 80% 과다). message.id로 한 번만 센다.
    let mut counted: std::collections::HashSet<String> = std::collections::HashSet::new();
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
        if let Some(id) = obj["message"]["id"].as_str() {
            if !counted.insert(id.to_string()) {
                continue;
            }
        }
        let ts = obj["timestamp"].as_str().unwrap_or("");
        if ts.len() < 10 {
            continue;
        }
        let date = ts[..10].to_string();
        let model = obj["message"]["model"].as_str().unwrap_or("?").to_string();
        let row = map
            .entry((date.clone(), model.clone(), cwd.clone()))
            .or_insert_with(|| UsageRow { date, model, cwd: cwd.clone(), ..Default::default() });
        row.input += u["input_tokens"].as_u64().unwrap_or(0);
        row.output += u["output_tokens"].as_u64().unwrap_or(0);
        row.cache_read += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        row.cache_5m += u["cache_creation"]["ephemeral_5m_input_tokens"].as_u64().unwrap_or(0);
        row.cache_1h += u["cache_creation"]["ephemeral_1h_input_tokens"].as_u64().unwrap_or(0);
        row.requests += 1;
    }
    map.into_values().collect()
}

/// 세션 하나의 턴별 토큰. 대시보드는 날짜·모델로 합쳐 버리지만, 여기서는 응답 하나가
/// 한 줄이다 — 캐시가 끊긴 지점(cache_read가 0으로 떨어지고 쓰기가 튀는 턴)을 찾기 위한 것.
#[derive(Serialize, Default, Clone)]
pub(crate) struct TurnRow {
    /// epoch seconds
    pub(crate) ts: f64,
    pub(crate) model: String,
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_5m: u64,
    pub(crate) cache_1h: u64,
    /// 이 응답을 낳은 사용자 프롬프트 (도구 호출 루프에서는 여러 턴이 같은 값을 공유한다)
    pub(crate) prompt: String,
    /// 같은 프롬프트에 속한 턴을 묶기 위한 번호
    pub(crate) prompt_idx: u32,
    /// 이 응답이 실제로 쓴 말 (앞부분만). 도구만 부른 턴은 비어 있다.
    pub(crate) text: String,
    /// 이 응답이 부른 도구들 — "Read src/usage.rs" 형태
    pub(crate) tools: Vec<String>,
}

/// assistant content 배열에서 사람이 읽을 것만 뽑는다.
/// thinking 블록은 화면에 낼 것이 아니므로 버리고, tool_use는 이름과 핵심 인자만 남긴다.
/// 도구 호출 옆에 붙일 한 조각. 인자 이름이 도구마다 달라서 순서에 기대면
/// (serde_json의 Map은 알파벳순이다) Write가 file_path 대신 content를 보여주는 식이 된다.
/// 사람이 보고 싶은 인자를 도구별로 골라 준다.
fn tool_arg<'a>(name: &str, input: &'a serde_json::Value) -> &'a str {
    // Bash는 명령줄보다 description이 훨씬 읽기 쉽다 (Claude Code 자신도 그걸 보여준다)
    let keys: &[&str] = if name == "Bash" {
        &["description", "command"]
    } else {
        &["file_path", "pattern", "path", "query", "url", "prompt", "command", "description"]
    };
    for k in keys {
        if let Some(v) = input[*k].as_str() {
            if !v.is_empty() {
                return v;
            }
        }
    }
    input
        .as_object()
        .and_then(|o| o.values().find_map(|v| v.as_str()))
        .unwrap_or("")
}

fn assistant_blocks(content: &serde_json::Value) -> (String, Vec<String>) {
    let mut text = String::new();
    let mut tools = Vec::new();
    match content {
        serde_json::Value::String(s) => text.push_str(s),
        serde_json::Value::Array(parts) => {
            for p in parts {
                match p["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = p["text"].as_str() {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                    }
                    Some("tool_use") => {
                        let name = p["name"].as_str().unwrap_or("tool");
                        let arg = tool_arg(name, &p["input"]);
                        let arg: String = arg.split('\n').next().unwrap_or("").chars().take(48).collect();
                        tools.push(if arg.is_empty() {
                            name.to_string()
                        } else {
                            format!("{name} {arg}")
                        });
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    (text, tools)
}

/// 세션 파일의 모든 assistant 응답을 시간순으로. 알려진 세션 저장소 안의 파일만 읽는다.
#[tauri::command]
pub(crate) fn session_turns(file: String) -> Result<Vec<TurnRow>, String> {
    let path = session_file_in_store(&file)?;
    let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    Ok(turns_from_text(&text))
}

/// session_turns의 본체 — 저장소 경로 검사 없이 텍스트만 파싱해 테스트할 수 있게 분리
pub(crate) fn turns_from_text(text: &str) -> Vec<TurnRow> {
    let mut out: Vec<TurnRow> = Vec::new();
    let mut prompt = String::new();
    let mut prompt_idx: u32 = 0;
    // usage_rows_of_file과 같은 이유로 message.id 기준 중복 제거 (한 응답 = 한 줄이 아니다)
    let mut counted: std::collections::HashSet<String> = std::collections::HashSet::new();
    // 방금 만든 턴의 id — 뒷줄을 합칠 때 위치가 아니라 id로 확인한다
    let mut last_id = String::new();
    for line in text.lines() {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
        // 사람이 실제로 친 프롬프트만 센다 — 도구 결과와 시스템 주입은 제외
        // (read_meta의 first_prompt 판정과 같은 규칙).
        if obj["type"] == "user" && obj["isMeta"] != true {
            let txt = extract_text(&obj["message"]["content"]);
            let txt = txt.trim();
            if !txt.is_empty()
                && !txt.starts_with('<')
                && !txt.starts_with("Caveat:")
                && !txt.starts_with("[Request interrupted")
            {
                prompt = txt.chars().take(300).collect();
                prompt_idx += 1;
            }
            continue;
        }
        if obj["type"] != "assistant" {
            continue;
        }
        let u = &obj["message"]["usage"];
        if u.is_null() {
            continue;
        }
        let (text, tools) = assistant_blocks(&obj["message"]["content"]);
        // 같은 message.id의 뒷줄들은 같은 응답의 나머지 블록이다. 사용량은 이미 셌으니
        // 더하지 말고 내용만 앞 턴에 합친다.
        if let Some(id) = obj["message"]["id"].as_str() {
            if !counted.insert(id.to_string()) {
                if id != last_id {
                    continue; // 사이에 다른 응답이 끼어든 경우 — 엉뚱한 턴에 붙이지 않는다
                }
                if let Some(prev) = out.last_mut() {
                    if !text.is_empty() && prev.text.chars().count() < 400 {
                        if !prev.text.is_empty() {
                            prev.text.push('\n');
                        }
                        prev.text.push_str(&text);
                        prev.text = prev.text.chars().take(400).collect();
                    }
                    prev.tools.extend(tools);
                }
                continue;
            }
        }
        let Some(ts) = obj["timestamp"].as_str().and_then(parse_iso_ts) else { continue };
        last_id = obj["message"]["id"].as_str().unwrap_or("").to_string();
        out.push(TurnRow {
            ts,
            model: obj["message"]["model"].as_str().unwrap_or("?").to_string(),
            input: u["input_tokens"].as_u64().unwrap_or(0),
            output: u["output_tokens"].as_u64().unwrap_or(0),
            cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
            cache_5m: u["cache_creation"]["ephemeral_5m_input_tokens"].as_u64().unwrap_or(0),
            cache_1h: u["cache_creation"]["ephemeral_1h_input_tokens"].as_u64().unwrap_or(0),
            prompt: prompt.clone(),
            prompt_idx,
            text: text.chars().take(400).collect(),
            tools,
        });
    }
    // 파일은 이미 시간순이지만 재개·포크로 뒤섞인 경우가 있어 안정 정렬로 맞춘다
    out.sort_by(|a, b| a.ts.partial_cmp(&b.ts).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// 파일별 집계 캐시 — 대시보드를 열 때마다 최근 N일치 jsonl을 전량 다시 읽지 않도록
/// mtime이 그대로면 재사용한다 (세션 목록의 META_CACHE와 같은 전략).
pub(crate) static USAGE_FILE_CACHE: LazyLock<Mutex<HashMap<String, (f64, Vec<UsageRow>)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 대시보드용: 최근 N일간 (날짜, 모델, 프로젝트)별 토큰 집계
#[tauri::command]
pub(crate) fn usage_stats(days: u32) -> Vec<UsageRow> {
    let mut map: HashMap<UsageKey, UsageRow> = HashMap::new();
    let Some(home) = dirs::home_dir() else { return vec![] };
    let projects = home.join(".claude").join("projects");
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(days as u64 * 86400 + 86400);
    let Ok(dirs_iter) = fs::read_dir(&projects) else { return vec![] };
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

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
            let key = p.to_string_lossy().to_string();
            let mtime = file_mtime(&p);
            seen.insert(key.clone());
            let cached = {
                let cache = USAGE_FILE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
                match cache.get(&key) {
                    Some((t, rows)) if *t == mtime => Some(rows.clone()),
                    _ => None,
                }
            };
            let rows = cached.unwrap_or_else(|| {
                let rows = usage_rows_of_file(&p);
                USAGE_FILE_CACHE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, (mtime, rows.clone()));
                rows
            });
            for r in rows {
                let entry = map
                    .entry((r.date.clone(), r.model.clone(), r.cwd.clone()))
                    .or_insert_with(|| UsageRow {
                        date: r.date.clone(),
                        model: r.model.clone(),
                        cwd: r.cwd.clone(),
                        ..Default::default()
                    });
                entry.input += r.input;
                entry.output += r.output;
                entry.cache_read += r.cache_read;
                entry.cache_5m += r.cache_5m;
                entry.cache_1h += r.cache_1h;
                entry.requests += r.requests;
            }
        }
    }
    // 기간 밖으로 밀려났거나 삭제된 파일의 캐시는 버린다
    USAGE_FILE_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|k, _| seen.contains(k));
    map.into_values().collect()
}

// ---------- 요금제 한도 (5시간/주간 사용률 + 리셋 시각) ----------
// 기본: Claude Code OAuth 토큰으로 사용량 API 직접 조회 (headroom 불필요)
// 폴백: headroom이 폴링해둔 subscription_state.json

pub(crate) fn oauth_token() -> Option<String> {
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

pub(crate) fn fetch_usage_direct() -> Option<serde_json::Value> {
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
    // limits[]의 weekly_scoped 항목이 모델별 주간 한도다 (scope.model.display_name = "Fable" 등)
    let scoped: Vec<serde_json::Value> = v["limits"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|l| l["kind"] == "weekly_scoped")
                .filter_map(|l| {
                    let name = l["scope"]["model"]["display_name"].as_str()?;
                    Some(serde_json::json!({
                        "label": name,
                        "utilization_pct": l["percent"],
                        "resets_at": l["resets_at"],
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(serde_json::json!({
        "source": "direct",
        "five_hour": map_win(&v["five_hour"]),
        "seven_day": map_win(&v["seven_day"]),
        "scoped": scoped,
        "limits": v["limits"],
        "polled_at": chrono_now_iso(),
    }))
}

pub(crate) fn chrono_now_iso() -> String {
    // 의존성 없이 대략적인 ISO 시각 (frontend는 상대시간 계산에 resets_at만 사용)
    let secs = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("@{}", secs)
}

pub(crate) fn usage_from_headroom() -> Option<serde_json::Value> {
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

pub(crate) fn codex_rollouts_by_mtime() -> Vec<PathBuf> {
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
pub(crate) fn codex_state() -> Option<serde_json::Value> {
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
pub(crate) static USAGE_CACHE: Mutex<Option<(std::time::Instant, serde_json::Value)>> = Mutex::new(None);

#[tauri::command]
pub(crate) fn subscription_state(force: bool) -> Option<serde_json::Value> {
    if !force {
        let cache = USAGE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, v)) = cache.as_ref() {
            if t.elapsed().as_secs() < 180 {
                return Some(v.clone());
            }
        }
    }
    // 상태줄이 살아 있으면 그 값을 쓴다 — API 호출도 토큰도 필요 없고 429도 없다
    if let Some(sl) = statusline_rate_limits() {
        return Some(sl);
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
pub(crate) fn headroom_stats() -> Option<serde_json::Value> {
    let p = dirs::home_dir()?.join(".headroom").join("proxy_savings.json");
    let text = fs::read_to_string(p).ok()?;
    serde_json::from_str(&text).ok()
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 응답 하나가 텍스트 블록과 도구 호출 블록으로 쪼개져 여러 줄로 기록될 때,
    /// 같은 usage를 여러 번 세면 안 된다 (실측 세션에서 비용이 80%까지 부풀었던 버그).
    #[test]
    fn one_response_split_across_lines_counts_once() {
        let usage = r#"{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":100,"cache_creation":{"ephemeral_5m_input_tokens":5}}"#;
        let text = format!(
            "{}\n{}\n{}\n",
            r#"{"type":"user","message":{"content":"안녕"},"timestamp":"2026-09-08T01:00:00.000Z"}"#,
            format!(r#"{{"type":"assistant","timestamp":"2026-09-08T01:00:01.000Z","message":{{"id":"msg_A","model":"claude-opus-5","usage":{usage},"content":[{{"type":"text","text":"응답"}}]}}}}"#),
            format!(r#"{{"type":"assistant","timestamp":"2026-09-08T01:00:02.000Z","message":{{"id":"msg_A","model":"claude-opus-5","usage":{usage},"content":[{{"type":"tool_use","name":"Read","input":{{"file_path":"src/usage.rs"}}}}]}}}}"#),
        );
        let turns = turns_from_text(&text);
        assert_eq!(turns.len(), 1, "같은 message.id는 한 턴");
        assert_eq!(turns[0].output, 20);
        assert_eq!(turns[0].cache_read, 100);
        assert_eq!(turns[0].prompt, "안녕");
        assert_eq!(turns[0].prompt_idx, 1);
        // 뒷줄의 내용은 버리지 않고 같은 턴에 합쳐진다 — 답변과 도구 흔적이 둘 다 남아야 한다
        assert_eq!(turns[0].text, "응답");
        assert_eq!(turns[0].tools, vec!["Read src/usage.rs".to_string()]);

        // 대시보드 집계도 같은 규칙이어야 한다
        let dir = std::env::temp_dir().join(format!("deck-usage-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let f = dir.join("s.jsonl");
        fs::write(&f, &text).unwrap();
        let rows = usage_rows_of_file(&f);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].requests, 1, "한 응답 = 요청 1회");
        assert_eq!(rows[0].output, 20);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_arg_picks_the_readable_field() {
        let write = serde_json::json!({"content": "<!doctype html>", "file_path": "ui/index.html"});
        assert_eq!(tool_arg("Write", &write), "ui/index.html");
        let bash = serde_json::json!({"command": "cd /x && python - <<EOF", "description": "Run tests"});
        assert_eq!(tool_arg("Bash", &bash), "Run tests");
    }

    /// UI 하네스용 덤프 — 실제 세션 파일을 Rust 경로로 파싱해 JSON으로 떨군다.
    /// `cargo test -- --ignored dump_turns` 로 실행. 경로는 DECK_DUMP_IN/OUT.
    #[test]
    #[ignore]
    fn dump_turns() {
        let src = std::env::var("DECK_DUMP_IN").expect("DECK_DUMP_IN");
        let dst = std::env::var("DECK_DUMP_OUT").expect("DECK_DUMP_OUT");
        let text = fs::read_to_string(&src).unwrap();
        let turns = turns_from_text(&text);
        fs::write(&dst, serde_json::to_string(&turns).unwrap()).unwrap();
        eprintln!("{} turns -> {dst}", turns.len());
    }
}
