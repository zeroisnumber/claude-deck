# ✻ Claude Deck

Claude Code 세션을 사이드바로 관리하고, 임베디드 ConPTY 터미널(xterm.js)에서 실제 CLI를 그대로 구동하는 Windows 데스크톱 앱. Tauri 2 기반.

## 기능

- **세션 사이드바** — `~/.claude/projects`의 모든 세션을 요약·프로젝트·시간순으로 표시, 검색 필터
- **클릭 한 번으로 재개** — 세션 클릭 시 임베디드 터미널에서 `claude --resume` 실행, 열려 있으면 즉시 전환
- **멀티 탭** — 여러 세션 동시 실행, 드래그로 순서 변경, `Ctrl+Tab` 순환
- **상태 표시** — 답변/작업 중(🟠 점멸) · 대기(🟢) · 종료(⚪)를 탭과 사이드바에 표시
- **에이전트 / 래퍼 프로필** — 실행 명령을 `래퍼 접두사 + 에이전트 명령`으로 합성 (예: `headroom wrap` + `claude`). 커스텀 프록시·다른 에이전트 자유롭게 등록
- **자동 업데이트** — GitHub Releases 기반 서명된 업데이트

## 개발

```powershell
npm install
cd src-tauri
cargo build          # 디버그 빌드 → target/debug/claude-deck.exe
cargo tauri build    # 배포 빌드 (NSIS 인스톨러 + 업데이터 아티팩트)
```

UI(`ui/`)는 빌드 시 바이너리에 임베드되므로, 프런트 수정 후에는 재빌드가 필요하다.

## 릴리스

```powershell
# 버전 올리기: package.json, src-tauri/Cargo.toml, src-tauri/tauri.conf.json
git tag v0.2.0
git push origin v0.2.0
```

태그를 푸시하면 GitHub Actions(tauri-action)가 빌드·서명·Release 업로드·`latest.json` 생성까지 수행하고, 설치된 앱이 다음 실행 시 업데이트 버튼을 표시한다.

업데이트 서명 개인키는 `%USERPROFILE%\.tauri\claude-deck.key` (저장소에 없음). GitHub Secrets의 `TAURI_SIGNING_PRIVATE_KEY`에 등록되어 있어야 한다.
