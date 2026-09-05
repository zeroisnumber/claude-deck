# ✻ CLI Deck

Claude Code · Codex · Gemini CLI 세션을 사이드바로 관리하고, 임베디드 ConPTY 터미널(xterm.js)에서 실제 CLI를 그대로 구동하는 멀티 에이전트 Windows 데스크톱 앱. Tauri 2 기반. (구 Claude Deck)

## 기능

- **세션 사이드바** — `~/.claude/projects`의 모든 세션을 요약·프로젝트·시간순으로 표시, 검색 필터
- **클릭 한 번으로 재개** — 세션 클릭 시 임베디드 터미널에서 `claude --resume` 실행, 열려 있으면 즉시 전환
- **멀티 탭** — 여러 세션 동시 실행, 드래그로 순서 변경, `Ctrl+Tab` 순환
- **상태 표시** — 답변/작업 중(🟠 점멸) · 대기(🟢) · 종료(⚪)를 탭과 사이드바에 표시
- **에이전트 / 래퍼 프로필** — 실행 명령을 `래퍼 접두사 + 에이전트 명령`으로 합성 (예: `headroom wrap` + `claude`). 커스텀 프록시·다른 에이전트 자유롭게 등록
- **전역 환경변수** — 설정에서 `KEY=VAL;KEY2=VAL2` 형식으로 지정하면 모든 세션 실행에 적용 (예: `HEADROOM_OUTPUT_SHAPER=1;PYTHONUTF8=1`)
- **클립보드** — `Ctrl+V`/`Shift+Insert` 붙여넣기, 선택 후 `Ctrl+C` 복사, 우클릭은 선택 시 복사·미선택 시 붙여넣기 (한글 IME 상태에서도 동작)
- **자동 업데이트** — GitHub Releases 기반 서명된 업데이트

## 개발

```powershell
npm install
cd src-tauri
cargo build          # 디버그 빌드 → target/debug/claude-deck.exe
cargo tauri build    # 배포 빌드 (NSIS 인스톨러 + 업데이터 아티팩트)
```

UI(`ui/`)는 빌드 시 바이너리에 임베드되므로, 프런트 수정 후에는 재빌드가 필요하다.

### 코드 구조

| 경로 | 역할 |
|---|---|
| `src-tauri/src/main.rs` | 앱 진입점, 트레이 메뉴, 명령 등록, 파일 열기·삭제 명령 |
| `src-tauri/src/sessions.rs` | `~/.claude/projects`·`~/.codex/sessions`·`~/.gemini/tmp` 스캔, 세션 메타 캐시, 백그라운드 잡(`~/.claude/jobs`, 데몬 로스터) 연결 |
| `src-tauri/src/pty.rs` | ConPTY 생성·입출력·종료, 출력 스트리밍 이벤트 |
| `src-tauri/src/activity.rs` | 작업 중/대기 판정(`~/.claude/sessions/<pid>.json` 우선, 출력 밀도 추정 폴백), 완료 알림, 프롬프트 캐시 유지 핑 |
| `src-tauri/src/usage.rs` | 세션 파일 기반 사용량 통계, 요금제 한도(상태줄 → 사용량 API → headroom 순) |
| `src-tauri/src/statusline.rs` | Claude Code 상태줄 명령으로 실행돼 세션별 상태 페이로드를 저장하는 탭 모드 |
| `src-tauri/src/diag.rs` | 크래시 로그, PTY 트레이스(설정에서 켬), 명령줄 비밀값 가림 |
| `ui/main.js` | 사이드바·탭·터미널·설정·한도 위젯 전체 프런트 (번들러 없이 스크립트 하나) |
| `ui/vendor/` | xterm.js와 애드온. 수동 패치는 `ui/vendor/PATCHES.md` 참고 |
| `tools/startup-check.js` | `main.js`를 최상위까지 실행해 시작 시 던지는지 검사 (`node tools/startup-check.js`) |

러스트 모듈은 `use super::*`로 크레이트 루트를 공유하므로, 모듈 간에 쓰는 항목은 `pub(crate)`로 둔다. 검증은 `cd src-tauri && cargo test`와 `node tools/startup-check.js`.

## 릴리스

```powershell
# 버전 올리기: package.json, src-tauri/Cargo.toml, src-tauri/tauri.conf.json
git tag v0.3.0
git push origin v0.3.0
```

태그를 푸시하면 GitHub Actions(tauri-action)가 빌드·서명·Release 업로드·`latest.json` 생성까지 수행하고, 설치된 앱이 다음 실행 시 업데이트 버튼을 표시한다.

업데이트 서명 개인키는 `%USERPROFILE%\.tauri\claude-deck.key` (저장소에 없음). GitHub Secrets의 `TAURI_SIGNING_PRIVATE_KEY`에 등록되어 있어야 한다.
