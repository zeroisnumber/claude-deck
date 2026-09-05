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
