// [terminal] 탭 하나의 터미널. sidebar.js 다음.
// 담당: GPU 렌더러(ensureWebgl)·줄 간격, 한글 입력기 재연결, 마우스 신호 솎기, 파일 경로 링크·
//       Ctrl+클릭 열기, makeTerm(xterm 만들기·붙여넣기·키), 세션 열기·새 세션·재시작(spawnInto).

// ---------- 터미널 ----------
function loadWebgl(entry) {
  if (!webglOn) { entry.webgl = null; return; }
  try {
    const webgl = new WebglAddon.WebglAddon();
    // 복구 없이 3초가 지나면 애드온이 이걸 쏜다 (GPU를 아예 못 쓰게 된 경우). 재생성을
    // 시도하고 실패하면 DOM 렌더러로 남는다.
    webgl.onContextLoss(() => scheduleWebglRebuild("context lost"));
    entry.term.loadAddon(webgl);
    entry.webgl = webgl;
  } catch { entry.webgl = null; /* GPU를 못 쓰면 DOM 렌더러 그대로 */ }
}

// 설정을 바꾸면 열려 있는 탭에 바로 반영한다 — 끊길 때 켜고 끄며 확인하는 값이라
// 재시작을 요구하면 쓸모가 없다.
// 줄 간격. GPU로 그리면 xterm이 블록·선 문자(▀▄█ │─)를 칸에 꽉 차게 직접 그리므로 줄
// 사이를 띄워도 이어진다. GPU를 끄면 그 문자들을 글꼴로 그려서 띄운 만큼 줄마다 틈이 생기고,
// 글자로 찍은 QR 코드가 줄마다 떨어져 인식이 안 됐다 — 그때는 줄 간격을 1로 붙인다.
// 탭마다 GPU가 붙고 떨어지는 것(최근 본 몇 개만)이 아니라 설정을 따른다. 탭을 바꿀 때마다
// 줄 수가 바뀌면 에이전트가 화면을 통째로 다시 그린다.
function termLineHeight() {
  return webglOn ? 1.25 : 1.0;
}

function applyRenderer() {
  for (const t of terms.values()) dropWebgl(t);
  glOrder = [];
  if (webglOn) for (const id of shownIds) ensureWebgl(id);
  const lh = termLineHeight();
  for (const t of terms.values()) {
    if (t.disposed || t.term.options.lineHeight === lh) continue;
    t.term.options.lineHeight = lh;
  }
  refitShown(); // 보이지 않는 탭은 띄울 때 맞춘다
  uiTrace("renderer", webglOn ? "gl" : "dom");
}

// GPU 렌더러는 최근에 본 탭 몇 개에만 붙인다. 탭마다 붙이면 WebGL 컨텍스트가 탭 수만큼
// 생기는데, 브라우저는 16개쯤에서 가장 오래된 것을 말없이 버리고 그 탭은 되살아나지
// 않는다(되살리려고 다시 만들면 또 하나가 버려진다). 안 보이는 탭은 그릴 일이 없으니
// CPU 렌더러로 둬도 비용이 없다. 보이는 탭은 GPU가 필요하다 — 풀스크린 클로드의 화면
// 한 장을 CPU로 그리면 60ms(최대 104ms), GPU는 17ms. 그동안 키 입력이 밀린다.
// 화면을 4칸까지 나누므로 보이는 4개 + 방금 본 2개. 보이는 칸은 상한에 걸려도 떼지 않는다.
const WEBGL_MAX = 6;
let glOrder = [];
function ensureWebgl(id) {
  if (!webglOn) return;
  const t = terms.get(id);
  if (!t || t.disposed) return;
  glOrder = [id, ...glOrder.filter((x) => x !== id)];
  if (!t.webgl) {
    loadWebgl(t);
    try { t.term.refresh(0, t.term.rows - 1); } catch { /* 닫히는 중인 탭 */ }
  }
  while (glOrder.length > WEBGL_MAX) {
    let k = glOrder.length - 1;
    while (k >= 0 && isShown(glOrder[k])) k--;
    if (k < 0) break;
    const old = terms.get(glOrder.splice(k, 1)[0]);
    if (old) dropWebgl(old);
  }
}
function dropWebgl(t) {
  try { t.webgl && t.webgl.dispose(); } catch { /* 죽은 컨텍스트 위의 dispose는 던질 수 있다 */ }
  t.webgl = null;
}

let webglRebuildTimer = null;
function scheduleWebglRebuild(why) {
  if (webglRebuildTimer) return; // 탭마다 이벤트가 오므로 한 번에 모은다
  reportFatal(`webgl ${why} — 모든 탭의 렌더러 재생성`);
  webglRebuildTimer = setTimeout(() => {
    webglRebuildTimer = null;
    for (const t of terms.values()) dropWebgl(t);
    // 최근에 본 순서를 그대로 살려 다시 붙인다 (오래된 것부터 붙여야 순서가 유지된다)
    const keep = glOrder.slice();
    glOrder = [];
    for (const id of keep.reverse()) ensureWebgl(id);
  }, 100);
}

