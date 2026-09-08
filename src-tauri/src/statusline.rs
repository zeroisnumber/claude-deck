// Claude Code 상태줄 탭 — 세션별 상태 페이로드 수집

use super::*;

// ---------- statusLine 탭 ----------
// Claude Code는 상태줄 명령에 세션 JSON을 stdin으로 넘긴다. 거기엔 요금제 한도,
// 실제 컨텍스트 윈도우 크기, 실제 비용이 들어 있다 — 우리가 지금 API 호출이나
// 추측으로 얻는 값들이다. 그 흐름을 옆으로 복사해 두고, 화면 출력은 원래대로
// 통과시킨다(사용자가 쓰던 상태줄이 있으면 그걸 실행해 그대로 넘긴다).
//
// 사용자의 ~/.claude/settings.json은 읽기만 하고 절대 수정하지 않는다.
// 우리 설정은 별도 파일로 두고 spawn 시 --settings 로 넘긴다.

pub(crate) fn status_dir() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("com.user.cli-deck").join("status"))
}

/// 사용자가 원래 쓰던 상태줄 명령 (없으면 None). 우리 자신은 걸러 재귀를 막는다.
pub(crate) fn user_statusline_command() -> Option<String> {
    let base = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".claude"));
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(base.join("settings.json")).ok()?).ok()?;
    let cmd = v["statusLine"]["command"].as_str()?.trim().to_string();
    if cmd.is_empty() || cmd.contains(STATUSLINE_FLAG) {
        return None;
    }
    Some(cmd)
}

pub(crate) const STATUSLINE_FLAG: &str = "--statusline-tap";

/// GUI를 띄우지 않고 stdin만 처리하고 끝나는 모드
pub(crate) fn run_statusline_tap() {
    use std::io::{Read as _, Write as _};
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    // 세션별로 저장 — 탭과 1:1로 매칭된다
    if let (Some(dir), Ok(v)) = (
        status_dir(),
        serde_json::from_str::<serde_json::Value>(&input),
    ) {
        if let Some(sid) = v["session_id"].as_str() {
            if fs::create_dir_all(&dir).is_ok() {
                let _ = fs::write(dir.join(format!("{}.json", sid)), &input);
            }
        }
    }
    // 화면은 원래대로: 사용자 명령이 있으면 같은 stdin으로 실행해 출력을 그대로 넘긴다
    if let Some(cmd) = user_statusline_command() {
        use std::process::{Command, Stdio};
        if let Ok(mut child) = Command::new("cmd.exe")
            .args(["/c", &cmd])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        {
            if let Some(mut si) = child.stdin.take() {
                let _ = si.write_all(input.as_bytes());
            }
            if let Ok(out) = child.wait_with_output() {
                let _ = std::io::stdout().write_all(&out.stdout);
            }
        }
    }
}

/// 세션의 최신 상태줄 페이로드
pub(crate) fn read_status(session_id: &str) -> Option<serde_json::Value> {
    let p = status_dir()?.join(format!("{}.json", session_id));
    serde_json::from_str(&fs::read_to_string(p).ok()?).ok()
}

/// 가장 최근에 갱신된 상태줄 페이로드의 요금제 한도.
/// 너무 오래된 값은 쓰지 않는다(세션이 다 닫혀 있으면 갱신이 멈춘다).
pub(crate) fn statusline_rate_limits() -> Option<serde_json::Value> {
    let dir = status_dir()?;
    let mut newest: Option<(f64, PathBuf)> = None;
    for e in fs::read_dir(&dir).ok()?.flatten() {
        let p = e.path();
        let t = file_mtime(&p);
        if newest.as_ref().map(|(bt, _)| t > *bt).unwrap_or(true) {
            newest = Some((t, p));
        }
    }
    let (mtime, path) = newest?;
    let age = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs_f64()
        - mtime;
    if age > 600.0 {
        return None; // 10분 넘게 안 갱신됐으면 신뢰하지 않는다
    }
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let rl = &v["rate_limits"];
    if rl["five_hour"].is_null() {
        return None;
    }
    let map = |w: &serde_json::Value| {
        serde_json::json!({
            "utilization_pct": w["used_percentage"],
            "resets_at": w["resets_at"].as_i64().map(|t| t * 1000),
        })
    };
    // 모델별 주간 창 (예: Fable). 서버가 줄 때만 있다.
    let scoped: Vec<serde_json::Value> = rl["model_scoped"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|w| {
                    let name = w["display_name"].as_str()?;
                    Some(serde_json::json!({
                        "label": name,
                        "utilization_pct": w["utilization"],
                        "resets_at": w["resets_at"],
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(serde_json::json!({
        "source": "statusline",
        "five_hour": map(&rl["five_hour"]),
        "seven_day": map(&rl["seven_day"]),
        "scoped": scoped,
        "polled_at": chrono_now_iso(),
    }))
}

/// CLI Deck 전용 설정 파일을 만들고 경로를 돌려준다. 이 경로를 spawn 시
/// --settings 로 넘기면 사용자 settings.json을 건드리지 않고 상태줄만 얹는다.
#[tauri::command(async)]
pub(crate) fn statusline_settings_path() -> Result<String, String> {
    let dir = dirs::data_local_dir()
        .map(|d| d.join("com.user.cli-deck"))
        .ok_or("데이터 폴더를 찾을 수 없습니다")?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let cfg = serde_json::json!({
        "statusLine": {
            "type": "command",
            "command": format!("\"{}\" {}", exe.to_string_lossy(), STATUSLINE_FLAG),
        }
    });
    let p = dir.join("statusline-settings.json");
    fs::write(&p, serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok(p.to_string_lossy().to_string())
}
