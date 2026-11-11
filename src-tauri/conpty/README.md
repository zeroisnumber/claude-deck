# 함께 배포하는 ConPTY

`conpty.dll`과 `OpenConsole.exe`는 Windows Terminal 저장소(microsoft/terminal, MIT — `LICENSE`)의
ConPTY다. 설치하면 실행 파일 옆 `conpty\` 폴더에 놓이고, 앱이 시작할 때 직접 확인해서 불러온다
(`src/conpty.rs`). 그러면 portable-pty가 `conpty.dll`을 이름으로 부를 때 이미 떠 있는 이 모듈을 받아
윈도우에 내장된 conhost 대신 쓴다.

- 판: 1.25.2603.03002 (두 파일 모두 Microsoft Corporation 서명). VS Code의 node-pty
  (`resources/app/node_modules.asar.unpacked/node-pty/build/Release/conpty`)에서 그대로 가져왔다 —
  node-pty가 NuGet `Microsoft.Windows.Console.ConPTY`에서 받는 것과 같은 파일이다.
- SHA-256
  - `conpty.dll`      d0b0114c602ace5c4f15495df5af23a8f601c11f1f4c386f506ee85aefd99a6e
  - `OpenConsole.exe` bae5576d8b4300c1dd262b020808c694a5fb839fcb08abd474ea7f1c073dd906

## 왜 바꿨나 (2026-10-22 실측, `pty.rs`의 ignored 시험)

| | 내장 conhost | 이 ConPTY |
|---|---|---|
| 화면을 계속 다시 그리는 앱에 키 하나 → 앱이 받기까지 (`conpty_input_latency`) | 가벼운 화면 p50 7.6ms, 큰 화면 p90 14.6ms, 최대 15ms | p50 0.3ms, p90 0.4~2ms |
| 동기화 출력 프레임(`?2026h … ?2026l`, `conpty_sync_output`) | 괄호가 먼저 나가고 본문은 약 20ms 뒤 괄호 밖으로 — xterm이 반쯤 그린 화면을 보인다 | 본문이 괄호 안에 함께 온다 |

## 무엇이 붙었는지 아는 법

말없이 내장으로 물러나는 일이 없게 세 군데에 드러난다.

- 설정 창 "새 ConPTY 사용" 아래 한 줄 — 설정값이 아니라 지금 실제로 붙어 있는 것
- 진단 기록의 `conpty` 줄 — 시작할 때 한 번, 탭을 열 때마다. 설치가 망가져 내장으로 물러난
  경우는 기록이 꺼져 있어도 남는다
- 설치 파일이 망가져 못 쓰면 시작할 때 알림이 뜨고, 누르면 지금 판을 다시 설치한다

0.5.28은 파일을 실행 파일 바로 옆에 두고 portable-pty가 알아서 찾게 했다. 그 판에서 실제로 있었던 일:
설치 프로그램이 파일을 쓴 지 수십 초 안에 PC가 꺼졌고(블루스크린), 다시 켜 보니 두 파일이 크기만
맞고 내용은 전부 0이었다. 불러오기는 조용히 실패했고 앱은 2주 넘게 내장으로 돌았다. 그래서:

- 불러오기 전에 PE 체크섬으로 파일 전체를 확인한다 (0으로 채워진 것, 앞부분만 기록된 것)
- 시작할 때 이 파일들을 디스크로 밀어낸다(`flush_install_files`) — 업데이트는 설치가 끝나자마자
  앱을 다시 띄우므로, 그 직후에 PC가 꺼져도 남는다
- 설정에서 끌 수 있다(데이터 폴더의 `INBOX_CONPTY` 표시 파일, 다음 실행부터). 문제가 의심될 때
  판을 내리지 않고 내장과 비교할 수 있다

## 주의

- 시작할 때 터미널의 장치 속성 질의(DA1, `ESC [ c`)에 답을 받을 때까지 자식을 세워 둔다. 답이
  없으면 3초 뒤에 띄운다. 앱에서는 xterm이 곧바로 답하므로 첫 출력이 내장과 같은 50ms 안팎이다
  (`DECK_ANSWER_DA1=1`로 `conpty_first_output`). 그래서 PTY를 띄우기 전에 그 탭의 xterm이 먼저
  있어야 한다 — 지금 모든 실행 경로가 `makeTerm` 뒤에 `spawnInto`를 부른다.
- 열린 탭마다 `OpenConsole.exe`가 떠 있다. 설치·제거 프로그램이 덮어쓰거나 지우기 전에
  설치 폴더의 것만 끈다(`installer-hooks.nsh`).

## 판을 올릴 때

1. 두 파일을 바꾸고 위 판·SHA-256을 고친다. `cargo test`의 `bundled_files_pass_the_integrity_check`가
   파일에 체크섬이 들어 있는지 본다(없는 빌드면 불러오기 전 확인을 다른 방식으로 바꿔야 한다).
2. `pty.rs`의 ignored 시험을 내장과 이 ConPTY 양쪽으로 돌린다 — 환경 변수로 이 폴더를 가리키면
   이쪽으로 돈다: `CLI_DECK_CONPTY_DIR=<저장소>\src-tauri\conpty cargo test -- --ignored conpty_ --nocapture`
3. 실제 앱으로 본다: `npx tauri build --no-bundle` 뒤 `python tools/app-smoke.py`와 `--inbox`.