// 한글 음절·자모. 붙여넣기는 xterm이 onData로 한 번에 넘기지만 괄호 붙여넣기 모드면
// ESC[200~로 시작해서, 조합 없이 온 "한 글자"와 구별된다(아래는 짧은 입력만 본다).
const HANGUL = /^[ᄀ-ᇿ㄰-㆏가-힣]{1,2}$/;
// 처음 값이 0이면 앱을 켜고 3초 안에는 재연결이 막혔다 — 바로 그때가 WebView2가 입력기를
// 놓치는 알려진 때다.
let imeRebindAt = -Infinity;
let imeOrphanAt = 0;
let pasting = false; // 붙여넣기는 조합 없이 한글이 오는 정상 경로다
function rebindIme(term) {
  const now = performance.now();
  // 한 번은 봐준다. 붙여넣기·음성 입력(Win+H)·자동화 도구도 조합 없이 글자를 넣는데,
  // 그때 재연결하면 포커스가 움직여 받아쓰기가 끊긴다. 입력기가 떨어진 상태라면 계속
  // 치게 되므로 1.5초 안에 두 번째가 곧 온다.
  const second = now - imeOrphanAt < 1500;
  imeOrphanAt = now;
  if (!second || now - imeRebindAt < 3000) return; // 연달아 치는 동안 한 번이면 된다
  imeRebindAt = now;
  uiTrace("ime:rebind", `${Math.round(now)}`);
  invoke("rebind_ime").catch(() => {});
  // 포커스는 #focus-sink가 받아 곧바로 터미널로 돌려준다. 재연결은 메인 스레드에서
  // 나중에 실행되므로 여기서 타이머로 돌려놓으면 순서가 어긋날 수 있다.
}
$("#focus-sink").addEventListener("focus", () => {
  const t = terms.get(activeId);
  if (t) { try { t.term.focus(); } catch { /* 닫힌 탭 */ } }
});

// 풀스크린 클로드는 "마우스가 움직일 때마다 알려 달라"(DEC 1003)를 켠다. 그러면 터미널
// 위에서 마우스를 스치기만 해도 칸이 바뀔 때마다 신호가 하나씩 가고, 휠은 한 번 굴리는
// 데 6~20ms 간격으로 여러 개가 간다. 실측: 보낸 입력의 88%가 마우스 신호였고, 한글 24자가
// 8.8초 막히기 직전에 움직임 115개와 휠 16개가 먼저 들어가 있었다. 클로드는 신호마다
// 화면을 다시 계산하므로 그동안 키 입력이 뒤로 밀린다.
// 앱이 마우스 신호를 받는 모드일 때만 솎는다. 평소 스크롤(xterm 자체 스크롤백)은 그대로.
const MOUSE_REPORT_GAP_MS = 50;
function throttleMouseReports(term, container) {
  const reporting = () => term.modes.mouseTrackingMode !== "none";
  // 휠: 클로드가 마우스 신호를 받는 중이면 xterm은 휠 이벤트 하나에 신호 하나를 보낸다
  // (delta 크기는 안 본다). 그래서 모아서 한 번에 보내면 이동량이 사라진다 — 실측으로
  // 12칸이 7칸이 됐다. 대신 "휠 한 칸에 신호 하나"로 맞춘다(Windows Terminal과 같다).
  // 보통 마우스는 이벤트 하나가 한 칸(100px 또는 줄 단위)이라 그대로 가고, 터치패드·
  // 부드러운 스크롤처럼 잘게 오는 것만 한 칸이 찰 때까지 모았다가 하나로 보낸다.
  const WHEEL_NOTCH = 100;
  let wheelAcc = 0;
  term.attachCustomWheelEventHandler((ev) => {
    if (ev.__deckPassed || !reporting() || ev.deltaMode !== 0 || Math.abs(ev.deltaY) >= WHEEL_NOTCH) {
      if (!ev.__deckPassed) wheelAcc = 0;
      return true;
    }
    if (Math.sign(ev.deltaY) !== Math.sign(wheelAcc)) wheelAcc = 0; // 방향이 바뀌면 새로
    wheelAcc += ev.deltaY;
    if (Math.abs(wheelAcc) < WHEEL_NOTCH) return false;
    // 한 칸이 찼다. 작은 원래 이벤트를 그대로 넘기면 xterm이 자기 기준(줄 수)으로 또
    // 걸러서 신호가 안 나갈 수 있다 — 정확히 한 칸짜리를 새로 만들어 넘긴다.
    const dir = Math.sign(wheelAcc);
    wheelAcc -= dir * WHEEL_NOTCH;
    const one = new WheelEvent("wheel", {
      deltaY: dir * WHEEL_NOTCH, deltaMode: 0, clientX: ev.clientX, clientY: ev.clientY,
      screenX: ev.screenX, screenY: ev.screenY, ctrlKey: ev.ctrlKey, altKey: ev.altKey,
      shiftKey: ev.shiftKey, bubbles: true, cancelable: true,
    });
    one.__deckPassed = true;
    ev.target.dispatchEvent(one);
    return false;
  });
  // 움직임은 xterm에 걸 고리가 없어 한 단계 위에서 가로챈다. 버튼을 누른 채 끄는
  // 것(선택·드래그)은 건드리지 않는다. 멈춘 자리는 꼭 알려야 해서 마지막 것은 늦게라도 보낸다.
  let lastMove = 0;
  let pending = null;
  let timer = 0;
  // 누르거나 터미널을 벗어나면 늦게 보낼 움직임을 버린다. 안 그러면 방금 멈춘 자리의
  // 움직임이 클릭의 누르기와 떼기 사이에 끼어 "클릭 도중 버튼이 떨어졌다"로 보인다
  // (실측: 이동 → 누름 → 옛 좌표의 움직임 → 뗌). 누르기는 자기 좌표를 싣고 간다.
  const dropPending = () => { clearTimeout(timer); timer = 0; pending = null; };
  container.addEventListener("mousedown", dropPending, true);
  container.addEventListener("mouseleave", dropPending, true);
  container.addEventListener("mousemove", (ev) => {
    if (ev.__deckPassed || ev.buttons || !reporting()) return;
    const now = performance.now();
    if (now - lastMove >= MOUSE_REPORT_GAP_MS) {
      lastMove = now;
      pending = null;
      return;
    }
    ev.stopPropagation();
    pending = ev;
    if (!timer) {
      timer = setTimeout(() => {
        timer = 0;
        const p = pending;
        pending = null;
        if (!p) return;
        lastMove = performance.now();
        const again = new MouseEvent("mousemove", p);
        again.__deckPassed = true;
        p.target.dispatchEvent(again);
      }, MOUSE_REPORT_GAP_MS);
    }
  }, true);
}

