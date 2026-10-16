//! 사이드바 검색이 제목·프로젝트만 보던 것을 대화 본문까지 넓힌다.
//!
//! 기록 파일은 수십 MB까지 간다(실측 33MB). 매 글자마다 줄을 전부 JSON으로 풀면 느리므로
//! 먼저 날 바이트에서 검색어를 찾고, 걸린 줄만 풀어 사람이 친 말·에이전트의 답 안에 정말
//! 있는지 본다 — 도구 결과(읽은 파일 내용, 명령 출력)에만 있는 말은 대화가 아니라 뺀다.
//! 대소문자는 영문만 무시한다.
use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize, Clone)]
pub(crate) struct Hit {
    session_id: String,
    /// 찾은 자리 앞뒤 몇십 글자
    snippet: String,
    /// "user" | "assistant"
    role: String,
}

const SNIPPET_SIDE: usize = 36;
const MAX_HITS: usize = 40;

/// files: [세션 id, 기록 파일]. 프런트가 아는 세션만 넘긴다(경로는 세션 저장소 안인지 본다).
#[tauri::command(async)]
pub(crate) fn search_sessions(query: String, files: Vec<(String, String)>) -> Vec<Hit> {
    let q = query.trim().to_string();
    if q.chars().count() < 2 {
        return vec![];
    }
    let files: Vec<(String, PathBuf)> = files
        .into_iter()
        .take(2000)
        .filter_map(|(id, f)| crate::session_file_in_store(&f).ok().map(|p| (id, p)))
        .collect();
    // 파일을 네 갈래로 나눠 동시에 읽는다(디스크·CPU 모두 한 줄로 읽기엔 아깝다)
    let n = 4.min(files.len().max(1));
    let chunks: Vec<Vec<(String, PathBuf)>> = (0..n)
        .map(|k| files.iter().skip(k).step_by(n).cloned().collect())
        .collect();
    let mut hits: Vec<Hit> = std::thread::scope(|sc| {
        let q = &q;
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| sc.spawn(move || chunk.iter().filter_map(|(id, p)| search_file(id, p, q)).collect::<Vec<_>>()))
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    hits.truncate(MAX_HITS);
    hits
}

/// 이 기록에서 가장 최근에 나온 자리 하나(최근 대화가 더 쓸모 있다).
fn search_file(id: &str, path: &std::path::Path, q: &str) -> Option<Hit> {
    // 바이트 검색과 같은 규칙(영문만 소문자로)으로 맞춘다
    let needle = q.to_ascii_lowercase();
    // Gemini는 통짜 JSON 하나다(작다) — 줄 단위로 볼 수 없어 통째로 푼다
    if path.extension().is_some_and(|e| e == "json") {
        let bytes = std::fs::read(path).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let msgs = v["messages"].as_array()?;
        return msgs.iter().rev().find_map(|m| {
            let role = if m["type"] == "user" { "user" } else { "assistant" };
            let text = m["content"].as_array().map(|c| {
                c.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")
            })?;
            snippet_of(&text, &needle).map(|snippet| Hit { session_id: id.into(), snippet, role: role.into() })
        });
    }
    // 줄 단위로 읽으며 날 바이트에서 찾는다 — 파일을 통째로 올리면 큰 기록(수백 MB)
    // 넷을 동시에 볼 때 메모리가 GB 단위로 튄다. 영문 대소문자는 줄을 소문자로 바꿔
    // 무시한다(한글 등의 UTF-8 바이트는 ASCII 소문자 변환에 영향받지 않는다). 검색어에
    // 영문이 없으면 바꾸지 않는다. 걸린 줄만 JSON으로 풀어 대화 글자 안에 있는지 보고,
    // 끝까지 읽어 가장 마지막(최근) 것을 남긴다. JSON 안의 \n·\" 이스케이프와 겹치는
    // 검색어는 놓칠 수 있지만 틀린 결과는 나오지 않는다.
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut rd = std::io::BufReader::with_capacity(1 << 20, file);
    let finder = memchr::memmem::Finder::new(needle.as_bytes());
    let fold = needle.bytes().any(|b| b.is_ascii_alphabetic());
    let (mut line, mut lower) = (Vec::new(), Vec::new());
    let mut last = None;
    loop {
        line.clear();
        match rd.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let hit = if fold {
            lower.clear();
            lower.extend_from_slice(&line);
            lower.make_ascii_lowercase();
            finder.find(&lower).is_some()
        } else {
            finder.find(&line).is_some()
        };
        if !hit {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line) else { continue };
        for (role, text) in spoken_texts(&v) {
            if let Some(snippet) = snippet_of(&text, &needle) {
                last = Some(Hit { session_id: id.into(), snippet, role: role.into() });
            }
        }
    }
    last
}

/// 한 줄(레코드)에서 사람이 친 말·에이전트의 답만. 도구 결과·생각(thinking)은 뺀다.
fn spoken_texts(v: &serde_json::Value) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    match v["type"].as_str().unwrap_or("") {
        // 클로드
        "user" | "assistant" if v["isSidechain"] != true => {
            let role = if v["type"] == "user" { "user" } else { "assistant" };
            let text = crate::sessions::extract_text(&v["message"]["content"]);
            if !text.trim().is_empty() && !crate::sessions::is_continuation_notice(&text) {
                out.push((role, text));
            }
        }
        // codex: 사용자 메시지·답글 이벤트
        "event_msg" => match v["payload"]["type"].as_str().unwrap_or("") {
            "user_message" => {
                if let Some(m) = v["payload"]["message"].as_str() {
                    out.push(("user", m.to_string()));
                }
            }
            "agent_message" => {
                if let Some(m) = v["payload"]["message"].as_str() {
                    out.push(("assistant", m.to_string()));
                }
            }
            _ => {}
        },
        _ => {}
    }
    out
}

