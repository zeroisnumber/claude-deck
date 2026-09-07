# vendor 수동 패치

`npm install` 후 `node_modules/@xterm/*/lib/*.js`를 이 폴더로 복사할 때 아래 패치를 다시 적용해야 한다.

## xterm.js — IME 조합창 위치 (xterm.js PR #5759, 7.0 마일스톤)

Claude Code처럼 화면을 공격적으로 다시 그리는 TUI에서 한글 IME 조합창이 창 왼쪽 위에 뜨는 문제.
조합 시작 직전에 textarea 위치를 커서에 맞춘다. 6.0.0 번들에서 한 곳만 바꾼다.

```
(this.textarea,"compositionstart",(()=>this._compositionHelper.compositionstart())))
→
(this.textarea,"compositionstart",(()=>{this._syncTextArea(),this._compositionHelper.compositionstart(),this._compositionHelper.updateCompositionElements()})))
```

## 2. DEC 2026 동기화 출력 무시 (xterm.js 6.0)

xterm.js 6.0은 `CSI ? 2026 h/l`(synchronized output)을 구현해 `h`와 `l` 사이의 출력을 화면에
반영하지 않고 모아 둔다. Claude Code 풀스크린 렌더러(`/tui fullscreen`)가 프레임마다 이 괄호를
쓰는데, 0.4.0에서 도구 출력을 접었다 펼 때 화면이 바로 갱신되지 않는 증상이 나왔다. 5.5처럼
이 모드를 무시해 즉시 렌더링한다. 깜빡임 억제 효과는 잃지만 화면이 멈추는 것보다 낫다.

`setMode`/`resetMode`의 `case 2026:` 분기를 `break`로 바꾼다:

```
case 2026:this._coreService.decPrivateModes.synchronizedOutput=!0}   →  case 2026:break}
case 2026:this._coreService.decPrivateModes.synchronizedOutput=!1,this._onRequestRefreshRows.fire(void 0)}   →  case 2026:break}
```

`requestMode`(DECRQM 응답)는 그대로 둔다 — 모드를 물으면 "지원 안 함"이 아니라 현재 값(false)을 답한다.