// 화면의 파일 경로에 마우스를 올리면 밑줄, Ctrl+클릭이면 연다(편집기가 있으면 그 줄로).
// 경로처럼 생긴 것 중 실제로 있는 것만 잇는다 — 있는지는 Rust가 본다. 마우스를 올린
// 줄마다 묻게 되므로 같은 줄 글자는 기억해 둔다.
const LINK_STOP = /[\s"'`()[\]{}<>|,;]/;
const linkCache = new Map(); // "폴더\n이어 붙인 글자" → { at, p: Promise<찾은 경로[]> }
function pathCandidates(text) {
  const out = [];
  let i = 0;
  while (i < text.length) {
    if (LINK_STOP.test(text[i])) { i++; continue; }
    let j = i;
    while (j < text.length && !LINK_STOP.test(text[j])) j++;
    let tok = text.slice(i, j), start = i;
    // 절대 경로가 다른 글자에 붙어 있으면(파일:C:\..., =C:\...) 드라이브 글자부터 본다
    const drive = tok.search(/(?<![\p{L}\p{N}])[A-Za-z]:[\\/]/u);
    if (drive > 0) { tok = tok.slice(drive); start += drive; }
    // 문장 끝 구두점, 앞에 붙은 기호(●⎿ 같은 것)는 떼어 낸다
    const lead = tok.match(/^[^\p{L}\p{N}_.~\\/]+/u); // 한글 폴더 이름은 남긴다
    if (lead) { tok = tok.slice(lead[0].length); start += lead[0].length; }
    // 확장자 뒤에 붙은 조사(main.rs를, a.png에서)는 경로가 아니다
    tok = tok.replace(/(\.[A-Za-z]\w{0,9}(?::\d+){0,2})[\uAC00-\uD7A3]+$/u, "$1");
    tok = tok.replace(/[.:]+$/, "");
    const m = tok.match(/^(.+?)(?::(\d+))?(?::(\d+))?$/);
    const p = m && m[1];
    if (p && !p.includes("://") && /[A-Za-z]/.test(p)
        && (/[\\/]/.test(p) || /\.[A-Za-z][\w]{0,9}$/.test(p))) {
      out.push({ start, end: start + tok.length, path: p,
                 line: m[2] ? +m[2] : null, col: m[3] ? +m[3] : null });
    }
    i = j;
  }
  return out;
}

// 줄 하나의 글자와, 글자마다 몇 번째 칸인지. 한글처럼 두 칸짜리 글자가 있으면 글자 위치와
// 칸 위치가 다르다.
function lineCells(buf, y0) {
  const line = buf.getLine(y0);
  if (!line) return null;
  let text = "";
  const colOf = [];
  for (let x = 0; x < line.length; x++) {
    const cell = line.getCell(x);
    if (!cell || cell.getWidth() === 0) continue;
    const ch = cell.getChars() || " ";
    for (let k = 0; k < ch.length; k++) colOf.push([x, cell.getWidth()]);
    text += ch;
  }
  return { y0, text, colOf, wrapped: line.isWrapped };
}

// 긴 경로는 두 줄로 갈린다. 터미널이 접은 줄(isWrapped)이거나, 에이전트가 스스로 줄을 바꾸며
// 다음 줄을 들여 쓴 경우(클로드의 ● 답글)다. 앞 줄이 거의 끝까지 차 있고 경로 글자로
// 끝나며, 다음 줄이 들여쓰기 뒤 곧바로 글자로 이어지면 한 경로로 본다.
const WRAP_SLACK = 6;
function continuesInto(a, b, cols) {
  if (!a || !b) return false;
  const t = a.text.replace(/\s+$/, "");
  if (!t || LINK_STOP.test(t[t.length - 1])) return false;
  if (b.wrapped) return true;
  const lastCol = a.colOf[t.length - 1];
  if (!lastCol || lastCol[0] + lastCol[1] < cols - WRAP_SLACK) return false;
  return /^\s+\S/.test(b.text) && !LINK_STOP.test(b.text.trimStart()[0]);
}

// y줄(1부터)과 이어지는 앞뒤 줄을 한 글자열로 붙이고, 글자마다 [줄, 칸, 폭]을 적는다.
function joinedLine(term, y) {
  const buf = term.buffer.active;
  const cols = term.cols;
  const cur = lineCells(buf, y - 1);
  if (!cur) return null;
  const segs = [cur];
  for (let k = 0; k < 2; k++) {               // 앞으로 두 줄까지
    const prev = lineCells(buf, segs[0].y0 - 1);
    if (!continuesInto(prev, segs[0], cols)) break;
    segs.unshift(prev);
  }
  for (let k = 0; k < 2; k++) {               // 뒤로 두 줄까지
    const next = lineCells(buf, segs[segs.length - 1].y0 + 1);
    if (!continuesInto(segs[segs.length - 1], next, cols)) break;
    segs.push(next);
  }
  let text = "";
  const at = [];
  segs.forEach((sg, n) => {
    let body = sg.text, skip = 0;
    if (n < segs.length - 1) body = body.replace(/\s+$/, "");
    if (n > 0 && !sg.wrapped) { skip = body.length - body.trimStart().length; body = body.slice(skip); }
    for (let k = 0; k < body.length; k++) {
      const [x, w] = sg.colOf[skip + k];
      at.push([sg.y0 + 1, x, w]);
    }
    text += body;
  });
  return { text, at };
}

// Ctrl+클릭한 파일을 무엇으로 열지, 확장자별로. 위에서부터 처음 맞는 줄을 따른다.
// 이미지까지 VS Code로 열리던 것(그림이 바이트로 보였다) — 그림·영상·문서는 기본 프로그램으로.
//   how: auto(VS Code가 있으면 거기서, 줄 번호로) · default(기본 프로그램) · vscode · rider · vs · reveal(탐색기)
const OPEN_RULES_DEFAULT = [
  { name: "그림·영상·문서", exts: "png jpg jpeg gif webp bmp ico svg tga dds psd exr hdr mp4 mov webm mkv mp3 wav ogg pdf docx xlsx pptx", how: "default" },
  { name: "C++·C#", exts: "cpp h hpp c cc cxx inl cs", how: "auto" },
  { name: "그 밖의 파일", exts: "*", how: "auto" },
];
let openRules = OPEN_RULES_DEFAULT.map((r) => ({ ...r }));
try {
  const saved = JSON.parse(localStorage.getItem("openRules") || "null");
  if (Array.isArray(saved) && saved.length === OPEN_RULES_DEFAULT.length) {
    openRules = OPEN_RULES_DEFAULT.map((r, i) => ({ ...r, exts: r.exts === "*" ? "*" : String(saved[i].exts || ""), how: String(saved[i].how || r.how) }));
  }
} catch { /* 기본값 */ }
function openHowFor(path) {
  const m = /\.([^.\\/]+)$/.exec(path);
  const ext = m ? m[1].toLowerCase() : "";
  for (const r of openRules) {
    if (r.exts === "*" || r.exts.toLowerCase().split(/[\s,]+/).filter(Boolean).map((e) => e.replace(/^\./, "")).includes(ext)) return r.how;
  }
  return "auto";
}
function openLinkPath(path, line, col) {
  invoke("open_link", { path, line, col, how: openHowFor(path) })
    .catch((err) => showToast("열 수 없습니다", String(err)));
}

function fileLinkProvider(term, entry) {
  return {
    provideLinks(y, callback) {
      const j = joinedLine(term, y);
      if (!j) return callback(undefined);
      const { text, at } = j;
      // 이 줄에 걸친 후보만
      const cands = pathCandidates(text).filter((c) => at.slice(c.start, c.end).some(([ly]) => ly === y));
      if (!cands.length || !entry.cwd && cands.every((c) => !/^[A-Za-z]:/.test(c.path))) return callback(undefined);
      const key = entry.cwd + "\n" + text;
      // 방금 만든 파일도 곧 잡히게 잠깐만 기억한다
      const hit = linkCache.get(key);
      let pending = hit && performance.now() - hit.at < 10000 ? hit.p : null;
      if (!pending) {
        pending = invoke("resolve_links", { cwd: entry.cwd || "", candidates: cands.map((c) => c.path) })
          .catch(() => []);
        linkCache.delete(key);
        linkCache.set(key, { at: performance.now(), p: pending });
        if (linkCache.size > 300) linkCache.delete(linkCache.keys().next().value);
      }
      pending.then((found) => {
        const links = [];
        cands.forEach((c, n) => {
          const full = found[n];
          if (!full) return;
          // 밑줄은 이 줄에 있는 부분만. 두 줄에 걸친 경로는 줄마다 따로 잡히고 같은 파일을 연다.
          const mine = at.slice(c.start, c.end).filter(([ly]) => ly === y);
          const [, x0] = mine[0];
          const [, x1, w1] = mine[mine.length - 1];
          links.push({
            range: { start: { x: x0 + 1, y }, end: { x: x1 + w1, y } },
            text: text.slice(c.start, c.end),
            activate: (e) => {
              if (!e.ctrlKey) return; // 그냥 클릭은 에이전트 화면 조작이다
              openLinkPath(full, c.line, c.col);
            },
          });
        });
        callback(links.length ? links : undefined);
      });
    },
  };
}

function makeTerm(id, title, cwd) {
  const container = document.createElement("div");
  container.className = "term-container";
  termArea.appendChild(container);

  const term = new Terminal({
    fontFamily: '"Cascadia Mono", Consolas, "D2Coding", monospace',
    fontSize: fontSize,
    lineHeight: termLineHeight(),
    letterSpacing: 0,
    cursorBlink: true,
    scrollback: 8000,
    theme: TERM_THEME,
    allowProposedApi: true,
    // 에이전트가 OSC 8 링크를 찍으면 xterm 기본 동작은 confirm() 창이다 — 앱을 막는다
    linkHandler: { activate() {} },
  });
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  // 글자 폭을 에이전트와 똑같이 세야 한다. 다르게 세면 그 글자 뒤가 한 칸씩 밀리고,
  // 줄바꿈 지점이 달라지면서 표나 박스가 아래로 갈수록 어긋난다.
  // xterm 기본값(유니코드 6)은 ✅ ❌도 1칸으로 세서 11 테이블을 썼는데, 11도 모자랐다.
  // 클로드는 Bun.stringWidth(ambiguousIsNarrow)로 세는데 그와 비교하면 11 테이블은
  //   ⚠️ ✔️ ℹ️ (문자+VS16)      1칸  (클로드 2)
  //   🥲 🫠 🪿 (유니코드 13~15)   1칸  (클로드 2)
  //   👍🏻 👨‍💻 (피부색·ZWJ 연결)  4칸  (클로드 2)
  // 로 어긋났다. 문자 묶음(grapheme)을 한 덩어리로 세는 15 테이블은 셋 다 맞춘다.
  // 애드온은 불러오는 순간 "15-graphemes"로 바꾼다. 없으면 11로 물러난다.
  try {
    term.loadAddon(new UnicodeGraphemesAddon.UnicodeGraphemesAddon());
  } catch {
    try {
      term.loadAddon(new Unicode11Addon.Unicode11Addon());
      term.unicode.activeVersion = "11";
    } catch { /* 애드온 없으면 기본 동작 유지 */ }
  }
  term.open(container);
  throttleMouseReports(term, container);
  // 기본 DOM 렌더러는 스크롤·출력마다 행을 메인 스레드에서 다시 만든다. WebGL은
  // 글리프를 셀 단위로 GPU에서 그려서 그 비용이 사라진다.
  // GPU 프로세스가 죽었다 살아나면(드라이버 리셋 — 2026-09-07/08 두 번 실측) 모든 탭의
  // 컨텍스트가 한꺼번에 사라졌다 "복구"되는데, 애드온의 자체 복구 경로는 이 경우 화면을
  // 비운 채 아무 이벤트도 내지 않는다(헤드리스 크롬에서 GPU 크래시로 재현). 복구 이벤트를
  // 우리가 직접 받아 모든 탭의 애드온을 버리고 다시 만들면 정상으로 돌아온다 — 탭들이
  // 글리프 아틀라스를 공유하므로 전부 함께 버려야 한다. 손실·복구 이벤트 때만 도는 코드다.
  const entry = { term, fit, container, title, cwd, exited: false, busy: false, profile: null, webgl: null };
  // GPU 렌더러는 activate가 붙인다 (최근에 본 몇 개만)
  term.element.addEventListener("webglcontextrestored", () => scheduleWebglRebuild("context restored"), true);
  term.registerLinkProvider(fileLinkProvider(term, entry));

  // 한글 입력이 가끔 두 번 들어가고 조합창이 엉뚱한 데 뜬다. 재현이 들쭉날쭉해서
  // 한 번 찍어 보는 로그로는 못 잡는다 — 진단 기록이 켜져 있는 동안 조합 이벤트와
  // 실제로 PTY로 나간 것을 같은 줄에 남겨, 증상이 난 시각을 나중에 뒤져 본다.
  // 두 번 들어가는 게 xterm이 두 번 보내서인지(onData 2회) 저쪽 에코인지(1회)가
  // 여기서 갈린다. 조합 중이 아닐 때의 평범한 타이핑은 남기지 않는다.
  let lastComposing = 0;
  const imeTrace = (kind, data) => {
    if (!traceOn) return;
    uiTrace(`ime:${kind}`, `${Math.round(performance.now())}|${id}|${data}`);
  };
  let lastOrphan = { at: 0, data: "" };
  term.onData((d) => {
    const now = performance.now();
    if (traceOn && now - lastComposing < 500) {
      imeTrace("data", JSON.stringify(d));
    }
    // 정상이면 한글은 반드시 조합(composition)을 거쳐 들어온다. 조합 없이 완성된 한글이
    // 오면 WebView2가 입력기를 놓친 것이다 — 조합창이 창 왼쪽 위에 뜨고 글자가 하나씩
    // 따로 확정되며, 가끔 같은 글자가 두 번 온다. 다른 창에 갔다 오면 풀리는 상태라
    // 앱이 그 재연결을 대신 한다. 그 사이 같은 글자가 거의 동시에 또 오면 버린다
    // (사람이 같은 글자를 50ms 안에 두 번 칠 수는 없다).
    if (!pasting && HANGUL.test(d) && now - lastComposing > 1000) {
      if (d === lastOrphan.data && now - lastOrphan.at < 50) {
        uiTrace("ime:orphan-dup", `${Math.round(now)}|${id}|${JSON.stringify(d)}`);
        return;
      }
      lastOrphan = { at: now, data: d };
      uiTrace("ime:orphan", `${Math.round(now)}|${id}|${JSON.stringify(d)}`);
      rebindIme(term);
    }
    invoke("write_pty", { id, data: d });
  });

  // 조합창이 커서에서 얼마나 떨어져 있는지. 한 칸을 넘게 벌어지면 그게 "왼쪽 위"다.
  const imeGap = () => {
    try {
      const ta = term.element.querySelector(".xterm-helper-textarea");
      const scr = term.element.querySelector(".xterm-screen");
      if (!ta || !scr) return "";
      const cell = term._core._renderService.dimensions.css.cell;
      const r = ta.getBoundingClientRect(), s = scr.getBoundingClientRect();
      const wantTop = s.top + term.buffer.active.cursorY * cell.height;
      const wantLeft = s.left + term.buffer.active.cursorX * cell.width;
      return `dy=${Math.round(r.top - wantTop)},dx=${Math.round(r.left - wantLeft)}`;
    } catch { return "?"; }
  };
  for (const ev of ["compositionstart", "compositionupdate", "compositionend"]) {
    term.textarea.addEventListener(ev, (e) => {
      lastComposing = performance.now();
      if (!traceOn) return;   // 켜져 있을 때만 문자열을 만든다
      imeTrace(ev.replace("composition", ""), `${JSON.stringify(e.data || "")} ${imeGap()}`);
    });
  }
  term.textarea.addEventListener("input", (e) => {
    if (!traceOn || performance.now() - lastComposing >= 500) return;
    imeTrace("input", `${JSON.stringify(e.data || "")} composing=${e.isComposing} ${e.inputType || ""}`);
  });
  // 백스페이스·엔터를 실제로 몇 번 눌렀는지. 조합을 백스페이스로 지운 뒤 0.03~0.19초
  // 만에 DEL이 한 번 더 나간 게 13번 중 7번 있었다 — 두 번 누른 건지, 한 번 눌렀는데
  // 두 번 처리된 건지는 누른 횟수를 봐야 갈린다. 조합 직후에만 남긴다.
  term.textarea.addEventListener("keydown", (e) => {
    if (!traceOn || performance.now() - lastComposing >= 1500) return;
    if (e.key !== "Backspace" && e.key !== "Enter" && e.keyCode !== 229) return;
    imeTrace("key", `${e.key} code=${e.keyCode} composing=${e.isComposing} repeat=${e.repeat}`);
  }, true);
  entry_markers.set(id, () => {
    // "지금 이상해" — 그 순간 화면에 실제로 그려진 입력 줄과 조합창 상태를 남긴다.
    // 클로드가 "두두"를 그렸는지(두 번 받았거나 두 번 그렸다), 조합창이 겹쳐 보인
    // 것인지가 여기서 갈린다.
    const b = term.buffer.active;
    const lines = [];
    for (let y = Math.max(0, b.cursorY - 2); y <= Math.min(term.rows - 1, b.cursorY + 1); y++) {
      const line = b.getLine(b.viewportY + y);
      lines.push(`${y === b.cursorY ? ">" : " "}${line ? line.translateToString(true) : ""}`);
    }
    const comp = term.element.querySelector(".composition-view");
    uiTrace("mark", `${Math.round(performance.now())}|${id}|cursor=${b.cursorX},${b.cursorY} ` +
      `ta=${JSON.stringify(term.textarea.value)} comp=${JSON.stringify(comp ? comp.textContent : "")}` +
      `${comp && comp.classList.contains("active") ? "(active)" : ""} ${imeGap()} ` +
      `lines=${JSON.stringify(lines)}`);
  });

  // Ctrl+V / Shift+Insert = Tauri 클립보드로 붙여넣기 (WebView2 네이티브 paste 미동작 대응.
  // preventDefault로 keydown을 완전히 가로채므로 이중 붙여넣기도 발생하지 않음)
  // 선택 상태에서 Ctrl+C = 복사. 앱 단축키(Ctrl+Tab/1~9/Shift+W/N)는 터미널이 먹지 않게 가로챔
  // ⚠ 한글 IME가 켜져 있으면 e.key가 "ㅍ"/"ㅊ"로 들어와 매칭이 실패하므로 물리 키(e.code) 기준으로 판정
  const pasteFromClipboard = async () => {
    let text = "";
    try {
      const cm = window.__TAURI__ && window.__TAURI__.clipboardManager;
      text = cm ? await cm.readText() : await navigator.clipboard.readText();
    } catch {
      try { text = await navigator.clipboard.readText(); } catch { /* 클립보드 접근 실패 */ }
    }
    // 글자가 없으면 캡처 같은 이미지일 수 있다. 터미널로는 이미지를 못 보내므로 임시
    // 파일로 저장해 그 경로를 붙여넣는다(클로드는 이미지 경로를 붙여넣으면 첨부한다).
    if (!text) {
      const path = await invoke("save_clipboard_image").catch(() => null);
      if (path) text = quotePath(path);
    }
    if (text) {
      // paste는 onData를 곧바로(동기로) 부른다 — 그동안만 입력기 검사를 끈다
      pasting = true;
      try { term.paste(text); } finally { pasting = false; }
    }
  };
  const copySelection = () => {
    const sel = term.getSelection();
    if (!sel) return;
    const cm = window.__TAURI__ && window.__TAURI__.clipboardManager;
    if (cm) cm.writeText(sel).catch(() => {});
    else navigator.clipboard.writeText(sel);
  };
  term.attachCustomKeyEventHandler((e) => {
    if (e.type !== "keydown") return true;
    // xterm은 false를 받아도 이벤트를 멈추지 않는다 — 그대로 window까지 올라가 거기
    // 리스너가 같은 단축키를 한 번 더 실행했다(Ctrl+Tab이 두 칸, Ctrl+Shift+B가 제자리).
    if (handleShortcut(e)) { e.stopPropagation(); return false; }
    if ((e.ctrlKey && !e.shiftKey && e.code === "KeyV") || (e.shiftKey && e.code === "Insert")) {
      e.preventDefault();
      pasteFromClipboard();
      return false;
    }
    if (e.ctrlKey && e.code === "KeyC" && term.hasSelection()) {
      copySelection();
      term.clearSelection();
      return false;
    }
    return true;
  });

  // 우클릭: 선택 영역이 있으면 복사, 없으면 붙여넣기 (Windows Terminal 방식)
  container.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    if (term.hasSelection()) {
      copySelection();
      term.clearSelection();
    } else {
      pasteFromClipboard();
    }
  });

  // 나눈 화면에서는 칸마다 크기가 따로 바뀐다(배치 변경, 경계 끌기)
  ro.observe(container);
  // 칸을 누르면 그 칸으로 입력이 간다
  container.addEventListener("mousedown", () => {
    if (activeId === id || !isShown(id)) return;
    focusTab(id);
    renderTabs();
    renderSidebar();
  }, true);

  if (lastGen.has(id)) entry.deadGen = lastGen.get(id);
  terms.set(id, entry);
  tabOrder.push(id);
  return entry;
}

