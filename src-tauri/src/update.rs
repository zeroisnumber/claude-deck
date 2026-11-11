// 앱 자동 업데이트 — 안정/베타 채널.
//
// JS 쪽 updater.check()는 확인할 주소를 바꿀 수 없다(헤더·타임아웃·프록시뿐). 채널을
// 고르려면 Rust의 updater_builder().endpoints(..)를 써야 하고, 그러면 찾은 Update도
// Rust에 있으니 설치까지 여기서 한다.
//
// 안정: tauri.conf.json의 releases/latest/download/latest.json. GitHub의 "latest"는
//       프리릴리스를 건너뛰므로 베타 릴리스가 안정 사용자에게 가지 않는다.
// 베타: 늘 같은 자리에 있는 beta-channel 릴리스의 latest.json. 릴리스 워크플로가 베타와
//       안정 둘 다 나올 때 더 높은 판으로 덮어쓴다 — 베타 사용자도 안정판을 받는다.
use std::sync::Mutex;

use tauri::{AppHandle, State};
use tauri_plugin_updater::{Update, UpdaterExt};

pub const BETA_ENDPOINT: &str =
    "https://github.com/zeroisnumber/claude-deck/releases/download/beta-channel/latest.json";

/// 마지막으로 찾은 업데이트. 설치 버튼이 이것을 받는다.
#[derive(Default)]
pub struct PendingUpdate(Mutex<Option<Update>>);

/// 새 판이 있으면 그 버전 문자열, 없으면 None.
/// repair=true면 같은 판이어도 받는다 — 설치 폴더의 파일이 망가졌을 때(conpty.rs) 지금 판을
/// 다시 설치해 고치는 길이다. 설치 프로그램이 모든 파일을 새로 쓴다.
#[tauri::command]
pub async fn check_update(
    app: AppHandle,
    pending: State<'_, PendingUpdate>,
    beta: bool,
    repair: Option<bool>,
) -> Result<Option<String>, String> {
    // 격리 실행(시험)은 업데이트를 보지도 받지도 않는다
    if crate::paths::sandbox().is_some() {
        return Ok(None);
    }
    let exit_app = app.clone();
    // 설치 프로그램을 띄운 플러그인은 곧바로 프로세스를 끝낸다(창 닫기를 거치지 않는다).
    // 그 직전에 열린 탭의 프로세스를 창을 닫을 때와 똑같이 정리한다. 이 고리는 플러그인이 기본으로
    // 걸어 둔 것(cleanup_before_exit — 트레이 아이콘을 거두고 창을 숨긴다)을 갈아 끼우므로 그것도
    // 여기서 부른다. 빼먹으면 죽은 트레이 아이콘이 남고 다시 뜬 앱이 하나를 더 올린다.
    let mut builder = app.updater_builder().on_before_exit(move || {
        crate::kill_all_ptys(&exit_app);
        exit_app.cleanup_before_exit();
    });
    if repair.unwrap_or(false) {
        // 더 낮은 판으로 내려가지는 않는다 (베타를 쓰다 채널을 바꾼 경우)
        builder = builder.version_comparator(|current, remote| remote.version >= current);
    }
    if beta {
        let url = BETA_ENDPOINT.parse().map_err(|e| format!("{e}"))?;
        builder = builder.endpoints(vec![url]).map_err(|e| e.to_string())?;
    }
    let found = builder
        .build()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    let version = found.as_ref().map(|u| u.version.clone());
    *pending.0.lock().unwrap_or_else(|e| e.into_inner()) = found;
    Ok(version)
}

/// check_update가 찾아 둔 판을 받아 설치하고 다시 띄운다.
/// Windows에서는 설치 프로그램을 띄운 뒤 플러그인이 앱을 끝내므로 restart까지 안 올 수 있다.
#[tauri::command]
pub async fn install_update(app: AppHandle, pending: State<'_, PendingUpdate>) -> Result<(), String> {
    let update = pending
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or("설치할 업데이트가 없습니다 — 먼저 확인하세요")?;
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    app.restart();
}

#[cfg(test)]
mod tests {
    #[test]
    fn beta_endpoint_is_a_valid_https_url() {
        let u: tauri::Url = super::BETA_ENDPOINT.parse().unwrap();
        assert_eq!(u.scheme(), "https");
        assert!(u.path().ends_with("/beta-channel/latest.json"));
    }
}
