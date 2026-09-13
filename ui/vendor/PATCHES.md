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

## addon-unicode-graphemes 0.4.0 — 패치 없음, 유니코드 폭 제공자

xterm 6.0.0과 같은 날 배포된 짝(`npm pack @xterm/addon-unicode-graphemes@0.4.0`의 `lib/addon-unicode-graphemes.js`를
그대로 복사). 불러오는 순간 `activeVersion`을 `"15-graphemes"`로 바꾼다. 없으면 main.js가 unicode11로 물러난다.

넣은 이유: 글자 폭을 클로드와 다르게 세면 그 뒤가 한 칸씩 밀린다. 클로드는
`Bun.stringWidth(s, {ambiguousIsNarrow: true})`로 센다(2.1.270 바이너리에서 확인). bun으로 57자를 대조했을 때
unicode11은 8자가 달랐고(⚠️ ✔️ ℹ️ 1칸, 🥲 🫠 🪿 1칸, 👍🏻 👨‍💻 4칸 — 클로드는 전부 2칸) graphemes는 2자만 다르다
(🫠 U+1FAE0, 🪿 U+1FABF를 1칸으로 셈). 상자 선·모호 폭 기호(─ │ ┌ ● ※ … · ° ± × ÷)는 둘 다 1칸으로 클로드와 같다.
xterm을 올릴 때 이 대조를 다시 돌린다.

## (기록) DEC 2026 동기화 출력 무시 패치 — 적용했다가 되돌림

0.4.1에서 xterm 6.0의 synchronized output(`CSI ? 2026 h/l`) 지원이 화면 갱신 지연의 원인이라
보고 무시하도록 패치했으나, 앱과 같은 방식(cmd.exe /c, ConPTY)으로 띄운 claude는 풀스크린
렌더러를 켜지 않아 2026 괄호를 내지 않는다는 것을 테스트 장치로 확인했다(xterm 5.5와 6.0의
입력→렌더 지연도 동일, 중앙값 16ms). 패치는 효과가 없어 되돌렸다. 나중에 Claude Code가
ConPTY에서도 풀스크린을 켜게 되면 다시 검토한다.