// 세션 id는 cmd.exe /c 명령줄에 그대로 붙는다. 클로드 id는 파일 이름에서, codex·gemini
// id는 기록 파일 안에서 온다 — 파일 이름에는 &도 쓸 수 있어서, 그런 이름의 파일 하나가
// 누르는 순간 명령을 실행시킬 수 있다. 실제 id는 영숫자와 -뿐이다.
function safeId(x) {
  const s = String(x || "");
  if (/^[A-Za-z0-9_-]{1,80}$/.test(s)) return s;
  showToast("⚠ 열 수 없는 세션", `세션 id에 쓸 수 없는 글자가 있습니다: ${s.slice(0, 40)}`);
  throw new Error("unsafe session id: " + s);
}

// 에이전트별 실행 명령. claude는 프로필 시스템, codex/gemini는 각자 CLI의 재개 방식
function commandFor(meta) {
  if (meta.agent === "codex") {
    return { cmd: `codex resume ${safeId(meta.session_id)}`, profile: { name: "Codex", cmd: "codex" } };
  }
  if (meta.agent === "gemini") {
    // gemini CLI는 세션 ID 재개가 없어 프로젝트별 최신 세션만 --resume latest 가능
    const newest = sessions
      .filter((x) => x.agent === "gemini" && x.cwd === meta.cwd)
      .sort((a, b) => b.mtime - a.mtime)[0];
    const isLatest = newest && newest.session_id === meta.session_id;
    return {
      cmd: isLatest ? "gemini --resume latest" : "gemini",
      profile: { name: "Gemini", cmd: "gemini" },
    };
  }
  return { cmd: composeCommand(meta.session_id), profile: currentProfile() };
}

