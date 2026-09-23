// 에이전트 CLI 버전 — 설치된 것과 받을 수 있는 최신판

use super::*;
use std::os::windows::process::CommandExt;

/// 콘솔 프로그램을 띄울 때 검은 창이 번쩍이지 않게 한다 (앱에는 콘솔이 없다)
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Serialize)]
pub(crate) struct AgentVersion {
    pub(crate) name: String,
    /// 설치돼 있지 않으면 None
    pub(crate) installed: Option<String>,
    /// 레지스트리를 못 읽으면 None (오프라인 등)
    pub(crate) latest: Option<String>,
    /// 최신판을 고른 배포 채널 (클로드는 사용자 설정을 따른다)
    pub(crate) channel: String,
    /// 앱에서 바로 올릴 수 있는가 — 클로드만 자체 업데이트 명령이 있다
    pub(crate) can_update: bool,
    /// 직접 올릴 때 쓸 명령 (안내용)
    pub(crate) update_cmd: String,
    /// 설치판보다 채널의 최신판이 높다
    pub(crate) update_available: bool,
}

/// "codex-cli 0.153.4", "2.1.280 (Claude Code)" 같은 출력에서 버전만 꺼낸다
pub(crate) fn parse_version(text: &str) -> Option<String> {
    for word in text.split(|c: char| c.is_whitespace() || c == '(' || c == ')') {
        let w = word.trim_start_matches('v');
        let mut parts = w.split('.');
        let ok = (0..3).all(|_| parts.next().map(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())).unwrap_or(false));
        if ok {
            return Some(w.to_string());
        }
    }
    None
}

/// 숫자 세 자리 비교. 앞판(-alpha 등)은 여기 오지 않는다 — 채널 태그로 고른 값만 비교한다.
pub(crate) fn is_newer(latest: &str, installed: &str) -> bool {
    let nums = |s: &str| -> Vec<u64> {
        s.split(['.', '-']).take(3).map(|p| p.parse().unwrap_or(0)).collect()
    };
    nums(latest) > nums(installed)
}

fn installed_version(cmd: &str) -> Option<String> {
    // npm 전역 설치는 .cmd 껍데기라 cmd.exe를 거쳐야 PATH에서 찾는다
    let out = std::process::Command::new("cmd.exe")
        .args(["/c", cmd, "--version"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

fn latest_version(package: &str, channel: &str) -> Option<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .ok()?;
    let url = format!("https://registry.npmjs.org/-/package/{package}/dist-tags");
    let v: serde_json::Value = client.get(url).send().ok()?.json().ok()?;
    v[channel].as_str().map(|s| s.to_string())
}

/// 클로드가 따르는 채널. 설정에 없으면 latest다.
fn claude_channel() -> String {
    dirs::home_dir()
        .and_then(|h| fs::read_to_string(h.join(".claude").join("settings.json")).ok())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["autoUpdatesChannel"].as_str().map(|s| s.to_string()))
        .filter(|s| s == "latest" || s == "stable")
        .unwrap_or_else(|| "latest".into())
}

/// 설정 창을 열 때 한 번 부른다. 에이전트마다 프로세스 하나와 요청 하나라 나란히 돌린다.
#[tauri::command(async)]
pub(crate) fn agent_versions() -> Vec<AgentVersion> {
    let specs = [
        ("Claude Code", "claude", "@anthropic-ai/claude-code", claude_channel(), true, "claude update"),
        ("Codex", "codex", "@openai/codex", "latest".into(), false, "npm i -g @openai/codex@latest"),
        ("Gemini", "gemini", "@google/gemini-cli", "latest".into(), false, "npm i -g @google/gemini-cli@latest"),
    ];
    let handles: Vec<_> = specs
        .into_iter()
        .map(|(name, cmd, pkg, channel, can_update, update_cmd)| {
            std::thread::spawn(move || {
                let installed = installed_version(cmd);
                // 설치 안 된 걸 굳이 물어보지 않는다
                let latest = installed.as_ref().and_then(|_| latest_version(pkg, &channel));
                let update_available = match (&latest, &installed) {
                    (Some(l), Some(i)) => is_newer(l, i),
                    _ => false,
                };
                AgentVersion {
                    name: name.into(),
                    installed,
                    latest,
                    channel,
                    can_update,
                    update_cmd: update_cmd.into(),
                    update_available,
                }
            })
        })
        .collect();
    handles.into_iter().filter_map(|h| h.join().ok()).collect()
}

/// 클로드만 앱에서 올린다(`claude update`). 돌고 있는 세션은 그대로 두고, 새로 여는
/// 세션부터 새 판을 쓴다. 결과 문구를 그대로 돌려준다.
#[tauri::command(async)]
pub(crate) fn update_claude() -> Result<String, String> {
    let out = std::process::Command::new("cmd.exe")
        .args(["/c", "claude", "update"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() {
        Ok(text)
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { text } else { err })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_found_in_each_cli_format() {
        assert_eq!(parse_version("2.1.280 (Claude Code)").as_deref(), Some("2.1.280"));
        assert_eq!(parse_version("codex-cli 0.153.4\n").as_deref(), Some("0.153.4"));
        assert_eq!(parse_version("0.52.0").as_deref(), Some("0.52.0"));
        assert_eq!(parse_version("v1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(parse_version("command not found"), None);
    }

    /// 이 기기에서 실제로 읽어 본다. `cargo test -- --ignored real_versions --nocapture`
    #[test]
    #[ignore]
    fn real_versions() {
        for a in agent_versions() {
            eprintln!("{:<12} 설치 {:?}  최신 {:?} ({})", a.name, a.installed, a.latest, a.channel);
        }
    }

    #[test]
    fn newer_compares_numbers_not_text() {
        assert!(is_newer("0.156.1", "0.153.4"));
        assert!(is_newer("2.1.280", "2.1.99")); // 글자로 비교하면 틀린다
        assert!(!is_newer("2.1.280", "2.1.280"));
        assert!(!is_newer("2.1.267", "2.1.280")); // stable이 설치판보다 낮을 수 있다
    }
}
