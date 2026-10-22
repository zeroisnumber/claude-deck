// [core] 모든 화면이 쓰는 바탕. 맨 먼저 로드된다.
// 담당: 오류 보고(detach·reportFatal), invoke·listen, 터미널 색(TERM_THEME), 앱 상태
//       (sessions·terms·activeId·shownIds·tabOrder), 진단 계측(uiTrace·frames), $ 등 작은 도구.
// 일반 <script>라 최상위 let·const·function은 뒤 파일들과 전역으로 함께 쓴다(모듈이 아니다 —
// ui-checks가 전역을 이름으로 부른다). 로드하는 동안 바로 실행되는 줄은 앞 파일의 것만 쓸 수 있다.

// CLI Deck — 프런트엔드: 세션 사이드바 + PTY 터미널 관리

// 이 스크립트가 어디서든 던지면 창도 안 뜨고 세션 목록도 안 그려지는데, 여태
// 아무 데도 안 남아서 "트레이에서만 열린다"는 증상만 보였다. 무엇이 어디서
// 터졌는지부터 남긴다 — 다른 어떤 코드보다 먼저 붙어야 의미가 있다.
const reportFatal = (what) => {
  try {
    window.__TAURI__.core.invoke("trace_ui", { kind: "script-failed", value: String(what) });
  } catch { /* 이것마저 안 되면 방법이 없다 */ }
};
window.addEventListener("error", (e) => {
  reportFatal(`${e.message} @ ${e.filename}:${e.lineno}:${e.colno}`);
});
// 결과를 기다리지 않고 띄우는 비동기 호출. 실패하면 어디서 났는지와 함께 남긴다.
// 이걸 안 붙이면 창구가 "unhandled rejection"뿐이라 호출 지점을 알 수 없다.
function detach(label, p) {
  if (p && typeof p.catch === "function") {
    p.catch((err) => reportFatal(`${label}: ${err && err.stack ? err.stack : err}`));
  }
  return p;
}

window.addEventListener("unhandledrejection", (e) => {
  reportFatal(`unhandled rejection: ${e.reason && e.reason.stack ? e.reason.stack : e.reason}`);
});

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// 스타일시트가 <head>에서 이 스크립트보다 먼저 로드돼야 한다.
// 배경·전경·커서·선택색은 style.css의 팔레트에서 읽는다. 값을 여기에 베껴 두면
// 팔레트를 바꿀 때 한쪽만 바뀌어 터미널만 옛 색으로 남는다 (실제로 그렇게 됐었다).
// ANSI 16색은 에이전트 출력이 기대하는 색이라 그대로 둔다.
function cssVar(name, fallback) {
  try {
    const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    return v || fallback;
  } catch {
    return fallback;
  }
}
const TERM_THEME = {
  background: cssVar("--bg", "#17181b"),
  foreground: cssVar("--text", "#e3e1dc"),
  cursor: cssVar("--accent", "#7aa2ff"),
  cursorAccent: cssVar("--bg", "#17181b"),
  selectionBackground: cssVar("--term-selection", "#2c3955"),
  black: cssVar("--bg-active", "#2d3137"),
  red: "#e05d5d", green: "#87b387", yellow: "#d9a057",
  blue: "#6a9bcc", magenta: "#b58dae",
  cyan: cssVar("--agent-codex", "#6aa8a8"),
  white: "#c7c9cd",
  brightBlack: cssVar("--text-faint", "#6d7178"),
  brightRed: cssVar("--red-soft", "#ef8080"),
  brightGreen: "#a3cba3",
  brightYellow: cssVar("--amber", "#e8b878"),
  brightBlue: cssVar("--agent-gemini", "#8cb4dd"),
  brightMagenta: "#cba6c4",
  brightCyan: "#8cc2c2",
  brightWhite: cssVar("--text", "#e3e1dc"),
};

