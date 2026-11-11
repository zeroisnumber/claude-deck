// 함께 배포하는 ConPTY를 직접 확인하고 불러온다 (conpty/README.md).
//
// portable-pty는 첫 PTY를 열 때 `conpty.dll`을 이름으로 불러 보고, 안 되면 말없이 윈도우 내장
// (kernel32 → conhost.exe)으로 간다. 0.5.28은 파일을 실행 파일 옆에 두고 그 동작에 맡겼는데,
// 설치 직후 PC가 꺼져 파일이 0으로 채워진 설치본에서 2주 넘게 아무 표시 없이 내장으로 돌았다
// (2026-11 실측: 크기는 맞고 내용이 전부 0). 그래서 여기서 한다:
//   - 파일이 온전한지 먼저 본다(PE 체크섬 — 반만 기록된 파일은 불러오면 실행 중에 죽는다)
//   - 전체 경로로 직접 불러온다. 같은 이름의 모듈이 이미 떠 있으면 portable-pty의
//     LoadLibraryW("conpty.dll")는 그 모듈을 받는다(로더는 이름만 준 요청에 이미 뜬 모듈을 먼저 본다)
//   - 무엇이 붙었고 왜인지 남긴다(app_status → 설정 창·손상 알림, 진단 기록의 conpty 줄)
//   - 끌 수 있다(데이터 폴더의 INBOX_CONPTY 표시 파일). 문제가 의심될 때 판을 내리지 않고 내장으로.
// 파일은 실행 파일 옆 conpty\ 폴더에 둔다 — 옆에 바로 두면 끈 상태에서도 portable-pty가 찾아낸다.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const DLL: &str = "conpty.dll";
const HOST: &str = "OpenConsole.exe";
const OFF_MARKER: &str = "INBOX_CONPTY";

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct ConptyStatus {
    /// 함께 배포한 것이 붙었는가 (아니면 윈도우 내장)
    pub(crate) bundled: bool,
    /// 내장인 이유: "off"(설정에서 끔) · "missing" · "damaged" · "load-failed"
    pub(crate) reason: String,
    /// 사람이 읽을 덧붙임 (어느 파일이, 무슨 오류로)
    pub(crate) detail: String,
    /// 붙은 conpty.dll의 경로
    pub(crate) path: String,
}

impl ConptyStatus {
    fn inbox(reason: &str, detail: String) -> Self {
        ConptyStatus { bundled: false, reason: reason.into(), detail, path: String::new() }
    }
    /// 설치가 망가져서 내장으로 물러난 것인가 (사용자가 끈 것은 아니다)
    pub(crate) fn damaged(&self) -> bool {
        !self.bundled && self.reason != "off"
    }
}

static STATUS: OnceLock<ConptyStatus> = OnceLock::new();

/// 처음 부를 때 한 번 불러온다. 첫 PTY를 열기 전에 불려야 한다 — 시작할 때(main)와
/// PTY를 여는 자리(spawn_pty) 양쪽에서 부른다.
pub(crate) fn ensure() -> &'static ConptyStatus {
    STATUS.get_or_init(|| {
        flush_install_files(); // 불러오기 전에 — 떠 있는 DLL은 쓰기로 못 연다
        let status = load();
        let line = if status.bundled {
            status.path.clone()
        } else {
            format!("inbox ({} {})", status.reason, status.detail.trim()).replace(" )", ")")
        };
        // 설치가 망가져 물러난 것은 진단 기록이 꺼져 있어도 남긴다 — 나중에 무슨 일이었는지 볼 수 있게
        if status.damaged() {
            crate::diag::trace_always("app", "", "conpty", &line);
        } else {
            crate::diag::trace("app", "", "conpty", &line);
        }
        status
    })
}

/// 함께 배포한 파일이 있는 폴더. 시험과 개발 빌드는 CLI_DECK_CONPTY_DIR로 저장소의 것을 가리킨다.
fn bundled_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLI_DECK_CONPTY_DIR") {
        return Some(PathBuf::from(dir));
    }
    Some(crate::paths::exe_dir()?.join("conpty"))
}

fn off_marker() -> Option<PathBuf> {
    Some(crate::paths::data_dir()?.join(OFF_MARKER))
}

/// 설정에서 꺼 두었는가. 불러오는 건 프로세스에 한 번이라 바꾸면 다음 실행부터 적용된다.
pub(crate) fn disabled_by_user() -> bool {
    off_marker().map(|p| p.exists()).unwrap_or(false)
}

