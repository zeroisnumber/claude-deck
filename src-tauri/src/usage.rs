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

