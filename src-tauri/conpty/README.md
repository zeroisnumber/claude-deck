# 함께 배포하는 ConPTY

`conpty.dll`과 `OpenConsole.exe`는 Windows Terminal 저장소(microsoft/terminal, MIT — `LICENSE`)의
ConPTY다. 설치하면 앱 실행 파일 옆에 놓이고, portable-pty가 `conpty.dll`을 이름으로 불러 윈도우에
내장된 conhost 대신 이쪽을 쓴다. 없으면 내장으로 돌아간다. 어느 쪽이 붙었는지는 진단 기록의
첫 `conpty` 줄에 남는다.

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

## 주의

- 시작할 때 터미널의 장치 속성 질의(DA1, `ESC [ c`)에 답을 받을 때까지 자식을 세워 둔다. 답이
  없으면 3초 뒤에 띄운다. 앱에서는 xterm이 곧바로 답하므로 첫 출력이 내장과 같은 50ms 안팎이다
  (`DECK_ANSWER_DA1=1`로 `conpty_first_output`). 그래서 PTY를 띄우기 전에 그 탭의 xterm이 먼저
  있어야 한다 — 지금 모든 실행 경로가 `makeTerm` 뒤에 `spawnInto`를 부른다.
- 열린 탭마다 `OpenConsole.exe`가 떠 있다. 설치·제거 프로그램이 덮어쓰거나 지우기 전에
  설치 폴더의 것만 끈다(`installer-hooks.nsh`).
- 판을 올릴 때는 위 시험들을 이 폴더의 파일을 시험 실행 파일 옆(`target/debug/deps`)에 두고 다시 돌린다.