// 데몬이 잡고 있는 백그라운드 세션은 --resume이 거절된다("claude attach <id>를 쓰라"고
// 안내하고 종료). 그 세션을 이 탭에 그대로 붙인다. 래퍼 프로필(예: my-proxy run claude)은
// 인자를 그대로 넘기므로 뒤에 attach를 붙이면 되고, claude가 아닌 명령이면 claude를 직접 부른다.
function attachCommand(short) {
  const p = currentProfile();
  const base = /(^|\s)claude$/.test(p.cmd.trim()) ? p.cmd.trim() : "claude";
  return envPrefix(globalEnv) + `${base} attach ${safeId(short)}`;
}

async function openSession(meta, focus = true, opts = {}) {
  const id = meta.session_id;
  // 복사본은 원본과 다른 세션이 된다. 원본이 이미 열려 있어도 새 탭이어야 하고,
  // 원본 id로 탭을 만들면 안 된다 — 그러면 재시작·복원이 원본을 건드린다.
  const forking = !!opts.fork;
  if (!forking) {
    // 방금 시작한 세션이 이미 새 탭으로 떠 있을 수 있다. 폴링이 그 세션을 알기 전이어도
    // 넘겨받은 meta로 짝을 맞춘다 — 안 맞추면 같은 세션이 두 번 뜬다(같은 기록 파일에
    // 두 프로세스가 붙는다).
    const openId = adoptFor(meta);
    if (openId) return focus && activate(openId);
  }

  const name = (sessionTitle(meta) || id.slice(0, 8)).slice(0, 40);
  const shown = forking ? `${name} (복사본)`.slice(0, 40) : name;
  const title = projOf(meta) + " · " + shown.slice(0, 24);
  // 복사본은 아직 자기 세션 id가 없다. 새 세션과 같은 임시 id로 띄우고, 자기 기록
  // 파일이 생기면 adoptFor가 붙여 준다.
  const tabId = forking ? "new-" + Date.now() : id;
  // 명령부터 만든다. 세션 id가 명령줄에 넣을 수 없는 모양이면 여기서 멈춰야 한다 —
  // 탭을 먼저 만들면 프로세스도 재시작 길도 없는 빈 탭이 남는다.
  const spec = commandFor(meta);
  const attach = !forking && meta.agent === "claude" && meta.bg_running && meta.bg_short;
  const spawnCommand = attach
    ? attachCommand(meta.bg_short)
    // --fork-session은 --resume과 짝이다. 프로필에서 재개를 꺼 놨으면 그대로는
    // 아무것도 갈라져 나오지 않으므로 여기서는 강제로 붙인다.
    : forking ? `${composeCommand(id, spec.profile, true)} --fork-session`
    : spec.cmd;
  const entry = makeTerm(tabId, title, meta.cwd);
  entry.name = shown;
  entry.proj = projOf(meta);
  entry.profile = spec.profile;
  if (forking) {
    entry.agent = meta.agent || "claude";
    entry.startedAt = Date.now() / 1000;
    entry.knownAtStart = new Set(sessions.map((x) => x.session_id));
    // 복사본의 기록 파일은 원본을 그대로 옮겨 적고 시작한다 — 첫 질문이 원본과 같다.
    // 같은 폴더에서 새 세션 탭과 복사본 탭이 함께 떠 있을 때 둘을 가르는 근거다.
    entry.forkOf = { id, firstPrompt: meta.first_prompt || "" };
  }
  entry.spawnCommand = spawnCommand;
  entry.file = forking ? null : meta.file;
  if (focus) activate(tabId);
  else { fillEmptyPanes(); renderTabs(); } // 나눈 화면이면 빈 칸에 앉힌다(복원할 때)
  await spawnInto(tabId, entry, "실행 실패");
  if (forking) catchUpAdoption(tabId);
  saveOpenTabs();
}