// ---------- 상태 ----------
let sessions = [];               // 스캔된 세션 메타
const terms = new Map();         // id -> { term, fit, container, title, cwd, exited }
let tabOrder = [];               // 탭 표시 순서 (드래그로 변경 가능)
let activeId = null;              // 입력을 받는 탭 (하나)
let shownIds = [];                // 화면에 떠 있는 탭 (화면을 나누면 여럿)
const isShown = (id) => shownIds.includes(id);
const shownTabs = () =>
  shownIds.map((id) => [id, terms.get(id)]).filter(([, t]) => t && !t.disposed);

// ---------- UI 계측 (진단 기록이 켜져 있을 때만 의미 있음) ----------
// 메인 스레드가 막히면 xterm이 못 그려서 "입력이 늦다가 한번에" 보인다.
// 막힌 구간의 길이와 시각을 브라우저가 직접 알려주므로, 원인이 20초 폴링인지
// 터미널 페인트인지 추론 없이 갈린다.
// 기록이 꺼져 있으면 IPC 자체를 보내지 않는다. Rust에서 버리게 두면 longtask가
// 잦을 때 그만큼 왕복이 생기는데, 진단이 부하를 만드는 건 이미 한 번 겪었다.
let traceOn = false;
invoke("trace_enabled").then((v) => { traceOn = !!v; }).catch(() => {});
// 값은 보통 밀리초(숫자)지만, IME 계측처럼 무슨 일이 있었는지를 문자열로
// 남기는 자리도 있다. 숫자일 때만 반올림한다 — 안 그러면 문자열이 NaN이 된다.
const uiTrace = (kind, v) => {
  if (!traceOn) return;
  invoke("trace_ui", { kind, value: typeof v === "number" ? String(Math.round(v)) : String(v) })
    .catch(() => {});
};
try {
  new PerformanceObserver((list) => {
    for (const e of list.getEntries()) uiTrace("longtask", e.duration);
  }).observe({ entryTypes: ["longtask"] });
} catch { /* 미지원 브라우저 */ }

