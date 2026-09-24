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
    /// 앱에서 바로 올릴 수 있는가 — 클로드는 자체 명령, codex·gemini는 npm 전역 설치일 때만
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

/// cmd.exe로 돌리고 끝나기를 기다린다. 출력은 따로 읽는다 — 안 읽으면 출력이 파이프
/// 버퍼(약 4KB)를 넘을 때 자식이 쓰다 멈춰 영영 안 끝난다(npm은 출력이 많다).
fn run(args: &[&str], secs: u64) -> Option<(bool, String, String)> {
    use std::io::Read as _;
    let mut child = std::process::Command::new("cmd.exe")
        .arg("/c")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let read = |mut r: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = r.read_to_end(&mut b);
            String::from_utf8_lossy(&b).into_owned()
        })
    };
    let out = read(Box::new(child.stdout.take()?));
    let err = read(Box::new(child.stderr.take()?));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100))
            }
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    };
    Some((status.success(), out.join().unwrap_or_default(), err.join().unwrap_or_default()))
}

/// npm 전역 설치 폴더 (%APPDATA%\npm 같은 곳)
fn npm_prefix() -> Option<String> {
    let (ok, out, _) = run(&["npm", "prefix", "-g"], 10)?;
    let p = out.trim().to_string();
    (ok && !p.is_empty()).then_some(p)
}

/// 이 CLI가 npm 전역으로 깔려 있는가. PATH에서 먼저 잡히는 실행 파일이 npm 전역 폴더
/// 안이면 그렇다. 다른 길(설치 프로그램, winget 등)로 깐 것을 npm으로 올리면 두 벌이
/// 생기거나 PATH 순서 때문에 안 올라간 것처럼 보이므로, 확실할 때만 앱이 올린다.
fn installed_via_npm(cmd: &str, prefix: &str) -> bool {
    let Some((true, out, _)) = run(&["where", cmd], 5) else { return false };
    let first = out.lines().next().unwrap_or("").trim().to_lowercase();
    !first.is_empty() && first.starts_with(&prefix.trim_end_matches('\\').to_lowercase())
}

fn installed_version(cmd: &str) -> Option<String> {
    use std::io::Read as _;
    // npm 전역 설치는 .cmd 껍데기라 cmd.exe를 거쳐야 PATH에서 찾는다.
    // 처음 실행이라 뭔가를 물어보며 멈추는 CLI도 있다 — 설정 창이 영영 "확인 중"에
    // 머물지 않게 5초 안에 답이 없으면 끊는다.
    let mut child = std::process::Command::new("cmd.exe")
        .args(["/c", cmd, "--version"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50))
            }
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    parse_version(&out)
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
    let prefix = npm_prefix();
    let specs = [
        ("Claude Code", "claude", "@anthropic-ai/claude-code", claude_channel(), "claude update"),
        ("Codex", "codex", "@openai/codex", "latest".into(), "npm i -g @openai/codex@latest"),
        ("Gemini", "gemini", "@google/gemini-cli", "latest".into(), "npm i -g @google/gemini-cli@latest"),
    ];
    let handles: Vec<_> = specs
        .into_iter()
        .map(|(name, cmd, pkg, channel, update_cmd)| {
            let prefix = prefix.clone();
            std::thread::spawn(move || {
                let installed = installed_version(cmd);
                let can_update = cmd == "claude"
                    || (installed.is_some() && prefix.as_deref().map(|p| installed_via_npm(cmd, p)).unwrap_or(false));
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

/// 에이전트 CLI를 올린다. 클로드는 자체 명령(`claude update`), codex·gemini는 npm 전역
/// 설치일 때만(agent_versions가 can_update로 알려 준다) `npm i -g <패키지>@latest`.
/// 돌고 있는 세션은 그대로 두고 새로 여는 세션부터 새 판을 쓴다. 받는 데 시간이 걸려 5분까지 기다린다.
#[tauri::command(async)]
pub(crate) fn update_agent(name: String) -> Result<String, String> {
    let args: &[&str] = match name.as_str() {
        "Claude Code" => &["claude", "update"],
        "Codex" => &["npm", "i", "-g", "@openai/codex@latest"],
        "Gemini" => &["npm", "i", "-g", "@google/gemini-cli@latest"],
        _ => return Err(format!("모르는 에이전트: {name}")),
    };
    let (ok, out, err) = run(args, 300).ok_or("5분 안에 끝나지 않아 멈췄습니다")?;
    if ok {
        return Ok(out.trim().lines().last().unwrap_or("").to_string());
    }
    // 돌고 있는 CLI의 실행 파일은 윈도우가 잠가서 npm이 못 바꾼다
    if err.contains("EBUSY") || err.contains("EPERM") {
        return Err(format!("{name}이(가) 실행 중이라 바꿀 수 없습니다 — 그 탭을 닫고 다시 시도하세요"));
    }
    let msg = if err.trim().is_empty() { out } else { err };
    // 마지막 몇 줄에 이유가 있다 (npm은 앞에 진행 표시를 길게 쓴다)
    let lines: Vec<&str> = msg.trim().lines().collect();
    Err(lines[lines.len().saturating_sub(3)..].join("\n"))
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
            eprintln!("{:<12} 설치 {:?}  최신 {:?} ({})  앱이 올림 {}", a.name, a.installed, a.latest, a.channel, a.can_update);
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