// 탭에 붙은 명령을 PTY로 띄운다. 실패하면 탭을 "종료됨"으로 두고 사유를 터미널과 토스트에 남긴다.
// 세션 열기·새 세션·재시작이 모두 이 경로를 쓴다.
// 에이전트가 기록을 읽는 동안(큰 세션은 3~5초) 탭이 비어 있어 입력이 안 먹는 것처럼
// 보였다. 실제로는 그 사이 친 글자를 클로드가 버리지 않는다(claude_typeahead로 확인 —
// 첫 화면이 뜨면 입력창에 그대로 있다). 그걸 알려 준다. 첫 화면이 오면 거두고, 늦어도
// 20초 뒤엔 거둔다(첫 화면이 작게 오는 에이전트도 있다).
function showLoading(t) {
  hideLoading(t);
  const el = document.createElement("div");
  el.className = "term-loading";
  // 뜨는 동안 친 글자를 버리지 않는다는 건 클로드로만 확인했다(claude_typeahead).
  // 다른 에이전트에는 약속하지 않는다.
  const agent = t.agent || agentOf(t.profile);
  el.textContent = agent === "claude" ? "불러오는 중 — 먼저 입력하셔도 그대로 전달됩니다" : "불러오는 중…";
  t.container.appendChild(el);
  t.loadingEl = el;
  t.loadingTimer = setTimeout(() => hideLoading(t), 20000);
}
function hideLoading(t) {
  clearTimeout(t.loadingTimer);
  if (t.loadingEl) { t.loadingEl.remove(); t.loadingEl = null; }
}

