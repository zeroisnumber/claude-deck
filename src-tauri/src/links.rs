//! 터미널 안의 파일 경로를 Ctrl+클릭으로 연다.
//!
//! 화면 글자는 에이전트(또는 에이전트가 읽은 파일·웹 페이지)가 찍은 것이라 믿을 수 없다.
//! 그래서 "여는" 것만 하고 "실행"은 하지 않는다: 편집기(VS Code)가 있으면 무엇이든 거기서
//! 열고, 없으면 실행 파일·스크립트는 탐색기에서 위치만 보여 준다.
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

/// 화면에서 찾은 경로 후보 중 실제로 있는 것. 상대 경로는 탭의 폴더를 기준으로 본다.
/// 마우스를 올린 줄마다 불리므로 존재 여부만 본다.
#[tauri::command(async)]
pub(crate) fn resolve_links(cwd: String, candidates: Vec<String>) -> Vec<Option<String>> {
    let base = PathBuf::from(&cwd);
    candidates
        .iter()
        .take(32)
        .map(|c| {
            if c.is_empty() || c.len() > 400 || is_remote(c) {
                return None;
            }
            let p = Path::new(c);
            let full = if p.is_absolute() { p.to_path_buf() } else if cwd.is_empty() { return None } else { base.join(p) };
            full.exists().then(|| full.to_string_lossy().into_owned())
        })
        .collect()
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

#[tauri::command(async)]
pub(crate) fn open_link(path: String, line: Option<u32>, col: Option<u32>) -> Result<(), String> {
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
    if let Some(code) = editor() {
        let target = match (line, col) {
            (Some(l), Some(c)) => format!("{shown}:{l}:{c}"),
            (Some(l), None) => format!("{shown}:{l}"),
            _ => shown.clone(),
        };
        std::process::Command::new(code)
            .args(["-g", &target])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| e.to_string())?;
        return Ok(());
    }
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if ext.is_empty() || RUNNABLE.contains(&ext.as_str()) {
        // 실행하지 않고 탐색기에서 골라 둔 채로 보여 준다
        std::process::Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{shown}\""))
            .spawn()
            .map_err(|e| e.to_string())?;
    } else {
        std::process::Command::new("explorer.exe").arg(&shown).spawn().map_err(|e| e.to_string())?;
    }
    Ok(())
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
}
