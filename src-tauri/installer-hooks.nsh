; 완전 삭제(제어판/설치 관리자 "제거") 시 앱이 남긴 데이터도 함께 정리한다.
; 업데이터의 조용한 재설치 경로는 이 훅을 타지 않고 실행 파일만 덮어쓰므로
; 일반 업데이트로는 설정이 지워지지 않는다.
;
; 앱이 실제로 쓰는 위치 (2026-07-18 기준 확인):
;   %LOCALAPPDATA%\com.user.claude-deck  — WebView2 프로필(localStorage 등) + crash.log
;   %APPDATA%\com.user.claude-deck       — tauri-plugin-window-state의 .window-state.json
;                                           (app_config_dir는 Windows에서 Roaming APPDATA로 매핑됨)
;
; 경로는 일부러 리터럴로 고정한다 (템플릿 변수를 잘못 짚으면 빈 문자열이 되어
; "$LOCALAPPDATA\"를 통째로 지우는 사고로 이어질 수 있음).
!macro NSIS_HOOK_POSTUNINSTALL
  RMDir /r "$LOCALAPPDATA\com.user.claude-deck"
  RMDir /r "$APPDATA\com.user.claude-deck"
!macroend