pub(crate) fn set_enabled(enabled: bool) -> Result<(), String> {
    let p = off_marker().ok_or("데이터 폴더를 찾을 수 없습니다")?;
    if enabled {
        match std::fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    } else {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&p, b"").map_err(|e| e.to_string())
    }
}

fn load() -> ConptyStatus {
    // 0.5.28이 실행 파일 옆에 바로 둔 파일을 치운다. 남아 있으면 꺼 둔 상태에서도 portable-pty가 찾는다.
    if let (Some(exe_dir), None) = (crate::paths::exe_dir(), std::env::var_os("CLI_DECK_CONPTY_DIR")) {
        remove_legacy_files(&exe_dir);
    }
    if disabled_by_user() {
        return ConptyStatus::inbox("off", String::new());
    }
    let Some(dir) = bundled_dir() else {
        return ConptyStatus::inbox("missing", "실행 파일 위치를 알 수 없음".into());
    };
    for name in [DLL, HOST] {
        match check_file(&dir.join(name)) {
            FileState::Ok => {}
            FileState::Missing => return ConptyStatus::inbox("missing", name.into()),
            FileState::Damaged(why) => return ConptyStatus::inbox("damaged", format!("{name}: {why}")),
        }
    }
    let dll = dir.join(DLL);
    match load_library(&dll) {
        Ok(()) => ConptyStatus {
            bundled: true,
            reason: String::new(),
            detail: String::new(),
            path: dll.to_string_lossy().into_owned(),
        },
        Err(e) => ConptyStatus::inbox("load-failed", e),
    }
}

/// 0.5.28이 실행 파일 바로 옆에 둔 두 파일의 헤더 체크섬 (ConPTY 1.25.2603.03002).
const LEGACY_FILES: [(&str, u32); 2] = [(DLL, 0x0002_48ff), (HOST, 0x0010_8e8d)];

/// 실행 파일 바로 옆의 conpty.dll·OpenConsole.exe 가운데 우리가 둔 것만 지운다: 헤더가 0.5.28이
/// 설치한 그 파일이라고 말하는 것(내용이 온전하든 반만 기록됐든), 그리고 실행 파일이 아니게 망가진
/// 것(설치 직후 0으로 채워진 것). 지금 함께 배포하는 파일이 멀쩡한지와는 상관없이 가린다 — 그쪽이
/// 망가진 날에도 옛 쌍을 반만 지워 짝 없는 DLL을 남기지 않게.
/// 같은 폴더에 다른 프로그램이 자기 것을 두었다면(다른 판의 파일) 손대지 않는다.
pub(crate) fn remove_legacy_files(exe_dir: &Path) {
    for (name, checksum) in LEGACY_FILES {
        let old = exe_dir.join(name);
        let Ok(bytes) = std::fs::read(&old) else { continue };
        let ours = match pe_checksum(&bytes) {
            None => true,
            Some((stored, _)) => stored == checksum,
        };
        if ours {
            let _ = std::fs::remove_file(&old);
        }
    }
    let _ = std::fs::remove_file(exe_dir.join("conpty-LICENSE.txt"));
}

#[derive(Debug, PartialEq)]
pub(crate) enum FileState {
    Ok,
    Missing,
    Damaged(String),
}

/// 실행 파일이 온전한가. 설치 직후 PC가 꺼지면 파일이 크기만 맞고 0으로 채워지거나(실측) 앞부분만
/// 기록될 수 있다. 두 파일 모두 헤더에 체크섬이 들어 있어(마이크로소프트 서명본) 전체를 대조한다.
pub(crate) fn check_file(path: &Path) -> FileState {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FileState::Missing,
        Err(e) => return FileState::Damaged(e.to_string()),
    };
    match pe_checksum(&bytes) {
        None => FileState::Damaged("실행 파일 형식이 아님 (내용이 비었거나 깨짐)".into()),
        Some((stored, _)) if stored == 0 => FileState::Damaged("체크섬 없음".into()),
        Some((stored, actual)) if stored != actual => FileState::Damaged("체크섬 불일치 (일부만 기록됨)".into()),
        Some(_) => FileState::Ok,
    }
}