// longtask는 50ms 이상만 잡는다. 출력이 쏟아지는 동안 20~40ms씩 자주 밀리는 "렉"은
// 거기 안 걸리므로 프레임 간격을 직접 잰다. 재는 일 자체가 매 프레임 깨어나는 부하라
// 5초 중 1초만 잰다(렉은 몇 초씩 이어지므로 그걸로 잡힌다). 느린 쪽(p95)과 최대를 남기되,
// 매끄러운 구간은 남기지 않는다. 기준은 화면 주사율에 맞춘다 — 그 구간의 중앙값이 한 프레임이고,
// p95가 그 1.6배 이하이고 최대 50ms 이하면 매끄럽다. 예전엔 p95 25ms로 고정해서 120Hz 화면이
// 40fps로 떨어져도 남지 않았다. 기록이 꺼져 있으면 안 돈다.
//   frames  <p95 ms>|<최대 ms>|<프레임 수>|<gl|dom>|<보이는 칸 수>|<5초간 받은 출력 KB>|<중앙값 ms>
// 렌더러 종류는 켤 때와 바꿀 때 따로 남긴다(renderer) — frames가 없을 때 "매끄러웠다"를
// 어느 렌더러에서였는지 알 수 있게.
// 같은 5초마다 출력 이벤트가 Rust에서 나와 여기 닿기까지 걸린 시간도 본다(보낸 시각은 기록이
// 켜져 있을 때만 실려 온다). 메인 스레드·WebView2에서 밀린 시간이라 화면 쪽 계측엔 안 보인다.
//   ipc-out <p50 ms>|<p95 ms>|<최대 ms>|<이벤트 수>   — p95 16ms 또는 최대 50ms를 넘을 때만
const FRAME_SAMPLE_MS = 1000;
let frameGaps = [];
let frameLast = 0;
let frameUntil = 0;
let frameOutBytes = 0; // pty-output이 더한다
let ipcOutLag = [];    // pty-output이 더한다 (ms)
let rendererLogged = false;
// Rust의 wall_ms()와 같은 벽시계(ms)
const wallNow = () => performance.timeOrigin + performance.now();
function frameTick(now) {
  if (!traceOn || now > frameUntil) { frameLast = 0; return; }
  if (frameLast) frameGaps.push(now - frameLast);
  frameLast = now;
  requestAnimationFrame(frameTick);
}
setInterval(() => {
  if (!traceOn) return;
  if (!rendererLogged) { rendererLogged = true; uiTrace("renderer", webglOn ? "gl" : "dom"); }
  // 창이 가려져 프레임이 멈췄던 구간은 버린다(그건 렉이 아니다)
  const gaps = frameGaps.filter((g) => g < 1000).sort((a, b) => a - b);
  const out = frameOutBytes;
  const lag = ipcOutLag.sort((a, b) => a - b);
  frameGaps = [];
  frameOutBytes = 0;
  ipcOutLag = [];
  frameUntil = performance.now() + FRAME_SAMPLE_MS;
  requestAnimationFrame(frameTick);
  if (lag.length) {
    const p95 = lag[Math.floor(lag.length * 0.95)], max = lag[lag.length - 1];
    if (p95 > 16 || max > 50) {
      uiTrace("ipc-out", `${lag[Math.floor(lag.length / 2)].toFixed(1)}|${p95.toFixed(1)}|${max.toFixed(1)}|${lag.length}`);
    }
  }
  if (gaps.length < 10) return;
  const p95 = gaps[Math.floor(gaps.length * 0.95)];
  const max = gaps[gaps.length - 1];
  const median = gaps[Math.floor(gaps.length / 2)];
  if (p95 <= median * 1.6 && max <= 50) return;
  uiTrace("frames", `${Math.round(p95)}|${Math.round(max)}|${gaps.length}|${webglOn ? "gl" : "dom"}|` +
    `${shownIds.length}|${Math.round(out / 1024)}|${median.toFixed(1)}`);
}, 5000);

const $ = (s) => document.querySelector(s);
const listEl = $("#session-list");
let kbId = null; // 사이드바에서 키보드로 고른 세션 (아래 키보드 탐색 참고)
const entry_markers = new Map(); // 탭 id → "지금 이상해" 기록 함수 (Ctrl+Shift+M)
// 닫은 탭의 마지막 실행 번호. 같은 세션을 곧바로 다시 열면 새 탭은 같은 id를 쓰는데,
// 끈 프로세스가 마저 흘린 출력이 새 화면에 섞이지 않게 그 번호 이하를 버린다.
const lastGen = new Map();
const tabsEl = $("#tabs");
const termArea = $("#term-area");
const emptyState = $("#empty-state");

// ---------- 유틸 ----------
// 출력 조각마다 부른다. 내장 fromBase64(WebView2 153에 있음)는 바이트 반복보다 10배쯤
// 빠르다(8KB에 84 → 8µs). 없는 런타임에서는 예전 방식으로.
const b64ToBytes = typeof Uint8Array.fromBase64 === "function"
  ? (b64) => Uint8Array.fromBase64(b64)
  : (b64) => {
    const bin = atob(b64);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    return bytes;
  };

// 보여 줄 프로젝트 이름. 목록은 저장소 이름(project)을 준다 — 저장소 안 하위 폴더
// (claude-deck\src-tauri\src)에서 띄운 세션이 "src"로 따로 보이던 것을 막는다.
function projOf(s) {
  return (s && s.project) || basename(s && s.cwd);
}

function basename(p) {
  return (p || "").replace(/[\\/]+$/, "").split(/[\\/]/).pop() || p;
}

function timeAgo(mtime) {
  const diff = Date.now() / 1000 - mtime;
  if (diff < 60) return "방금";
  if (diff < 3600) return `${Math.floor(diff / 60)}분 전`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}시간 전`;
  return `${Math.floor(diff / 86400)}일 전`;
}

