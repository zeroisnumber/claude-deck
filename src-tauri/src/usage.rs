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
    /// "claude" | "codex" — 프런트가 요금 표시 여부를 가른다
    pub(crate) agent: String,
}

pub(crate) type UsageKey = (String, String, String); // (날짜, 모델, 프로젝트)

/// 응답 하나. 파일별 캐시는 집계된 행이 아니라 이걸 담는다 — 포크·재개가 부모의
/// 기록을 자식 파일에 같은 응답 id로 복사하기 때문에, 파일 안에서만 중복을 지우면
/// 파일 사이의 중복이 남는다 (실측: 최근 30일 화면에서 비용 36% 과다).
#[derive(Serialize, Default, Clone)]
pub(crate) struct UsageEntry {
    /// API 응답 id. 코덱스처럼 id가 없는 저장소는 파일 경로로 만든 고유값을 쓴다.
    pub(crate) id: String,
    pub(crate) date: String,
    pub(crate) model: String,
    pub(crate) cwd: String,
    pub(crate) agent: String,
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_5m: u64,
    pub(crate) cache_1h: u64,
}

/// 세션 파일 하나의 응답 목록. 집계는 호출 쪽에서 파일 사이 중복까지 지운 뒤에 한다.
pub(crate) fn usage_entries_of_file(path: &PathBuf) -> Vec<UsageEntry> {
    let Ok(text) = fs::read_to_string(path) else { return vec![] };
    let mut out = Vec::new();
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
        let id = match obj["message"]["id"].as_str() {
            Some(id) => {
                if !counted.insert(id.to_string()) {
                    continue;
                }
                id.to_string()
            }
            // id가 없는 옛 기록은 파일 안 위치로 고유값을 만든다 (파일 간 중복 제거 대상 아님)
            None => format!("{}#{}", path.to_string_lossy(), out.len()),
        };
        let ts = obj["timestamp"].as_str().unwrap_or("");
        if ts.len() < 10 {
            continue;
        }
        out.push(UsageEntry {
            id,
            date: ts[..10].to_string(),
            model: obj["message"]["model"].as_str().unwrap_or("?").to_string(),
            cwd: cwd.clone(),
            agent: "claude".into(),
            input: u["input_tokens"].as_u64().unwrap_or(0),
            output: u["output_tokens"].as_u64().unwrap_or(0),
            cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
            cache_5m: u["cache_creation"]["ephemeral_5m_input_tokens"].as_u64().unwrap_or(0),
            cache_1h: u["cache_creation"]["ephemeral_1h_input_tokens"].as_u64().unwrap_or(0),
        });
    }
    // cwd는 파일 앞부분에서야 나오는 경우가 있어 뒤늦게 채운다
    if !cwd.is_empty() {
        for e in out.iter_mut() {
            if e.cwd.is_empty() {
                e.cwd = cwd.clone();
            }
        }
    }
    out
}

/// 파일 하나만 집계한 행 (테스트용 — 실사용 경로는 파일 사이 중복까지 지운다)
#[cfg(test)]
pub(crate) fn usage_rows_of_file(path: &PathBuf) -> Vec<UsageRow> {
    aggregate(usage_entries_of_file(path))
}