/// (헤더에 적힌 체크섬, 실제로 계산한 체크섬). PE 파일이 아니면 None.
/// 16비트 단위로 접어 더한 값에 파일 길이를 더한다 — 체크섬 칸 자신은 0으로 친다.
pub(crate) fn pe_checksum(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 0x40 || &bytes[..2] != b"MZ" {
        return None;
    }
    let lfanew = u32::from_le_bytes(bytes[0x3c..0x40].try_into().ok()?) as usize;
    let field = lfanew.checked_add(88)?; // 서명 4 + 파일 헤더 20 + 선택 헤더 안의 CheckSum 64
    if bytes.len() < field + 4 || &bytes[lfanew..lfanew + 4] != b"PE\0\0" {
        return None;
    }
    let stored = u32::from_le_bytes(bytes[field..field + 4].try_into().ok()?);
    let mut sum: u32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let word = if i == field || i == field + 2 {
            0
        } else {
            u16::from_le_bytes([bytes[i], *bytes.get(i + 1).unwrap_or(&0)]) as u32
        };
        sum += word;
        sum = (sum & 0xffff) + (sum >> 16);
        i += 2;
    }
    sum = (sum & 0xffff) + (sum >> 16);
    Some((stored, sum.wrapping_add(bytes.len() as u32)))
}

/// 설치한 직후 PC가 꺼져도 남게, 방금 기록된 파일을 디스크로 밀어낸다. 설치 프로그램은 쓰고
/// 바로 끝나는데, 윈도우는 그 내용을 몇 초~수십 초 뒤에야 디스크에 쓴다. 업데이트는 설치가 끝나자마자
/// 앱을 다시 띄우므로 여기서 한 번 밀어 주면 그 틈이 닫힌다. 쓰기로 못 여는 파일(지금 실행 중인
/// 자기 자신, 권한 없는 설치 폴더)은 건너뛴다. 기록할 것이 없으면 곧바로 돌아온다.
pub(crate) fn flush_install_files() {
    let Some(exe_dir) = crate::paths::exe_dir() else { return };
    let mut files = vec![exe_dir.join("uninstall.exe")];
    if let Some(dir) = bundled_dir() {
        files.push(dir.join(DLL));
        files.push(dir.join(HOST));
    }
    for p in files {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&p) {
            let _ = f.sync_all();
        }
    }
}

/// 설치 폴더에서 망가진 파일들 (없으면 빈 목록). 손상 알림과 "다시 설치"가 이걸 본다.
/// 개발 빌드(설치 프로그램을 거치지 않음)에서 파일이 없는 것은 손상으로 치지 않는다.
pub(crate) fn install_damage(status: &ConptyStatus) -> Vec<String> {
    let mut out = Vec::new();
    if status.damaged() && !(cfg!(debug_assertions) && status.reason == "missing") {
        out.push(format!("ConPTY ({})", if status.detail.is_empty() { &status.reason } else { &status.detail }));
    }
    // 제거 프로그램은 설치할 때 만들어진다. 체크섬이 없는 파일이라 머리만 본다.
    if let Some(un) = crate::paths::exe_dir().map(|d| d.join("uninstall.exe")) {
        if let Ok(mut f) = std::fs::File::open(&un) {
            use std::io::Read as _;
            let mut head = [0u8; 2];
            if f.read_exact(&mut head).is_err() || &head != b"MZ" {
                out.push("uninstall.exe".into());
            }
        }
    }
    out
}

fn load_library(dll: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::FreeLibrary;
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_WITH_ALTERED_SEARCH_PATH,
    };
    let wide: Vec<u16> = dll.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let module = LoadLibraryExW(wide.as_ptr(), std::ptr::null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH);
        if module.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // portable-pty는 이 셋이 다 있어야 쓴다. 하나라도 없으면 말없이 내장으로 가는데, 그러면
        // 불러 놓고도 안 쓰는 상태가 된다 — 여기서 확인하고 그 경우엔 내려놓는다.
        for name in [&b"CreatePseudoConsole\0"[..], b"ResizePseudoConsole\0", b"ClosePseudoConsole\0"] {
            if GetProcAddress(module, name.as_ptr()).is_none() {
                FreeLibrary(module);
                return Err(format!("{} 없음", String::from_utf8_lossy(&name[..name.len() - 1])));
            }
        }
    }
    Ok(())
}