/// 찾은 자리 앞뒤 몇십 글자를 한 줄로. 못 찾으면 None.
fn snippet_of(text: &str, needle_lower: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let at = lower.find(needle_lower)?;
    // 바이트 위치를 글자 위치로 (한글은 한 글자가 3바이트)
    let char_at = lower[..at].chars().count();
    let chars: Vec<char> = text.chars().collect();
    let start = char_at.saturating_sub(SNIPPET_SIDE);
    let end = (char_at + needle_lower.chars().count() + SNIPPET_SIDE).min(chars.len());
    let body: String = chars[start..end].iter().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
    Some(format!("{}{}{}", if start > 0 { "…" } else { "" }, body, if end < chars.len() { "…" } else { "" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_spoken_text_not_tool_output() {
        let dir = std::env::temp_dir().join(format!("deck-search-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("s.jsonl");
        let lines = [
            r#"{"type":"user","message":{"content":"셰이더 컴파일 오류가 나요"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"DamageCalc shader line 12"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Shader 캐시를 지우면 됩니다"}]}}"#,
        ];
        std::fs::write(&f, lines.join("\n")).unwrap();
        // 가장 최근(뒤) 줄의 답에서 먼저 찾는다 — 영문은 대소문자 무시
        let h = search_file("s1", &f, "shader").unwrap();
        assert_eq!(h.role, "assistant");
        assert!(h.snippet.contains("Shader 캐시"), "{}", h.snippet);
        // 한글
        let h = search_file("s1", &f, "컴파일").unwrap();
        assert_eq!(h.role, "user");
        // 도구 결과에만 있는 말은 대화가 아니다
        assert!(search_file("s1", &f, "DamageCalc").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 이 기기의 기록 전부에서 찾는 데 얼마나 걸리나. `cargo test -- --ignored real_search --nocapture`
    #[test]
    #[ignore]
    fn real_search() {
        let home = dirs::home_dir().unwrap();
        let mut files = Vec::new();
        for proj in std::fs::read_dir(home.join(".claude").join("projects")).unwrap().flatten() {
            for f in std::fs::read_dir(proj.path()).into_iter().flatten().flatten() {
                if f.path().extension().is_some_and(|e| e == "jsonl") {
                    files.push((f.file_name().to_string_lossy().into_owned(), f.path().to_string_lossy().into_owned()));
                }
            }
        }
        for f in crate::usage::codex_rollout_files() {
            files.push((f.to_string_lossy().into_owned(), f.to_string_lossy().into_owned()));
        }
        let mb: u64 = files.iter().filter_map(|(_, f)| std::fs::metadata(f).ok()).map(|m| m.len()).sum::<u64>() / 1_000_000;
        for q in ["셰이더", "renderTabs", "zzqq-없는-말"] {
            let t = std::time::Instant::now();
            let hits = search_sessions(q.into(), files.clone());
            eprintln!("{q}: 파일 {}개 {mb}MB → {}개, {:.0}ms", files.len(), hits.len(), t.elapsed().as_secs_f64() * 1000.0);
            if let Some(h) = hits.first() {
                eprintln!("  예: [{}] {}", h.role, h.snippet);
            }
        }
    }

    #[test]
    fn snippet_trims_around_the_match() {
        let long = format!("{} 찾을말 {}", "가".repeat(80), "나".repeat(80));
        let s = snippet_of(&long, "찾을말").unwrap();
        assert!(s.starts_with('…') && s.ends_with('…') && s.contains("찾을말"), "{s}");
        assert!(s.chars().count() < 90, "{}", s.chars().count());
    }
}