/// 항목들을 (날짜, 모델, 프로젝트)별로 합친다
pub(crate) fn aggregate(entries: Vec<UsageEntry>) -> Vec<UsageRow> {
    let mut map: HashMap<UsageKey, UsageRow> = HashMap::new();
    for e in entries {
        let row = map
            .entry((e.date.clone(), e.model.clone(), e.cwd.clone()))
            .or_insert_with(|| UsageRow {
                date: e.date.clone(),
                model: e.model.clone(),
                cwd: e.cwd.clone(),
                agent: e.agent.clone(),
                ..Default::default()
            });
        row.input += e.input;
        row.output += e.output;
        row.cache_read += e.cache_read;
        row.cache_5m += e.cache_5m;
        row.cache_1h += e.cache_1h;
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
    // 경로는 앞이 아니라 뒤가 정보다. "C:\\workspace\\git\\claude-deck\\src-tauri\\src"까지
    // 보여주고 잘리면 어느 파일인지 알 수 없다.
    fn is_pathish(k: &str) -> bool {
        k == "file_path" || k == "path" || k == "notebook_path"
    }
    // Bash는 명령줄보다 description이 훨씬 읽기 쉽다 (Claude Code 자신도 그걸 보여준다)
    let keys: &[&str] = if name == "Bash" {
        &["description", "command"]
    } else {
        &["file_path", "pattern", "path", "query", "url", "prompt", "command", "description"]
    };
    for k in keys {
        if let Some(v) = input[*k].as_str() {
            if !v.is_empty() {
                if is_pathish(k) {
                    return path_tail(v);
                }
                return v;
            }
        }
    }
    input
        .as_object()
        .and_then(|o| o.values().find_map(|v| v.as_str()))
        .unwrap_or("")
}

/// 긴 경로는 마지막 세 조각만 남긴다 — 어느 파일인지는 뒤에 있다.
fn path_tail(p: &str) -> &str {
    if p.chars().count() <= 40 {
        return p;
    }
    let sep = |c: char| c == '/' || c == '\\';
    let mut cut = None;
    for (i, _) in p.rmatch_indices(sep).take(3) {
        cut = Some(i + 1);
    }
    match cut {
        Some(i) if i < p.len() => &p[i..],
        _ => p,
    }
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
    if path.components().any(|c| c.as_os_str() == ".codex") {
        return Ok(codex_turns_from_text(&text));
    }
    Ok(turns_from_text(&text))
}

/// Codex rollout을 클로드와 같은 모양의 턴 목록으로. 스키마가 전혀 달라서 별도 파서다.
///
/// - 토큰은 total_token_usage의 차분 (중복 이벤트가 있어 last_token_usage 합산은 부정확)
/// - 한 턴의 경계는 token_count 이벤트다. 그 사이에 쌓인 말과 도구 호출을 그 턴에 붙인다.
/// - Codex에는 캐시 쓰기 개념이 따로 없어 cache_5m/1h는 항상 0이다.
pub(crate) fn codex_turns_from_text(text: &str) -> Vec<TurnRow> {
    let mut out: Vec<TurnRow> = Vec::new();
    let mut model = String::from("?");
    let mut prompt = String::new();
    let mut prompt_idx: u32 = 0;
    let (mut p_in, mut p_cached, mut p_out) = (0u64, 0u64, 0u64);
    let mut said = String::new();
    let mut tools: Vec<String> = Vec::new();

    for line in text.lines() {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
        let payload = &obj["payload"];
        match obj["type"].as_str().unwrap_or("") {
            "turn_context" => {
                if let Some(m) = payload["model"].as_str() {
                    model = m.to_string();
                }
            }
            // 도구 호출은 response_item으로 따로 기록된다 (arguments는 JSON 문자열)
            "response_item" if payload["type"] == "function_call" => {
                let name = payload["name"].as_str().unwrap_or("tool");
                let args = payload["arguments"].as_str().unwrap_or("");
                let arg = serde_json::from_str::<serde_json::Value>(args)
                    .ok()
                    .and_then(|v| {
                        v.as_object()
                            .and_then(|o| o.values().find_map(|x| x.as_str()).map(str::to_string))
                    })
                    .unwrap_or_default();
                let arg: String = arg.split('\n').next().unwrap_or("").chars().take(48).collect();
                tools.push(if arg.is_empty() { name.to_string() } else { format!("{name} {arg}") });
            }
            "event_msg" => match payload["type"].as_str().unwrap_or("") {
                "user_message" => {
                    if let Some(m) = payload["message"].as_str() {
                        let m = m.trim();
                        if !m.is_empty() && !m.starts_with('<') {
                            prompt = m.chars().take(300).collect();
                            prompt_idx += 1;
                        }
                    }
                }
                "agent_message" => {
                    if let Some(m) = payload["message"].as_str() {
                        if said.is_empty() {
                            said = m.chars().take(400).collect();
                        }
                    }
                }
                "token_count" => {
                    let t = &payload["info"]["total_token_usage"];
                    let (i, c, o) = (
                        t["input_tokens"].as_u64().unwrap_or(0),
                        t["cached_input_tokens"].as_u64().unwrap_or(0),
                        t["output_tokens"].as_u64().unwrap_or(0),
                    );
                    // 압축·롤백으로 누적값이 되감기면 차분을 낼 수 없다. 0부터 다시 세면
                    // 그 시점까지를 한 번 더 더하게 되므로, 그 이벤트만은 last_token_usage
                    // (그 턴 하나의 사용량)를 쓴다.
                    let rewound = i < p_in || c < p_cached || o < p_out;
                    let (d_in, d_c, d_out) = if rewound {
                        let l = &payload["info"]["last_token_usage"];
                        (
                            l["input_tokens"].as_u64().unwrap_or(0),
                            l["cached_input_tokens"].as_u64().unwrap_or(0),
                            l["output_tokens"].as_u64().unwrap_or(0),
                        )
                    } else {
                        (i - p_in, c - p_cached, o - p_out)
                    };
                    p_in = i;
                    p_cached = c;
                    p_out = o;
                    if d_in == 0 && d_c == 0 && d_out == 0 {
                        continue; // 같은 값을 다시 낸 중복 이벤트
                    }
                    let Some(ts) = obj["timestamp"].as_str().and_then(parse_iso_ts) else { continue };
                    out.push(TurnRow {
                        ts,
                        model: model.clone(),
                        input: d_in - d_c.min(d_in),
                        output: d_out,
                        cache_read: d_c,
                        cache_5m: 0,
                        cache_1h: 0,
                        prompt: prompt.clone(),
                        prompt_idx,
                        text: std::mem::take(&mut said),
                        tools: std::mem::take(&mut tools),
                    });
                }
                _ => {}
            },
            _ => {}
        }
    }
    out
}

/// session_turns의 본체 — 저장소 경로 검사 없이 텍스트만 파싱해 테스트할 수 있게 분리
pub(crate) fn turns_from_text(text: &str) -> Vec<TurnRow> {
    let mut out: Vec<TurnRow> = Vec::new();
    let mut prompt = String::new();
    let mut prompt_idx: u32 = 0;
    // usage_entries_of_file과 같은 이유로 message.id 기준 중복 제거 (한 응답 = 한 줄이 아니다)
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

/// Codex rollout 하나를 (날짜, 모델, 프로젝트)별로 집계.
///
/// Codex는 turn마다 token_count 이벤트를 남기는데 거기 담긴 last_token_usage를 그냥
/// 더하면 안 된다 — 같은 값을 다시 내보내는 중복 이벤트가 있어서 실측 세션에서 2.3%
/// 넘게 부풀었다. total_token_usage는 18개 세션 전부에서 단조 증가했고 그 차분을 더하면
/// 마지막 total과 정확히 일치하므로, 차분을 쓴다.
///
/// input_tokens는 cached_input_tokens를 포함한다. 대시보드의 input은 캐시가 아닌
/// 입력이므로 둘의 차이를 넣는다.
/// 파일 하나만 집계한 행 (테스트용)
#[cfg(test)]
pub(crate) fn codex_rows_of_file(path: &PathBuf) -> Vec<UsageRow> {
    aggregate(codex_entries_of_file(path))
}

pub(crate) fn codex_entries_of_file(path: &PathBuf) -> Vec<UsageEntry> {
    let Ok(text) = fs::read_to_string(path) else { return vec![] };
    let mut out: Vec<UsageEntry> = Vec::new();
    let mut cwd = String::new();
    let mut model = String::from("?");
    let (mut p_in, mut p_cached, mut p_out) = (0u64, 0u64, 0u64);

    for line in text.lines() {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
        let payload = &obj["payload"];
        match obj["type"].as_str().unwrap_or("") {
            "session_meta" => {
                if let Some(c) = payload["cwd"].as_str() {
                    cwd = strip_verbatim(c);
                }
            }
            "turn_context" => {
                if let Some(m) = payload["model"].as_str() {
                    model = m.to_string();
                }
            }
            "event_msg" if payload["type"] == "token_count" => {
                let t = &payload["info"]["total_token_usage"];
                let (i, c, o) = (
                    t["input_tokens"].as_u64().unwrap_or(0),
                    t["cached_input_tokens"].as_u64().unwrap_or(0),
                    t["output_tokens"].as_u64().unwrap_or(0),
                );
                // 되감김(압축·롤백)이면 그 시점부터 다시 센다
                // 압축·롤백으로 누적값이 되감기면 차분을 낼 수 없다. 0부터 다시 세면
                // 그 시점까지를 한 번 더 더하게 되므로, 그 이벤트만은 last_token_usage
                // (그 턴 하나의 사용량)를 쓴다.
                let rewound = i < p_in || c < p_cached || o < p_out;
                let (d_in, d_c, d_out) = if rewound {
                    let l = &payload["info"]["last_token_usage"];
                    (
                        l["input_tokens"].as_u64().unwrap_or(0),
                        l["cached_input_tokens"].as_u64().unwrap_or(0),
                        l["output_tokens"].as_u64().unwrap_or(0),
                    )
                } else {
                    (i - p_in, c - p_cached, o - p_out)
                };
                p_in = i;
                p_cached = c;
                p_out = o;
                if d_in == 0 && d_c == 0 && d_out == 0 {
                    continue; // 같은 값을 다시 낸 중복 이벤트
                }
                let ts = obj["timestamp"].as_str().unwrap_or("");
                if ts.len() < 10 {
                    continue;
                }
                // 코덱스 rollout에는 응답 id가 없다. 파일 사이 중복 대상이 아니므로
                // 파일 경로와 순번으로 고유값을 만든다.
                out.push(UsageEntry {
                    id: format!("{}#{}", path.to_string_lossy(), out.len()),
                    date: ts[..10].to_string(),
                    model: model.clone(),
                    cwd: cwd.clone(),
                    agent: "codex".into(),
                    input: d_in - d_c.min(d_in),
                    output: d_out,
                    cache_read: d_c,
                    cache_5m: 0,
                    cache_1h: 0,
                });
            }
            _ => {}
        }
    }
    out
}

/// Windows의 `\\?\` 접두사를 떼어 탭의 cwd와 같은 모양으로 맞춘다
pub(crate) fn strip_verbatim(p: &str) -> String {
    p.strip_prefix(r"\\?\").unwrap_or(p).to_string()
}

/// ~/.codex/sessions 아래의 rollout 파일 전부
pub(crate) fn codex_rollout_files() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else { return vec![] };
    let mut out = Vec::new();
    let mut stack = vec![home.join(".codex").join("sessions")];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                out.push(p);
            }
        }
    }
    out
}

