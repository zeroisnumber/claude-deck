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
    run_in(args, secs, None, None, false)
}

/// run과 같되, 작업 폴더와 표준 입력을 줄 수 있다. direct면 cmd.exe를 거치지 않고 첫
/// 인자를 바로 띄운다 — 따옴표·중괄호가 든 인자를 cmd.exe가 다시 풀어 헤치지 않게.
fn run_in(
    args: &[&str],
    secs: u64,
    cwd: Option<&std::path::Path>,
    input: Option<String>,
    direct: bool,
) -> Option<(bool, String, String)> {
    use std::io::{Read as _, Write as _};
    let mut cmd = if direct {
        let mut c = std::process::Command::new(args[0]);
        c.args(&args[1..]);
        c
    } else {
        let mut c = std::process::Command::new("cmd.exe");
        c.arg("/c").args(args);
        c
    };
    cmd.creation_flags(CREATE_NO_WINDOW)
        .stdin(if input.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let mut child = cmd.spawn().ok()?;
    if let Some(text) = input {
        // 쓰는 동안 자식이 출력을 못 비우면 서로 기다리므로 따로 쓴다
        if let Some(mut si) = child.stdin.take() {
            std::thread::spawn(move || {
                let _ = si.write_all(text.as_bytes());
            });
        }
    }
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

#[derive(Clone, Serialize)]
pub(crate) struct ChangeEntry {
    pub(crate) version: String,
    /// 마크다운 그대로 — 화면이 이스케이프한 뒤 그린다
    pub(crate) notes: String,
}

fn http_get(url: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        // GitHub API는 User-Agent가 없으면 거절한다
        .user_agent("cli-deck")
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("{} — {url}", resp.status()));
    }
    resp.text().map_err(|e| e.to_string())
}

/// 설치판 다음부터 최신판까지(둘 다 포함하지 않는 쪽: from < v <= to).
fn in_range(v: &str, from: &str, to: &str) -> bool {
    is_newer(v, from) && !is_newer(v, to)
}

/// 클로드: 저장소의 CHANGELOG.md가 "## 2.1.281" 제목으로 버전을 가른다.
fn claude_changes(from: &str, to: &str) -> Result<Vec<ChangeEntry>, String> {
    let text = http_get("https://raw.githubusercontent.com/anthropics/claude-code/main/CHANGELOG.md")?;
    let mut out = Vec::new();
    for sec in text.split("\n## ").skip(1) {
        let (head, body) = sec.split_once('\n').unwrap_or((sec, ""));
        let Some(v) = parse_version(head) else { continue };
        if in_range(&v, from, to) {
            out.push(ChangeEntry { version: v, notes: body.trim().to_string() });
        }
    }
    Ok(out)
}

/// codex·gemini: GitHub 릴리스 설명. 태그에 붙은 접두사(rust-v, v)는 떼고, 미리보기·
/// 알파·nightly는 건너뛴다(앱이 비교하는 채널은 latest다).
fn github_changes(repo: &str, from: &str, to: &str) -> Result<Vec<ChangeEntry>, String> {
    let mut out = Vec::new();
    // codex는 알파를 하루에도 여러 번 내서 한 쪽(100개)이 거의 알파다. 설치판보다 오래된
    // 정식판을 만날 때까지 세 쪽까지 넘긴다.
    for page in 1..=3 {
        let text = http_get(&format!("https://api.github.com/repos/{repo}/releases?per_page=100&page={page}"))?;
        let list: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let items = list.as_array().cloned().unwrap_or_default();
        let mut passed_from = false;
        for r in &items {
            if r["prerelease"] == true || r["draft"] == true {
                continue;
            }
            // 접두사(codex의 "rust-v", gemini의 "v")를 뗀 뒤에 앞판(-preview, -alpha)을 가린다
            let tag = r["tag_name"].as_str().unwrap_or("");
            let bare = tag.trim_start_matches("rust-").trim_start_matches('v');
            if bare.contains('-') {
                continue;
            }
            let Some(v) = parse_version(bare) else { continue };
            if !is_newer(&v, from) {
                passed_from = true;
            }
            if in_range(&v, from, to) {
                let notes = r["body"].as_str().unwrap_or("").trim().to_string();
                out.push(ChangeEntry { version: v, notes });
            }
        }
        if passed_from || items.len() < 100 {
            break;
        }
    }
    Ok(out)
}

/// 설치판(from)과 최신판(to) 사이의 변경 내용, 새 판부터. 네트워크 호출이라 별도
/// 스레드에서 한다(blocking reqwest를 tokio 작업 스레드에서 부르면 안 된다).
/// 너무 많이 뒤처졌으면 최근 15개만.
#[tauri::command(async)]
pub(crate) fn agent_changelog(name: String, from: String, to: String) -> Result<Vec<ChangeEntry>, String> {
    std::thread::spawn(move || {
        let mut list = match name.as_str() {
            "Claude Code" => claude_changes(&from, &to),
            "Codex" => github_changes("openai/codex", &from, &to),
            "Gemini" => github_changes("google-gemini/gemini-cli", &from, &to),
            _ => Err(format!("모르는 에이전트: {name}")),
        }?;
        list.sort_by(|a, b| {
            if is_newer(&a.version, &b.version) { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater }
        });
        list.truncate(15);
        Ok(list)
    })
    .join()
    .map_err(|_| "변경 내용을 읽다 멈췄습니다".to_string())?
}

