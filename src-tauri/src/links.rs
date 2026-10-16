//! 터미널 안의 파일 경로를 Ctrl+클릭으로 연다.
//!
//! 화면 글자는 에이전트(또는 에이전트가 읽은 파일·웹 페이지)가 찍은 것이라 믿을 수 없다.
//! 그래서 "여는" 것만 하고 "실행"은 하지 않는다: 무엇으로 열지는 확장자별 설정(프런트)이
//! 정하고, 기본 프로그램으로 열면 실행되어 버리는 종류는 탐색기에서 위치만 보여 준다.
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 기본 연결 프로그램으로 열면 실행되어 버리는 확장자
const RUNNABLE: &[&str] = &[
    "exe", "com", "bat", "cmd", "ps1", "psm1", "psd1", "vbs", "vbe", "js", "jse", "wsf", "wsh",
    "msi", "msp", "msc", "scr", "lnk", "url", "hta", "cpl", "reg", "jar", "pif", "inf", "scf",
    "application", "appref-ms", "appx", "msix", "appinstaller", "py", "pyw", "pyz", "sh", "dll",
    "sys", "iso", "img", "vhd", "vhdx", "library-ms", "search-ms", "settingcontent-ms", "chm",
];

/// 화면에서 찾은 경로 후보 중 실제로 있는 것. 상대 경로는 탭의 폴더를 먼저 보고, 없으면
/// 그 폴더가 속한 프로젝트(.git이 있는 곳)의 맨 위를 본다 — 에이전트는 하위 폴더에서
/// 돌아도 경로를 저장소 기준(src/main.rs)으로 찍는 일이 많다.
/// 마우스를 올린 줄마다 불리므로 존재 여부만 본다.
#[tauri::command(async)]
pub(crate) fn resolve_links(cwd: String, candidates: Vec<String>) -> Vec<Option<String>> {
    let base = PathBuf::from(&cwd);
    let root = if cwd.is_empty() { None } else { project_root(&base) };
    candidates
        .iter()
        .take(32)
        .map(|c| {
            if c.is_empty() || c.len() > 400 || is_remote(c) {
                return None;
            }
            let p = Path::new(c);
            if p.is_absolute() {
                return p.exists().then(|| p.to_string_lossy().into_owned());
            }
            if cwd.is_empty() {
                return None;
            }
            [Some(&base), root.as_ref()]
                .into_iter()
                .flatten()
                .map(|b| b.join(p))
                .find(|full| full.exists())
                .map(|full| tidy(&full))
        })
        .collect()
}

/// a\b\..\c 를 a\c 로 (편집기에 넘기고 보여 줄 때 읽기 좋게). 파일 시스템은 건드리지 않는다.
fn tidy(p: &Path) -> String {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out.to_string_lossy().into_owned()
}

/// 이 폴더가 속한 저장소의 맨 위(.git이 있는 곳). 그 폴더 자신이면 볼 필요가 없어 None.
/// 홈 폴더에 닿으면 멈춘다(홈 전체가 저장소인 설정은 기준으로 쓰지 않는다).
fn project_root(dir: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir();
    for d in dir.ancestors().take(10) {
        if Some(d) == home.as_deref() {
            return None;
        }
        if d.join(".git").exists() {
            return (d != dir).then(|| d.to_path_buf());
        }
    }
    None
}

/// \\server\share 같은 네트워크 경로. 화면에 찍힌 글자만으로 마우스를 올리기만 해도
/// 그 서버에 접속(윈도우 로그인 정보로 인증까지)하게 되므로 아예 보지 않는다.
fn is_remote(p: &str) -> bool {
    p.starts_with(r"\\") || p.starts_with("//") || p.starts_with(r"\/") || p.starts_with(r"/\")
}

/// PATH의 VS Code(또는 그 계열). code.cmd는 cmd.exe를 거쳐야 해서 파일 이름의 &·% 같은
/// 글자가 명령으로 풀릴 수 있다 — 옆의 실행 파일을 바로 띄운다.
fn editor() -> Option<&'static PathBuf> {
    static EDITOR: OnceLock<Option<PathBuf>> = OnceLock::new();
    EDITOR
        .get_or_init(|| {
            let out = std::process::Command::new("where")
                .arg("code.cmd")
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .ok()?;
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.lines().find_map(|l| {
                // ...\Microsoft VS Code\bin\code.cmd → ...\Microsoft VS Code\Code.exe
                // ...\cursor\resources\app\bin\code.cmd → ...\cursor\Cursor.exe
                Path::new(l.trim()).ancestors().skip(2).take(3).find_map(|dir| {
                    ["Code.exe", "Code - Insiders.exe", "Cursor.exe", "VSCodium.exe"]
                        .iter()
                        .map(|n| dir.join(n))
                        .find(|p| p.is_file())
                })
            })
        })
        .as_ref()
}