/// 파일별 집계 캐시 — 대시보드를 열 때마다 최근 N일치 jsonl을 전량 다시 읽지 않도록
/// mtime이 그대로면 재사용한다 (세션 목록의 META_CACHE와 같은 전략).
pub(crate) static USAGE_FILE_CACHE: LazyLock<Mutex<HashMap<String, (f64, Vec<UsageEntry>)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 대시보드용: 최근 N일간 (날짜, 모델, 프로젝트)별 토큰 집계
#[tauri::command]
pub(crate) fn usage_stats(days: u32) -> Vec<UsageRow> {
    let mut all_entries: Vec<UsageEntry> = Vec::new();
    // 파일 사이 중복 제거용 — 포크가 복사해 온 응답을 두 번 세지 않기 위해
    let mut counted: std::collections::HashSet<String> = std::collections::HashSet::new();
    let Some(home) = dirs::home_dir() else { return vec![] };
    let projects = home.join(".claude").join("projects");
    // days=0은 전체 기간 — 파일을 mtime으로 거르지 않는다
    let cutoff = if days == 0 {
        None
    } else {
        Some(
            std::time::SystemTime::now()
                - std::time::Duration::from_secs(days as u64 * 86400 + 86400),
        )
    };
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 클로드 세션 + 코덱스 rollout. 저장소 구조가 달라서 목록을 먼저 모으고
    // 파일 위치로 파서를 고른다.
    let mut all: Vec<PathBuf> = Vec::new();
    if let Ok(dirs_iter) = fs::read_dir(&projects) {
        for proj in dirs_iter.flatten() {
            if let Ok(files) = fs::read_dir(proj.path()) {
                all.extend(files.flatten().map(|f| f.path()));
            }
        }
    }
    all.extend(codex_rollout_files());
    // 원본이 사본보다 먼저 오도록 오래된 파일부터 — 중복은 나중에 만난 쪽을 버린다
    all.sort_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());

    {
        for p in all {
            if !p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                continue;
            }
            // 추가 기록은 mtime을 갱신하므로 오래된 파일은 통째로 건너뜀
            if let Some(cutoff) = cutoff {
                if fs::metadata(&p)
                    .and_then(|m| m.modified())
                    .map(|t| t < cutoff)
                    .unwrap_or(true)
                {
                    continue;
                }
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
            let entries = cached.unwrap_or_else(|| {
                let is_codex = p.components().any(|c| c.as_os_str() == ".codex");
                let rows = if is_codex {
                    codex_entries_of_file(&p)
                } else {
                    usage_entries_of_file(&p)
                };
                USAGE_FILE_CACHE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, (mtime, rows.clone()));
                rows
            });
            for e in entries {
                // 포크·재개는 부모의 기록을 그대로 복사해 온다. 같은 응답 id를
                // 다시 만나면 버린다 — 파일을 오래된 순으로 도니 원본이 남는다.
                if !counted.insert(e.id.clone()) {
                    continue;
                }
                all_entries.push(e);
            }
        }
    }
    // 기간 밖으로 밀려났거나 삭제된 파일의 캐시는 버린다
    USAGE_FILE_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|k, _| seen.contains(k));
    aggregate(all_entries)
}