/// 변경 내역 한 판을 한국어로. 이미 쓰고 있는 클로드(Haiku)에게 맡긴다 — 따로 키나 외부
/// 번역 서비스가 필요 없고 기술 문서 품질이 좋다. 한 번 번역한 판은 저장해 두고 다시 쓴다.
/// - --no-session-persistence: 번역마다 사이드바에 세션이 생기지 않게
/// - MCP를 띄우지 않는다: 띄우면 두 줄 번역에 14초, 안 띄우면 7초(실측)
/// - 앱 전용 폴더에서 돈다: 클로드가 작업 폴더마다 프로젝트 폴더를 만들므로 한 곳으로 모은다
/// 긴 판(수만 자)은 1분 넘게 걸릴 수 있어 3분까지 기다린다.
#[tauri::command(async)]
pub(crate) fn translate_changelog(name: String, version: String, text: String) -> Result<String, String> {
    let base = dirs::data_local_dir().ok_or("앱 폴더를 찾을 수 없습니다")?.join("com.user.cli-deck").join("translate");
    let cache = base.join("cache");
    fs::create_dir_all(&cache).map_err(|e| e.to_string())?;
    let safe: String = format!("{name}-{version}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    let file = cache.join(format!("{safe}.ko.md"));
    if let Ok(t) = fs::read_to_string(&file) {
        if !t.trim().is_empty() {
            return Ok(t);
        }
    }
    let prompt = "Translate this software changelog into natural Korean for a developer. \
                  Keep the markdown structure, code spans, command-line flags, file names, setting keys \
                  and product names exactly as they are. Output only the translation.";
    // 빈 MCP 설정은 파일로 넘긴다 — 명령줄에 JSON을 쓰면 따옴표 처리가 꼬일 수 있다
    let mcp = base.join("no-mcp.json");
    if !mcp.exists() {
        fs::write(&mcp, "{\"mcpServers\":{}}").map_err(|e| e.to_string())?;
    }
    let mcp = mcp.to_string_lossy().to_string();
    let args = ["claude", "-p", prompt, "--no-session-persistence", "--model", "haiku",
                "--strict-mcp-config", "--mcp-config", mcp.as_str()];
    // 네이티브 설치(claude.exe)는 바로 띄우고, npm 설치(claude.cmd)면 cmd.exe로 넘어간다
    // 바로 못 띄웠을 때(곧바로 실패)만 cmd.exe로 다시 — 시간이 다 돼 끊긴 걸 한 번 더 기다리지 않는다
    let t0 = std::time::Instant::now();
    let first = run_in(&args, 180, Some(&base), Some(text.clone()), true);
    let (ok, out, err) = match first {
        Some(r) => r,
        None if t0.elapsed() < std::time::Duration::from_secs(2) => {
            run_in(&args, 180, Some(&base), Some(text), false).ok_or("3분 안에 번역이 끝나지 않았습니다")?
        }
        None => return Err("3분 안에 번역이 끝나지 않았습니다".into()),
    };
    let out = out.trim().to_string();
    if !ok || out.is_empty() {
        let e = err.trim();
        return Err(if e.is_empty() { "클로드가 번역을 돌려주지 않았습니다".into() } else { e.lines().last().unwrap_or(e).to_string() });
    }
    let _ = fs::write(&file, &out);
    Ok(out)
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

    /// 실제 변경 내역을 받아 본다. `cargo test -- --ignored real_changelogs --nocapture`
    #[test]
    #[ignore]
    fn real_changelogs() {
        for (name, from, to) in [("Claude Code", "2.1.278", "2.1.281"), ("Claude Code", "", "2.1.281"), ("Codex", "0.153.4", "0.156.1"), ("Gemini", "0.52.0", "0.61.0")] {
            match agent_changelog(name.into(), from.into(), to.into()) {
                Ok(list) => {
                    let vs: Vec<_> = list.iter().map(|e| format!("{}({}자)", e.version, e.notes.chars().count())).collect();
                    eprintln!("{name}: {}", vs.join(" "));
                }
                Err(e) => eprintln!("{name}: 실패 {e}"),
            }
        }
    }

    /// 실제로 번역해 본다(클로드 Haiku, 구독 사용량이 조금 든다). 두 번째는 저장본이라 즉시.
    /// `cargo test -- --ignored real_translate --nocapture`
    #[test]
    #[ignore]
    fn real_translate() {
        let text = "- Added `\"attribution\": false` in `settings.json` to hide commit attribution
- Fixed a crash when resuming a session".to_string();
        for round in 0..2 {
            let t = std::time::Instant::now();
            let r = translate_changelog("Test".into(), format!("0.0.{}", std::process::id()), text.clone());
            eprintln!("{round}: {:.1}s {:?}", t.elapsed().as_secs_f64(), r);
        }
    }

    /// 범위는 설치판 다음부터 최신판까지다
    #[test]
    fn changelog_range_excludes_installed_and_includes_latest() {
        assert!(!in_range("2.1.278", "2.1.278", "2.1.281"));
        assert!(in_range("2.1.279", "2.1.278", "2.1.281"));
        assert!(in_range("2.1.281", "2.1.278", "2.1.281"));
        assert!(!in_range("2.1.282", "2.1.278", "2.1.281"));
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