async function spawnInto(id, t, failLabel) {
  try {
    // 새로 뜨는 프로세스는 아직 아무 상태도 알리지 않았다. 전 실행이 "작업 중"이나
    // "입력 필요"에서 끝났으면 그 표시가 남아, 한가해도 영영 작업 중으로 보였다.
    t.busy = false;
    t.waiting = false;
    t.attention = false;
    showLoading(t);
    const gen = await invoke("spawn_pty", {
      id, cwd: t.cwd, command: t.spawnCommand || "claude", file: t.file || null, title: t.title,
      sessionId: t.sessionId || null,
      cols: t.term.cols, rows: t.term.rows,
    });
    if (typeof gen === "number") t.gen = gen;
    return true;
  } catch (err) {
    hideLoading(t);
    t.exited = true;
    t.term.write(`
[31m${failLabel}: ${err}[0m
`);
    showToast(`⚠ 세션 ${failLabel}`, String(err));
    renderTabs();
    return false;
  }
}

function agentOf(prof) {
  const cmd = (prof && prof.cmd) || "";
  if (/(^|\s)codex(\s|$)/.test(cmd)) return "codex";
  if (/(^|\s)gemini(\s|$)/.test(cmd)) return "gemini";
  return "claude";
}

async function openNewSession(cwd, prof) {
  const p = prof || currentProfile();
  const id = "new-" + Date.now();
  const entry = makeTerm(id, basename(cwd) + " · 새 세션", cwd);
  entry.name = "새 세션";
  entry.proj = basename(cwd);
  entry.profile = p;
  entry.agent = agentOf(p);
  entry.startedAt = Date.now() / 1000;
  // 짝을 맞출 때 "탭이 뜬 뒤에 새로 생긴 세션"만 후보로 둔다. mtime만 보면 다른 데서
  // 쓰고 있던 옛 세션도 조건을 통과해 엉뚱한 탭에 붙는다.
  entry.knownAtStart = new Set(sessions.map((s) => s.session_id));
  entry.spawnCommand = composeCommand(null, p);
  activate(id);
  // 띄우지 못한 경로를 최근 폴더에 넣으면 누를 때마다 같은 실패가 난다
  if (await spawnInto(id, entry, "실행 실패")) addRecentDir(cwd);
  catchUpAdoption(id);
}

async function restartTab(id) {
  const t = terms.get(id);
  if (!t || !t.exited) return;
  if (t.gen != null) t.deadGen = t.gen;
  t.exited = false;
  t.attention = false;
  t.term.write("\r\n\x1b[38;5;244m── 재시작 ──\x1b[0m\r\n\r\n");
  activate(id);
  await spawnInto(id, t, "재시작 실패");
}