// ---------- 요금제 한도 (5시간/주간 사용률 + 리셋 시각) ----------
// 상태줄 페이로드가 있으면 그걸 쓰고, 없으면 Claude Code OAuth 토큰으로 사용량 API를 부른다.

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
    // direct 호출이 실패하면(429 등) 캐시 TTL을 넘겼더라도 직전에 성공한 응답을 쓴다 —
    // 조금 묵은 숫자가 빈 게이지보다 낫다.
    let cache = USAGE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.as_ref().map(|(_, v)| v.clone())
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
    fn long_paths_keep_their_tail() {
        let long = r"C:\workspace\git\claude-deck\src-tauri\src\usage.rs";
        let v = serde_json::json!({ "file_path": long });
        assert_eq!(tool_arg("Read", &v), r"src-tauri\src\usage.rs", "마지막 세 조각");
        let short = serde_json::json!({ "file_path": "ui/main.js" });
        assert_eq!(tool_arg("Read", &short), "ui/main.js", "짧으면 그대로 둔다");
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
        let turns = if src.contains(".codex") {
            codex_turns_from_text(&text)
        } else {
            turns_from_text(&text)
        };
        fs::write(&dst, serde_json::to_string(&turns).unwrap()).unwrap();
        eprintln!("{} turns -> {dst}", turns.len());
    }
}