/// Rider. PATH에 없는 경우가 많아 설치 폴더를 직접 본다(여러 판이 있으면 가장 최근 것).
///   C:\Program Files\JetBrains\JetBrains Rider 2025.1.4\bin\rider64.exe   (설치 프로그램)
///   %LOCALAPPDATA%\Programs\Rider\bin\rider64.exe                        (Toolbox)
fn rider() -> Option<&'static PathBuf> {
    static RIDER: OnceLock<Option<PathBuf>> = OnceLock::new();
    RIDER
        .get_or_init(|| {
            let mut roots = Vec::new();
            if let Some(pf) = std::env::var_os("ProgramFiles") {
                roots.push(PathBuf::from(pf).join("JetBrains"));
            }
            if let Some(local) = dirs::data_local_dir() {
                roots.push(local.join("Programs"));
                roots.push(local.join("JetBrains").join("Toolbox").join("apps"));
            }
            let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
            for root in roots {
                let Ok(entries) = std::fs::read_dir(&root) else { continue };
                for e in entries.flatten() {
                    if !e.file_name().to_string_lossy().to_lowercase().contains("rider") {
                        continue;
                    }
                    let exe = e.path().join("bin").join("rider64.exe");
                    if let Ok(m) = exe.metadata() {
                        found.push((m.modified().unwrap_or(std::time::UNIX_EPOCH), exe));
                    }
                }
            }
            found.into_iter().max_by_key(|(t, _)| *t).map(|(_, p)| p)
        })
        .as_ref()
}

