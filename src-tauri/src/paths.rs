// 앱이 읽고 쓰는 두 뿌리 — 사용자 홈(에이전트 기록: ~/.claude, ~/.codex, ~/.gemini)과
// 앱 데이터 폴더(%LOCALAPPDATA%\com.user.cli-deck: 진단 기록·붙여넣은 이미지·번역·WebView2 프로필).
//
// 격리 실행: 환경 변수 CLI_DECK_SANDBOX=<폴더>로 띄우면 둘 다 그 폴더 아래(home, data)로 옮겨
// 간다. 빌드한 실제 앱을 쓰던 앱 옆에서 띄워 시험하려는 것이다(tools/app-smoke.py). 그냥 한 번
// 더 띄우면 같은 프로필을 읽어 지난 탭을 되살리고, 열려 있는 세션에 프로세스가 하나씩 더 붙는다.
// 격리하면 세션 목록이 비어 되살릴 것이 없고, 진단 기록·창 위치도 건드리지 않는다.
// 한 스위치로 둘을 함께 옮긴다 — 반만 옮겨진 상태가 없게.

use std::path::PathBuf;
use std::sync::LazyLock;

pub(crate) const APP_ID: &str = "com.user.cli-deck";

static SANDBOX: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    let dir = std::env::var_os("CLI_DECK_SANDBOX")?;
    let dir = PathBuf::from(dir);
    // 상대 경로나 빈 값은 받지 않는다 — 어디에 쓰는지 모호하면 격리가 아니다
    dir.is_absolute().then_some(dir)
});

/// 격리 실행 중이면 그 폴더.
pub(crate) fn sandbox() -> Option<&'static PathBuf> {
    SANDBOX.as_ref()
}

/// 에이전트 기록이 있는 홈.
pub(crate) fn home_dir() -> Option<PathBuf> {
    match sandbox() {
        Some(dir) => Some(dir.join("home")),
        None => dirs::home_dir(),
    }
}

/// 앱 데이터 폴더. WebView2 프로필(EBWebView — 설정이 든 localStorage)도 이 안에 있다.
pub(crate) fn data_dir() -> Option<PathBuf> {
    // 시험(cargo test)은 쓰고 있는 앱의 데이터 폴더를 건드리지 않는다. 시험이 진단 기록에 한 줄만
    // 남겨도 기록 스레드가 "새 실행"으로 보고 쓰던 기록을 prev로 밀고 옛 prev를 지운다 —
    // 실제로 시험을 돌릴 때마다 사용 중인 앱의 기록이 밀려 나갔다.
    if cfg!(test) {
        return Some(std::env::temp_dir().join("cli-deck-test-data"));
    }
    match sandbox() {
        Some(dir) => Some(dir.join("data")),
        None => Some(dirs::data_local_dir()?.join(APP_ID)),
    }
}

/// 실행 파일이 있는 폴더 (함께 배포한 파일들의 자리).
pub(crate) fn exe_dir() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.to_path_buf())
}

#[cfg(test)]
mod tests {
    /// 시험이 쓰는 데이터 폴더는 진짜 앱의 것이 아니다 (진단 기록·표시 파일·캐시가 다 이 아래다).
    #[test]
    fn tests_never_use_the_real_app_data_folder() {
        let dir = super::data_dir().unwrap();
        assert!(dir.starts_with(std::env::temp_dir()), "{dir:?}");
        let real = dirs::data_local_dir().unwrap().join(super::APP_ID);
        assert_ne!(dir, real);
        assert_eq!(crate::diag::trace_log_path().unwrap().parent().unwrap(), dir);
    }
}