#[cfg(test)]
mod codex_tests {
    use super::*;

    /// 중복 token_count 이벤트를 그냥 더하면 토큰이 부풀어 오른다 (실측 2.3%).
    /// 누적값의 차분을 써야 마지막 total과 정확히 맞는다.
    #[test]
    fn duplicate_token_events_are_not_double_counted() {
        let ev = |ts: &str, i: u64, c: u64, o: u64| {
            format!(
                r#"{{"type":"event_msg","timestamp":"{ts}","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{i},"cached_input_tokens":{c},"output_tokens":{o}}}}}}}}}"#
            )
        };
        let text = format!(
            "{}\n{}\n{}\n{}\n",
            r#"{"type":"session_meta","payload":{"cwd":"\\\\?\\C:\\work"}}"#,
            r#"{"type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
            ev("2026-05-25T03:10:36Z", 100, 40, 10),
            // 같은 값을 다시 낸 중복 이벤트 — 무시돼야 한다
            ev("2026-05-25T03:10:37Z", 100, 40, 10),
        );
        let dir = std::env::temp_dir().join(format!("deck-cxu-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let f = dir.join("rollout.jsonl");
        fs::write(&f, text).unwrap();
        let rows = codex_rows_of_file(&f);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.requests, 1, "중복 이벤트는 요청으로 세지 않는다");
        assert_eq!(r.cache_read, 40);
        assert_eq!(r.input, 60, "input_tokens는 캐시를 포함하므로 빼고 넣는다");
        assert_eq!(r.output, 10);
        assert_eq!(r.model, "gpt-5.5");
        assert_eq!(r.agent, "codex");
        assert_eq!(r.cwd, r"C:\work", r"\?\ 접두사는 떼어낸다");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 30일 창의 실제 합계 — `cargo test -- --ignored window_cost`
    #[test]
    #[ignore]
    fn window_cost() {
        let rows = usage_stats(30);
        let price = |m: &str| -> (f64, f64) {
            if m.contains("sonnet") {
                (3.0, 15.0)
            } else if m.contains("haiku") {
                (1.0, 5.0)
            } else {
                (5.0, 25.0)
            }
        };
        let mut cost = 0.0;
        let mut req = 0u64;
        for r in &rows {
            // 프런트가 하는 날짜 필터를 똑같이 적용해야 비교가 된다
            let cutoff = std::env::var("DECK_CUTOFF").unwrap_or_default();
            if r.agent != "claude" || r.date < cutoff {
                continue;
            }
            let (i, o) = price(&r.model);
            cost += (r.input as f64 * i
                + r.cache_read as f64 * i * 0.1
                + r.cache_5m as f64 * i * 1.25
                + r.cache_1h as f64 * i * 2.0
                + r.output as f64 * o)
                / 1e6;
            req += r.requests;
        }
        eprintln!("claude rows={} requests={req} cost=${cost:.2}", rows.len());
    }

    /// 대시보드가 실제로 코덱스 줄을 내보내는지 — `cargo test -- --ignored dash_rows`
    #[test]
    #[ignore]
    fn dash_rows() {
        let rows = usage_stats(std::env::var("DECK_DAYS").ok().and_then(|v| v.parse().ok()).unwrap_or(30));
        let cx: Vec<_> = rows.iter().filter(|r| r.agent == "codex").collect();
        eprintln!("total rows={} codex rows={}", rows.len(), cx.len());
        for r in &cx {
            eprintln!(
                "  {} {} {} in={} cache={} out={} req={}",
                r.date, r.model, r.cwd, r.input, r.cache_read, r.output, r.requests
            );
        }
    }

    /// 실제 rollout 전체 집계 — `cargo test -- --ignored codex_totals`
    #[test]
    #[ignore]
    fn codex_totals() {
        let (mut i, mut c, mut o, mut req) = (0u64, 0u64, 0u64, 0u64);
        let files = codex_rollout_files();
        for p in &files {
            for r in codex_rows_of_file(p) {
                i += r.input;
                c += r.cache_read;
                o += r.output;
                req += r.requests;
            }
        }
        eprintln!(
            "{} rollouts: uncached_in={i} cached_in={c} out={o} total={} requests={req}",
            files.len(),
            i + c + o
        );
    }
}