/// 지금 프로세스에 떠 있는 conpty.dll의 경로. 없으면(내장) None.
pub(crate) fn loaded_module() -> Option<String> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
    let name: Vec<u16> = "conpty.dll\0".encode_utf16().collect();
    unsafe {
        let module = GetModuleHandleW(name.as_ptr());
        if module.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let n = GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) as usize;
        Some(String::from_utf16_lossy(&buf[..n]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_file(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("conpty").join(name)
    }

    /// 저장소에 든 두 파일은 온전하다 — 판을 올릴 때 잘못 들어간 파일을 여기서 잡는다.
    #[test]
    fn bundled_files_pass_the_integrity_check() {
        assert_eq!(check_file(&repo_file(DLL)), FileState::Ok);
        assert_eq!(check_file(&repo_file(HOST)), FileState::Ok);
    }

    /// 실제로 겪은 손상(크기는 맞고 전부 0)과 반만 기록된 경우를 불러오기 전에 걸러 낸다.
    #[test]
    fn zero_filled_and_half_written_files_are_caught() {
        let dir = std::env::temp_dir().join(format!("deck-conpty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = std::fs::read(repo_file(DLL)).unwrap();

        let zeros = dir.join("zeros.dll");
        std::fs::write(&zeros, vec![0u8; good.len()]).unwrap();
        assert!(matches!(check_file(&zeros), FileState::Damaged(_)));

        let half = dir.join("half.dll");
        let mut b = good.clone();
        let from = b.len() / 2;
        b[from..].fill(0);
        std::fs::write(&half, &b).unwrap();
        assert!(matches!(check_file(&half), FileState::Damaged(_)));

        let flipped = dir.join("flipped.dll");
        let mut b = good.clone();
        b[0x3000] ^= 0x01;
        std::fs::write(&flipped, &b).unwrap();
        assert!(matches!(check_file(&flipped), FileState::Damaged(_)));

        assert_eq!(check_file(&dir.join("none.dll")), FileState::Missing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 옛 자리(실행 파일 바로 옆)의 파일은 우리 것만 치운다: 0.5.28이 둔 같은 파일, 0으로 채워진 파일.
    /// 다른 프로그램의 멀쩡한 conpty.dll은 남긴다.
    #[test]
    fn legacy_cleanup_only_removes_our_own_files() {
        let dir = std::env::temp_dir().join(format!("deck-legacy-{}", std::process::id()));
        let bundled = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("conpty");
        std::fs::create_dir_all(&dir).unwrap();
        let good = std::fs::read(bundled.join(DLL)).unwrap();

        // 저장소의 파일이 0.5.28이 설치한 그 판(1.25.2603)인 동안 유효한 시험이다. 판을 올리면
        // 아래 "0.5.28이 둔 것"은 그 판의 파일을 따로 구해 와야 한다 — 그때는 이 줄에서 멈춘다.
        assert_eq!(pe_checksum(&good).unwrap().0, LEGACY_FILES[0].1, "함께 배포하는 판이 바뀌었다");

        std::fs::write(dir.join(DLL), &good).unwrap();                       // 0.5.28이 둔 것
        std::fs::write(dir.join(HOST), vec![0u8; 1000]).unwrap();            // 0으로 채워진 것
        std::fs::write(dir.join("conpty-LICENSE.txt"), "x").unwrap();
        remove_legacy_files(&dir);
        assert!(!dir.join(DLL).exists() && !dir.join(HOST).exists() && !dir.join("conpty-LICENSE.txt").exists());

        // 반만 기록된 우리 파일(헤더는 온전, 뒤는 0)도 치운다 — 남으면 꺼 둔 상태에서 불려 온다
        let mut half = good.clone();
        let from = half.len() / 2;
        half[from..].fill(0);
        std::fs::write(dir.join(DLL), &half).unwrap();
        remove_legacy_files(&dir);
        assert!(!dir.join(DLL).exists(), "반만 기록된 옛 파일이 남았다");

        // 다른 판의 멀쩡한 파일(여기서는 OpenConsole.exe를 conpty.dll 이름으로) — 남의 것이다
        let other = std::fs::read(bundled.join(HOST)).unwrap();
        std::fs::write(dir.join(DLL), &other).unwrap();
        remove_legacy_files(&dir);
        assert!(dir.join(DLL).exists(), "다른 프로그램의 파일을 지웠다");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_broken_install_counts_as_damage() {
        let off = ConptyStatus::inbox("off", String::new());
        assert!(!off.damaged());
        assert!(ConptyStatus::inbox("damaged", "conpty.dll: 체크섬 불일치".into()).damaged());
        let ok = ConptyStatus { bundled: true, reason: String::new(), detail: String::new(), path: "x".into() };
        assert!(!ok.damaged());
    }
}
