; 완전 삭제(제어판/설치 관리자 "제거") 시 앱이 남긴 데이터도 함께 정리한다.
; 업데이터의 조용한 재설치 경로는 이 훅을 타지 않고 실행 파일만 덮어쓰므로
; 일반 업데이트로는 설정이 지워지지 않는다.
;
; 앱이 실제로 쓰는 위치 (2026-07-18 기준 확인):
;   %LOCALAPPDATA%\com.user.cli-deck  — WebView2 프로필(localStorage 등) + crash.log
;   %APPDATA%\com.user.cli-deck       — tauri-plugin-window-state의 .window-state.json
;                                        (app_config_dir는 Windows에서 Roaming APPDATA로 매핑됨)
;
; 경로는 일부러 리터럴로 고정한다 (템플릿 변수를 잘못 짚으면 빈 문자열이 되어
; "$LOCALAPPDATA\"를 통째로 지우는 사고로 이어질 수 있음).
!macro NSIS_HOOK_POSTUNINSTALL
  RMDir /r "$LOCALAPPDATA\com.user.cli-deck"
  RMDir /r "$APPDATA\com.user.cli-deck"
!macroend

; 함께 배포하는 ConPTY(conpty/README.md)는 열린 탭마다 OpenConsole.exe를 띄운다. 앱이 꺼질 때
; 같이 끝나지만, 덜 끝난 것이 남아 있으면 설치 프로그램이 그 파일을 덮어쓰지 못해 멈춘다.
; 이 설치 폴더의 것만 끈다 — Windows Terminal·VS Code도 같은 이름의 프로세스를 띄운다.
; 이 훅은 "앱이 떠 있나" 확인보다 먼저 돈다.
; - 직접 설치를 실행했고 앱이 떠 있으면 손대지 않는다 — 여기서 끄면 그 확인 창에서 취소해도 열린
;   탭이 이미 죽어 있다.
; - 업데이트(/UPDATE)는 앱이 설치 프로그램을 띄운 직후 스스로 꺼진다. 그때 OpenConsole은 1~2초 더
;   남는다(openconsole_exits_with_its_pty 실측) — 앱이 꺼지기를(최대 15초), 그다음 OpenConsole이
;   스스로 끝나기를(최대 5초) 기다리고 남은 것만 끈다.
!macro DECK_STOP_OWN_OPENCONSOLE_NOW
  nsExec::Exec `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "if (-not (Get-Process claude-deck -ErrorAction SilentlyContinue | Where-Object { $$_.Path -eq '$INSTDIR\claude-deck.exe' })) { Get-Process OpenConsole -ErrorAction SilentlyContinue | Where-Object { $$_.Path -eq '$INSTDIR\OpenConsole.exe' } | Stop-Process -Force -ErrorAction SilentlyContinue }"`
  Pop $0
!macroend

!macro DECK_STOP_OWN_OPENCONSOLE_AFTER_APP
  nsExec::Exec `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$$app = '$INSTDIR\claude-deck.exe'; $$oc = '$INSTDIR\OpenConsole.exe'; $$end = (Get-Date).AddSeconds(15); while ((Get-Process claude-deck -ErrorAction SilentlyContinue | Where-Object { $$_.Path -eq $$app }) -and (Get-Date) -lt $$end) { Start-Sleep -Milliseconds 200 }; $$end = (Get-Date).AddSeconds(5); while ((Get-Process OpenConsole -ErrorAction SilentlyContinue | Where-Object { $$_.Path -eq $$oc }) -and (Get-Date) -lt $$end) { Start-Sleep -Milliseconds 200 }; Get-Process OpenConsole -ErrorAction SilentlyContinue | Where-Object { $$_.Path -eq $$oc } | Stop-Process -Force -ErrorAction SilentlyContinue"`
  Pop $0
!macroend

!macro NSIS_HOOK_PREINSTALL
  ${If} $UpdateMode = 1
    !insertmacro DECK_STOP_OWN_OPENCONSOLE_AFTER_APP
  ${Else}
    !insertmacro DECK_STOP_OWN_OPENCONSOLE_NOW
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro DECK_STOP_OWN_OPENCONSOLE_NOW
!macroend