/// Visual Studio. 설치 위치는 vswhere가 안다.
fn visual_studio() -> Option<&'static PathBuf> {
    static VS: OnceLock<Option<PathBuf>> = OnceLock::new();
    VS.get_or_init(|| {
        let pf86 = std::env::var_os("ProgramFiles(x86)")?;
        let vswhere = PathBuf::from(pf86).join("Microsoft Visual Studio").join("Installer").join("vswhere.exe");
        let out = std::process::Command::new(vswhere)
            .args(["-latest", "-property", "productPath"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        p.is_file().then_some(p)
    })
    .as_ref()
}

/// 설정 창에 고를 수 있는 것만 보여 주려고
#[derive(serde::Serialize)]
pub(crate) struct Editors {
    vscode: bool,
    rider: bool,
    vs: bool,
}

#[tauri::command(async)]
pub(crate) fn list_editors() -> Editors {
    Editors { vscode: editor().is_some(), rider: rider().is_some(), vs: visual_studio().is_some() }
}

fn spawn(exe: &Path, args: &[&str]) -> Result<(), String> {
    std::process::Command::new(exe)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 실행하지 않고 탐색기에서 골라 둔 채로 보여 준다
fn reveal(shown: &str) -> Result<(), String> {
    std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{shown}\""))
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 연결된 기본 프로그램으로. 열면 실행되어 버리는 종류는 위치만 보여 준다.
fn open_default(p: &Path, shown: &str) -> Result<(), String> {
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if ext.is_empty() || RUNNABLE.contains(&ext.as_str()) {
        return reveal(shown);
    }
    std::process::Command::new("explorer.exe").arg(shown).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// how: 무엇으로 열지 — 프런트가 확장자별 설정에서 골라 넘긴다.
///   auto     VS Code가 있으면 거기서(줄 번호로), 없으면 기본 프로그램
///   default  기본 연결 프로그램 (이미지는 사진 앱 등)
///   vscode / rider / vs   그 편집기 (없으면 오류)
///   reveal   탐색기에서 위치만
#[tauri::command(async)]
pub(crate) fn open_link(path: String, line: Option<u32>, col: Option<u32>, how: Option<String>) -> Result<(), String> {
    if is_remote(&path) {
        return Err("네트워크 경로는 열지 않습니다".into());
    }
    // canonicalize는 연결된 네트워크 드라이브(Z:)를 \\?\UNC\...로 바꿔 편집기가 못 연다.
    // 경로는 resolve_links가 이미 확인한 것이라 절대 경로로만 만든다.
    let p = std::path::absolute(&path).map_err(|e| e.to_string())?;
    if !p.exists() {
        return Err(format!("파일이 없습니다: {path}"));
    }
    let shown = p.to_string_lossy().into_owned();
    if p.is_dir() {
        std::process::Command::new("explorer.exe").arg(&shown).spawn().map_err(|e| e.to_string())?;
        return Ok(());
    }
    let line_s = line.map(|l| l.to_string());
    let code_target = match (line, col) {
        (Some(l), Some(c)) => format!("{shown}:{l}:{c}"),
        (Some(l), None) => format!("{shown}:{l}"),
        _ => shown.clone(),
    };
    match how.as_deref().unwrap_or("auto") {
        "default" => open_default(&p, &shown),
        "reveal" => reveal(&shown),
        "vscode" => spawn(editor().ok_or("VS Code를 찾지 못했습니다")?, &["-g", &code_target]),
        "rider" => {
            let exe = rider().ok_or("Rider를 찾지 못했습니다")?;
            match &line_s {
                Some(l) => spawn(exe, &["--line", l, &shown]),
                None => spawn(exe, &[&shown]),
            }
        }
        // 줄 번호로 여는 인자는 없다 — 파일만 연다
        "vs" => spawn(visual_studio().ok_or("Visual Studio를 찾지 못했습니다")?, &["/Edit", &shown]),
        _ => match editor() {
            Some(code) => spawn(code, &["-g", &code_target]),
            None => open_default(&p, &shown),
        },
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn resolves_only_existing_paths() {
        let dir = std::env::temp_dir().join(format!("deck-links-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("main.rs"), "x").unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let abs = dir.join("src").join("main.rs").to_string_lossy().into_owned();
        let r = super::resolve_links(
            cwd,
            vec!["src/main.rs".into(), "src/nope.rs".into(), abs.clone(), "".into()],
        );
        assert!(r[0].as_deref().is_some_and(|p| p.ends_with("main.rs")), "{r:?}");
        assert_eq!(r[1], None);
        assert_eq!(r[2].as_deref(), Some(abs.as_str()));
        assert_eq!(r[3], None);
        assert_eq!(super::resolve_links(String::new(), vec![r"\\evil\share\a.txt".into()]), vec![None]);
        // 폴더 없이 상대 경로는 볼 수 없다
        assert_eq!(super::resolve_links(String::new(), vec!["src/main.rs".into()]), vec![None]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 하위 폴더에서 돌아도 저장소 기준 경로(ui/main.js)와 ../ 경로를 찾는다
    #[test]
    fn resolves_from_project_root_and_parent() {
        let dir = std::env::temp_dir().join(format!("deck-links-root-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("ui")).unwrap();
        std::fs::create_dir_all(dir.join("src").join("deep")).unwrap();
        std::fs::write(dir.join("ui").join("main.js"), "x").unwrap();
        let cwd = dir.join("src").join("deep").to_string_lossy().into_owned();
        let r = super::resolve_links(cwd, vec!["ui/main.js".into(), "../../ui/main.js".into(), "nope/x.js".into()]);
        let want = dir.join("ui").join("main.js").to_string_lossy().into_owned();
        assert_eq!(r[0].as_deref(), Some(want.as_str()), "저장소 기준");
        assert_eq!(r[1].as_deref(), Some(want.as_str()), "../ 는 정리해서 돌려준다");
        assert_eq!(r[2], None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 이 기기에 깔린 편집기를 찾는다. `cargo test -- --ignored real_editors --nocapture`
    #[test]
    #[ignore]
    fn real_editors() {
        eprintln!("vscode {:?}\nrider {:?}\nvs {:?}", super::editor(), super::rider(), super::visual_studio());
    }
}
