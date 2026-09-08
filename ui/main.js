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
  background: cssVar("--bg", "#202124"),
  foreground: cssVar("--text", "#e6e7ea"),
  cursor: cssVar("--accent", "#8b9bf5"),
  cursorAccent: cssVar("--bg", "#202124"),
  selectionBackground: cssVar("--term-selection", "#3b4050"),
  black: cssVar("--bg-active", "#33363b"),
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
  brightWhite: cssVar("--text", "#e6e7ea"),
};

// ---------- 상태 ----------
let sessions = [];               // 스캔된 세션 메타
const terms = new Map();         // id -> { term, fit, container, title, cwd, exited }
let tabOrder = [];               // 탭 표시 순서 (드래그로 변경 가능)
let activeId = null;

// ---------- UI 계측 (진단 기록이 켜져 있을 때만 의미 있음) ----------
// 메인 스레드가 막히면 xterm이 못 그려서 "입력이 늦다가 한번에" 보인다.
// 막힌 구간의 길이와 시각을 브라우저가 직접 알려주므로, 원인이 20초 폴링인지
// 터미널 페인트인지 추론 없이 갈린다.
// 기록이 꺼져 있으면 IPC 자체를 보내지 않는다. Rust에서 버리게 두면 longtask가
// 잦을 때 그만큼 왕복이 생기는데, 진단이 부하를 만드는 건 이미 한 번 겪었다.
let traceOn = false;
invoke("trace_enabled").then((v) => { traceOn = !!v; }).catch(() => {});
const uiTrace = (kind, ms) => {
  if (!traceOn) return;
  invoke("trace_ui", { kind, value: String(Math.round(ms)) }).catch(() => {});
};
try {
  new PerformanceObserver((list) => {
    for (const e of list.getEntries()) uiTrace("longtask", e.duration);
  }).observe({ entryTypes: ["longtask"] });
} catch { /* 미지원 브라우저 */ }

const $ = (s) => document.querySelector(s);
const listEl = $("#session-list");
const tabsEl = $("#tabs");
const termArea = $("#term-area");
const emptyState = $("#empty-state");

// ---------- 유틸 ----------
function b64ToBytes(b64) {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
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

// ---------- 사이드바 ----------
async function refreshSessions() {
  // 창이 안 보이면 목록을 새로 읽지 않는다 — 전 프로젝트 폴더를 훑고 사이드바를
  // 통째로 다시 만드는 작업이라, 보이지도 않는 동안 20초마다 할 이유가 없다.
  // 다시 보이는 순간 아래 visibilitychange가 한 번 당겨 실행한다.
  if (document.hidden && restored) return;
  const t0 = performance.now();
  sessions = await invoke("list_sessions");
  const t1 = performance.now();
  if (syncCtxGauges()) renderTabs();
  renderSidebar();
  if (t1 - t0 > 20) uiTrace("list-ipc", t1 - t0);
  if (performance.now() - t1 > 5) uiTrace("poll-render", performance.now() - t1);
  restoreTabs(); // 최초 1회만 동작 (이전에 열려 있던 탭 자동 재개)
}

// 별칭 / 핀 (localStorage)
let aliases = {};
try { aliases = JSON.parse(localStorage.getItem("aliases")) || {}; } catch { /* 무시 */ }
let pins = [];
try { pins = JSON.parse(localStorage.getItem("pins")) || []; } catch { /* 무시 */ }

const AGENT_GLYPH = { claude: "✻", codex: "❖", gemini: "✦" };

// 모델 ID를 사람이 읽는 이름으로. 목록 행에는 자리가 없어 호버 미리보기 카드에만 쓴다.
// 알려진 접두사만 다듬고 모르는 ID는 그대로 보여준다 — 새 모델이 나와도 틀린 이름이 안 나오게.
function modelName(id) {
  if (!id || id === "<synthetic>") return "";
  const m = id.match(/^claude-(fable|opus|sonnet|haiku|mythos)-(\d+)(?:-(\d+))?/);
  if (!m) return id;
  return `${m[1][0].toUpperCase()}${m[1].slice(1)} ${m[2]}${m[3] ? "." + m[3] : ""}`;
}
let agentFilter = "all";

// 인라인 이름 편집 중 — 사이드바를 통째로 다시 그리면 입력창이 사라진다.
// 탭 드래그의 isDraggingTab과 같은 이유, 같은 방식.
let isRenaming = false;

const BG_STATE = {
  working: { cls: "run", label: "실행 중" },
  blocked: { cls: "wait", label: "입력 필요" },
  failed:  { cls: "fail", label: "실패" },
};

const PIN_SVG = `<svg class="si-pin" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="M12 17v5"></path><path d="M9 3h6l-1 6 3 4H7l3-4z"></path></svg>`;

// 상태 슬롯 하나에 두 축을 합친다. 탭이 열려 있으면 탭 상태(작업 중·대기·종료)가 이기고,
// 없으면 백그라운드 잡 상태(실행 중·입력 필요·완료·실패), 둘 다 없으면 빈 자리.
// 배지는 백그라운드 잡에만 붙고, 실행 중이 아닐 때는 "종료됨" 대신 완료/실패로 갈린다.
function sessionStatus(s, t) {
  if (t) return { cls: statusClass(t), label: statusLabel(t), badge: null };
  if (!s.bg_state) return { cls: "", label: "", badge: null };
  if (s.bg_running) {
    const bg = BG_STATE[s.bg_state] || { cls: "run", label: s.bg_state };
    return { cls: "bg-" + bg.cls, label: `백그라운드 · ${bg.label}`, badge: bg };
  }
  const bg = s.bg_state === "failed" ? BG_STATE.failed : { cls: "done", label: "완료" };
  return { cls: "bg-" + bg.cls, label: `백그라운드 · ${bg.label}`, badge: bg };
}

// 표시 이름: 사용자 별칭 → Claude Code가 붙인 세션 이름(포크면 "⑂", 복제면 "(2)"가
// 이미 붙어 있다) → 요약 → 첫 프롬프트 순.
function sessionTitle(s) {
  return aliases[s.session_id] || s.title || s.summary || s.first_prompt || "";
}

// 사이드바 한 줄. child=true면 원본 아래 들여쓴 백그라운드 세션이다.
function sessionRow(s, child) {
  const el = document.createElement("div");
  el.className = "session-item" + (s.session_id === activeId ? " active" : "") + (child ? " child" : "");
  el.dataset.id = s.session_id;
  const t = terms.get(s.session_id);
  const title = sessionTitle(s) || "(내용 없음)";
  // 포크된 세션인데 이름에 표식이 없으면 붙여 준다 (원본이 목록에 없어 들여쓰기가 안 될 때도 보이게)
  const fork = s.parent_id && !title.includes("⑂") ? `<span class="si-fork" title="포크된 세션">⑂</span>` : "";

  const st = sessionStatus(s, t);
  const unread = t && t.attention && !t.exited;
  const slot = st.cls ? `<span class="si-status ${st.cls}${unread ? " unread" : ""}" title="${unread ? "응답 완료 — 아직 안 봄" : st.label}"></span>` : "";
  const pin = pins.includes(s.session_id) ? PIN_SVG : "";
  const glyph = `<span class="si-agent ${s.agent}" title="${s.agent}">${AGENT_GLYPH[s.agent] || "•"}</span>`;
  const badge = st.badge ? `<span class="si-bg ${st.badge.cls}" title="${st.label}">${st.badge.label}</span>` : "";
  const expEpoch = s.cache_last_ts && s.cache_ttl_secs ? s.cache_last_ts + s.cache_ttl_secs : null;
  const ttl = expEpoch ? `<span class="si-ttl" data-exp="${expEpoch}" title="프롬프트 캐시 남은 TTL"></span>` : "<span></span>";
  const ctxText = s.ctx_tokens
    ? `<span class="si-ctx" title="마지막 응답 시점 컨텍스트 토큰 수">${fmtTok(s.ctx_tokens)} ctx</span>`
    : "<span></span>";

  // 왼쪽 상태 슬롯 + 본문. 메타 줄은 고정 칸(프로젝트 | 시각 | ctx | TTL)이라 행마다 자리가 같다.
  el.innerHTML = `
    <div class="si-slot">${slot}</div>
    <div class="si-body">
      <div class="si-title">${pin}${glyph}${fork}<span class="si-title-text"></span>${badge}</div>
      <div class="si-meta">
        <span class="si-proj"></span>
        <span class="si-time">${timeAgo(s.mtime)}</span>
        ${ctxText}
        ${ttl}
      </div>
      ${s.bg_detail ? `<div class="si-bg-detail"></div>` : ""}
    </div>`;
  el.querySelector(".si-title-text").textContent = title;
  el.querySelector(".si-proj").textContent = basename(s.cwd);
  if (s.bg_detail) el.querySelector(".si-bg-detail").textContent = s.bg_detail;

  el.onclick = () => openSession(s);
  el.oncontextmenu = (e) => { e.preventDefault(); showCtxMenu(e, s, el); };
  el.onmouseenter = () => schedulePreview(el, s);
  el.onmouseleave = hidePreview;
  return el;
}

// 백그라운드·포크 세션 전용 우클릭 항목. 왼쪽 클릭은 openSession이 알아서
// (실행 중이면 attach, 아니면 --resume) 처리하므로 여기엔 대안만 둔다.
function bgMenuItems(s) {
  const items = [];
  const parent = s.parent_id ? sessions.find((x) => x.session_id === s.parent_id) : null;
  if (parent) items.push(["↖ 원본 세션 열기", () => openSession(parent)]);
  // 복사본은 아무 claude 세션에서나 갈라져 나올 수 있다 (codex/gemini는 --fork-session이 없다)
  if (s.agent === "claude") items.push(["⑂ 복사본으로 열기", () => openSession(s, true, { fork: true })]);
  return items;
}

const STATUS_FILTERS = {
  "!": (s, t) => (t && t.busy && !t.waiting && !t.exited) || (!t && s.bg_running && s.bg_state === "working"),
  "@": (s, t) => (t && (t.attention || t.waiting)) || (!t && s.bg_running && s.bg_state === "blocked"),
  "#": (s, t) => !!t,
  "&": (s, t) => !t && s.bg_state === "failed",
};

function renderSidebar() {
  // 접혀 있으면 그리지 않는다 — 20초 폴링마다 보이지도 않는 DOM을 통째로 다시
  // 만들 이유가 없다. 펼칠 때 setSidebarCollapsed가 한 번 다시 그린다.
  if (sideCollapsed) return;
  const sbT0 = performance.now();
  // 플래그만 믿으면 어떤 경로로든 안 풀렸을 때 사이드바가 영구히 멈춘다.
  // 실제 입력창이 남아 있을 때만 건너뛰고, 없으면 플래그를 스스로 되돌린다.
  if (isRenaming) {
    if (listEl.querySelector(".si-rename")) return;
    isRenaming = false;
  }
  // 검색어 앞의 기호 하나는 상태 필터다 (Agent Deck 방식):
  //   !  작업 중  ·  @  입력 필요·읽지 않은 완료  ·  #  열린 탭  ·  &  실패
  let q = $("#search").value.trim().toLowerCase();
  const statusKey = STATUS_FILTERS[q[0]] ? q[0] : "";
  if (statusKey) q = q.slice(1).trim();
  listEl.innerHTML = "";

  const visible = sessions.filter((s) => {
    if (agentFilter !== "all" && s.agent !== agentFilter) return false;
    if (statusKey && !STATUS_FILTERS[statusKey](s, terms.get(s.session_id))) return false;
    if (!q) return true;
    // 제목·프로젝트에 더해 백그라운드 상태 문구도 검색 대상에 넣는다
    const hay = [
      sessionTitle(s),
      basename(s.cwd),
      s.bg_detail || "",
      s.bg_state || "",
      modelName(s.model),
    ].join(" ").toLowerCase();
    return hay.includes(q);
  });
  const sorted = [...visible].sort((a, b) => {
    const pa = pins.includes(a.session_id) ? 1 : 0;
    const pb = pins.includes(b.session_id) ? 1 : 0;
    return pb - pa || b.mtime - a.mtime;
  });

  // 백그라운드 세션은 원본 아래로 모은다 (원본이 목록에 있을 때만)
  const shown = new Set(sorted.map((s) => s.session_id));
  const kids = new Map();
  for (const s of sorted) {
    if (s.parent_id && shown.has(s.parent_id)) {
      if (!kids.has(s.parent_id)) kids.set(s.parent_id, []);
      kids.get(s.parent_id).push(s);
    }
  }

  let count = 0;
  for (const s of sorted) {
    if (s.parent_id && shown.has(s.parent_id)) continue; // 부모 밑에서 그린다
    listEl.appendChild(sessionRow(s, false));
    count++;
    for (const k of kids.get(s.session_id) || []) {
      listEl.appendChild(sessionRow(k, true));
      count++;
    }
  }

  if (performance.now() - sbT0 > 5) uiTrace("sidebar", performance.now() - sbT0);
  const busyN = [...terms.values()].filter((t) => !t.exited && t.busy).length;
  const runN = [...terms.values()].filter((t) => !t.exited).length;
  const bgN = sessions.filter((s) => s.bg_running).length;
  const prof = currentProfile();
  const parts = [`세션 ${count}개`, `실행 ${runN}`, `답변중 ${busyN}`];
  if (bgN) parts.push(`백그라운드 ${bgN}`);
  if (prof.cmd !== "claude") parts.push(prof.name);
  $("#foot-count").textContent = parts.join(" · ");
  updateTtlBadges();
}

// 캐시 TTL 배지: 매초 남은 시간만 갱신 (전체 리렌더 없이 텍스트만 갱신해 저비용)
function updateTtlBadges() {
  if (document.hidden || sideCollapsed) return; // 안 보이는 배지는 갱신할 필요가 없다
  const now = Date.now() / 1000;
  for (const el of listEl.querySelectorAll(".si-ttl[data-exp]")) {
    const remain = Math.round(Number(el.dataset.exp) - now);
    if (remain <= 0) {
      el.textContent = "";
      el.classList.remove("warn");
    } else {
      const m = Math.floor(remain / 60);
      const sec = remain % 60;
      el.textContent = `⏱ ${m}:${String(sec).padStart(2, "0")}`;
      el.classList.toggle("warn", remain < 60);
    }
  }
}
setInterval(updateTtlBadges, 1000);

// ---------- 세션 우클릭 메뉴 ----------
const ctxMenu = $("#ctx-menu");

function showCtxMenu(e, s, itemEl) {
  hidePreview();
  const pinned = pins.includes(s.session_id);
  ctxMenu.innerHTML = "";
  const items = [
    ...bgMenuItems(s),
    ["📊 토큰 상세", () => openTurns(s)],
    [pinned ? "📌 핀 해제" : "📌 핀 고정", () => {
      pins = pinned ? pins.filter((x) => x !== s.session_id) : [...pins, s.session_id];
      localStorage.setItem("pins", JSON.stringify(pins));
      renderSidebar();
    }],
    ["✏️ 이름 바꾸기", () => startRename(s, itemEl)],
    ["📂 폴더 열기", () => {
      invoke("open_path", { path: s.cwd }).catch((e) => showToast("⚠ 폴더 열기 실패", String(e)));
    }],
    ["📄 로그 파일 열기", () => {
      invoke("open_log_file", { file: s.file }).catch((e) => showToast("⚠ 로그 열기 실패", String(e)));
    }],
    ["🗑️ 세션 삭제", async () => {
      try {
        const ok = await window.__TAURI__.dialog.confirm(
          `이 세션 기록을 영구 삭제할까요?\n\n${sessionTitle(s) || s.session_id}`,
          { title: "세션 삭제", kind: "warning" });
        if (!ok) return;
        if (terms.has(s.session_id)) await closeTab(s.session_id);
        await invoke("delete_session", { file: s.file });
        refreshSessions();
      } catch { /* 무시 */ }
    }],
  ];
  for (const [label, fn] of items) {
    const d = document.createElement("div");
    d.className = "ctx-item";
    d.textContent = label;
    d.onclick = () => { hideCtxMenu(); fn(); };
    ctxMenu.appendChild(d);
  }
  ctxMenu.classList.remove("hidden");
  const mw = 160;
  ctxMenu.style.left = Math.min(e.clientX, window.innerWidth - mw - 8) + "px";
  ctxMenu.style.top = Math.min(e.clientY, window.innerHeight - 160) + "px";
}
function hideCtxMenu() { ctxMenu.classList.add("hidden"); }
window.addEventListener("click", hideCtxMenu);
window.addEventListener("blur", hideCtxMenu);

function startRename(s, itemEl) {
  const span = itemEl.querySelector(".si-title-text");
  const input = document.createElement("input");
  input.className = "si-rename";
  input.value = aliases[s.session_id] || s.summary || s.first_prompt || "";
  span.replaceWith(input);
  isRenaming = true; // 재렌더가 이 input을 지워버리지 않게 (20초 폴링이 있어 확정적으로 발생)
  input.focus();
  input.select();
  const finish = (save) => {
    if (!isRenaming) return; // commit과 blur가 겹쳐 두 번 들어오는 경우
    isRenaming = false;
    if (save) {
      const v = input.value.trim();
      if (v) aliases[s.session_id] = v;
      else delete aliases[s.session_id];
      localStorage.setItem("aliases", JSON.stringify(aliases));
    }
    renderSidebar();
  };
  input.onkeydown = (e) => {
    if (e.key === "Enter") finish(true);
    if (e.key === "Escape") finish(false);
    e.stopPropagation();
  };
  input.onblur = () => finish(true);
  input.onclick = (e) => e.stopPropagation();
}

// ---------- 경량 마크다운 렌더러 (외부 의존성 없음, HTML 이스케이프 후 변환) ----------
function escapeHtml(s) {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}
// 인라인 표시만 HTML로. 먼저 이스케이프하고 그 위에 장식을 얹는 순서라
// 세션 파일에서 온 문자열이 태그로 되살아나지 않는다.
// 링크는 글자만 남긴다 — 이걸 쓰는 화면(호버 카드, 토큰 팝업)은 마우스를 떼면
// 사라지거나 클릭을 받지 않는 자리라 앵커를 만들 이유가 없다.
function mdPlain(src) {
  return String(src || "")
    .replace(/`{1,3}/g, "")
    .replace(/\*\*/g, "")
    .replace(/\[([^\]]+)\]\(([^)]+)\)/g, "$1");
}
function mdInline(src) {
  return escapeHtml(String(src || ""))
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(?<!\*)\*([^*]+)\*(?!\*)/g, "<em>$1</em>")
    .replace(/\[([^\]]+)\]\(([^)]+)\)/g, "$1");
}

// 호버 카드용 축약 렌더. 카드가 좁아서 표나 코드 블록을 제대로 그려도 못 읽는다.
// 그래서 블록 요소를 없애는 대신 한 줄로 눌러 내용은 남긴다 — 표는 셀을 가운뎃점으로
// 잇고, 코드 블록은 앞 두 줄만 보이고 나머지는 줄 수로 적는다.
const MD_TABLE_ROWS = 4;
const MD_CODE_LINES = 2;
function mdToHtml(src) {
  const lines = String(src || "").split("\n");
  const out = [];
  let inCode = false;
  let codeBuf = [];
  let listOpen = false;
  let tableBuf = [];
  const closeList = () => {
    if (listOpen) {
      out.push("</ul>");
      listOpen = false;
    }
  };
  const flushTable = () => {
    if (!tableBuf.length) return;
    const rows = tableBuf.slice(0, MD_TABLE_ROWS);
    for (const r of rows) out.push(`<p class="md-row">${mdInline(r)}</p>`);
    if (tableBuf.length > rows.length) {
      out.push(`<p class="md-more">…${tableBuf.length - rows.length}줄 더</p>`);
    }
    tableBuf = [];
  };
  const flushCode = () => {
    const head = codeBuf.slice(0, MD_CODE_LINES).map(escapeHtml).join("\n");
    const rest = codeBuf.length - Math.min(codeBuf.length, MD_CODE_LINES);
    out.push(`<pre><code>${head}</code></pre>`);
    if (rest > 0) out.push(`<p class="md-more">코드 ${rest}줄 더</p>`);
    codeBuf = [];
  };
  for (const line of lines) {
    if (line.trim().startsWith("```")) {
      if (inCode) {
        flushCode();
        inCode = false;
      } else {
        closeList();
        flushTable();
        inCode = true;
      }
      continue;
    }
    if (inCode) {
      codeBuf.push(line);
      continue;
    }
    const t = line.trim();
    if (t.startsWith("|") && t.endsWith("|")) {
      const cells = t.slice(1, -1).split("|").map((c) => c.trim());
      if (!cells.every((c) => /^:?-{2,}:?$/.test(c))) {
        closeList();
        tableBuf.push(cells.filter(Boolean).join(" · "));
      }
      continue;
    }
    flushTable();
    const h = line.match(/^(#{1,6})\s+(.*)/);
    if (h) {
      closeList();
      out.push(`<h6>${mdInline(h[2])}</h6>`);
      continue;
    }
    const q = line.match(/^\s*>\s?(.*)/);
    if (q) {
      closeList();
      out.push(`<p class="md-quote">${mdInline(q[1])}</p>`);
      continue;
    }
    const li = line.match(/^\s*[-*]\s+(.*)/);
    if (li) {
      if (!listOpen) {
        out.push("<ul>");
        listOpen = true;
      }
      out.push(`<li>${mdInline(li[1])}</li>`);
      continue;
    }
    const ol = line.match(/^\s*(\d+)[.)]\s+(.*)/);
    if (ol) {
      closeList();
      out.push(`<p class="md-num">${ol[1]}. ${mdInline(ol[2])}</p>`);
      continue;
    }
    closeList();
    if (line.trim() === "") {
      out.push("");
      continue;
    }
    out.push(`<p>${mdInline(line)}</p>`);
  }
  if (inCode) flushCode();
  flushTable();
  closeList();
  return out.join("");
}

// ---------- hover 미리보기 ----------
const previewCard = $("#preview-card");
let previewTimer = null;

// 대화 본문(recent/last_text)은 목록 응답에 없다 — 호버한 세션만 따로 가져온다.
let previewToken = 0;

function schedulePreview(el, s) {
  clearTimeout(previewTimer);
  if (!s.first_prompt && !s.summary) return;   // 보여줄 게 없는 세션은 조회 자체를 생략
  const token = ++previewToken;
  previewTimer = setTimeout(async () => {
    let pv = null;
    try {
      pv = await invoke("session_preview", { file: s.file });
    } catch { /* 무시 */ }
    if (token !== previewToken) return; // 응답을 기다리는 사이 다른 항목으로 이동함
    if (!pv || (!pv.last_text && !pv.recent.length && !s.first_prompt)) return;
    previewCard.innerHTML = `<div class="pv-title"></div><div class="pv-body"></div><div class="pv-meta"></div>`;
    previewCard.querySelector(".pv-title").textContent = aliases[s.session_id] || s.summary || s.first_prompt || "";
    const body = previewCard.querySelector(".pv-body");
    if (pv.recent && pv.recent.length) {
      body.innerHTML = pv.recent
        .map((m) => `<div class="pv-turn ${m.role}"><span class="pv-role">${m.role === "user" ? "나" : "AI"}</span>${mdToHtml(m.text)}</div>`)
        .join("");
    } else {
      body.innerHTML = mdToHtml(pv.last_text || "(응답 없음)");
    }
    // 아래줄: 경로 · 모델(마지막 응답 기준) · 메시지 수
    const model = modelName(s.model);
    previewCard.querySelector(".pv-meta").textContent =
      [s.cwd, model, `${s.message_count}개 메시지`].filter(Boolean).join(" · ");
    previewCard.classList.remove("hidden");
    const r = el.getBoundingClientRect();
    previewCard.style.left = r.right + 8 + "px";
    previewCard.style.top = Math.max(8, Math.min(r.top, window.innerHeight - 400)) + "px";
  }, 350);
}
function hidePreview() {
  clearTimeout(previewTimer);
  previewToken++;                       // 이미 날아간 요청의 응답이 뒤늦게 뜨지 않게
  previewCard.classList.add("hidden");
}

function statusClass(t) {
  return t.exited ? "exited" : t.waiting ? "wait" : t.busy ? "busy" : "idle";
}
function statusLabel(t) {
  return t.exited ? "종료됨" : t.waiting ? "입력 필요" : t.busy ? "답변/작업 중" : "대기 중";
}

// ---------- 완료 알림 (앱 내 토스트 + OS 알림) ----------
function showToast(title, body, onClick) {
  const el = document.createElement("div");
  el.className = "toast";
  el.innerHTML = `<div class="toast-title"></div><div class="toast-body"></div>`;
  el.querySelector(".toast-title").textContent = title;
  el.querySelector(".toast-body").textContent = body;
  el.onclick = () => { el.remove(); if (onClick) onClick(); };
  $("#toasts").appendChild(el);
  setTimeout(() => {
    el.classList.add("fade");
    setTimeout(() => el.remove(), 400);
  }, 6000);
}

function notifyDone(id, t) {
  // 활성 탭 + 창 포커스 상태면 사용자가 이미 보고 있음 — 알림 불필요
  if (id === activeId && document.hasFocus()) return;
  t.attention = true;
  showToast("✻ 응답 완료", t.title, () => activate(id));
  // OS 알림은 Rust에서 보낸다. 창이 최소화되면 WebView2가 렌더러를 재워서
  // 이 코드 자체가 늦게 도는데, 알림이 필요한 순간이 정확히 그때이기 때문이다.
}

// 작업중/완료 판정은 Rust가 한다 (PTY 출력 밀도 + 세션 파일의 턴 종료).
// 예전에는 여기서 1초마다 "최근 2.5초 내 출력이 있었나"로 추정했는데,
// 실측 결과 턴 내부 침묵이 78.7초까지 나와서 그 방식으로는 완료를 알 수 없었다.
// 창이 백그라운드로 가면 이 타이머 자체가 스로틀링되는 문제도 있었다.
listen("pty-state", (ev) => {
  const { id, working, waiting, notify } = ev.payload;
  const t = terms.get(id);
  if (!t) return;
  const wasWaiting = !!t.waiting;
  if (t.busy === working && wasWaiting === !!waiting) return;
  t.busy = working;
  t.waiting = !!waiting;
  if (!working && !t.exited && notify !== false) notifyDone(id, t);
  // 권한 확인·질문으로 멈춘 세션: 보고 있지 않으면 완료 알림과 같은 방식으로 알린다
  if (t.waiting && !wasWaiting && !(id === activeId && document.hasFocus())) {
    t.attention = true;
    showToast("✋ 입력 필요", t.title, () => activate(id));
  }
  renderTabs();
  renderSidebar();
});

// ---------- 터미널 ----------
function loadWebgl(entry) {
  try {
    const webgl = new WebglAddon.WebglAddon();
    // 복구 없이 3초가 지나면 애드온이 이걸 쏜다 (GPU를 아예 못 쓰게 된 경우). 재생성을
    // 시도하고 실패하면 DOM 렌더러로 남는다.
    webgl.onContextLoss(() => scheduleWebglRebuild("context lost"));
    entry.term.loadAddon(webgl);
    entry.webgl = webgl;
  } catch { entry.webgl = null; /* GPU를 못 쓰면 DOM 렌더러 그대로 */ }
}

let webglRebuildTimer = null;
function scheduleWebglRebuild(why) {
  if (webglRebuildTimer) return; // 탭마다 이벤트가 오므로 한 번에 모은다
  reportFatal(`webgl ${why} — 모든 탭의 렌더러 재생성`);
  webglRebuildTimer = setTimeout(() => {
    webglRebuildTimer = null;
    for (const t of terms.values()) {
      try { t.webgl && t.webgl.dispose(); } catch { /* 죽은 컨텍스트 위의 dispose는 던질 수 있다 */ }
      t.webgl = null;
    }
    for (const t of terms.values()) {
      loadWebgl(t);
      try { t.term.refresh(0, t.term.rows - 1); } catch { /* 닫히는 중인 탭 */ }
    }
  }, 100);
}

function makeTerm(id, title, cwd) {
  const container = document.createElement("div");
  container.className = "term-container";
  termArea.appendChild(container);

  const term = new Terminal({
    fontFamily: '"Cascadia Mono", Consolas, "D2Coding", monospace',
    fontSize: fontSize,
    lineHeight: 1.25,
    letterSpacing: 0,
    cursorBlink: true,
    scrollback: 8000,
    theme: TERM_THEME,
    allowProposedApi: true,
  });
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  // xterm 기본값은 유니코드 6 폭 테이블이라 ✅ ❌ 같은 이모지를 1칸으로 센다.
  // 에이전트는 2칸으로 그리므로 행마다 1칸씩 어긋나고, 줄바꿈 지점이 달라지면서
  // 표나 박스가 아래로 갈수록 왼쪽으로 밀린다. 11 테이블에서는 2칸으로 센다.
  try {
    term.loadAddon(new Unicode11Addon.Unicode11Addon());
    term.unicode.activeVersion = "11";
  } catch { /* 애드온 없으면 기본 동작 유지 */ }
  term.open(container);
  // 기본 DOM 렌더러는 스크롤·출력마다 행을 메인 스레드에서 다시 만든다. WebGL은
  // 글리프를 셀 단위로 GPU에서 그려서 그 비용이 사라진다.
  // GPU 프로세스가 죽었다 살아나면(드라이버 리셋 — 2026-09-07/08 두 번 실측) 모든 탭의
  // 컨텍스트가 한꺼번에 사라졌다 "복구"되는데, 애드온의 자체 복구 경로는 이 경우 화면을
  // 비운 채 아무 이벤트도 내지 않는다(헤드리스 크롬에서 GPU 크래시로 재현). 복구 이벤트를
  // 우리가 직접 받아 모든 탭의 애드온을 버리고 다시 만들면 정상으로 돌아온다 — 탭들이
  // 글리프 아틀라스를 공유하므로 전부 함께 버려야 한다. 손실·복구 이벤트 때만 도는 코드다.
  const entry = { term, fit, container, title, cwd, exited: false, busy: false, profile: null, webgl: null };
  loadWebgl(entry);
  term.element.addEventListener("webglcontextrestored", () => scheduleWebglRebuild("context restored"), true);

  term.onData((d) => invoke("write_pty", { id, data: d }));

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
    if (text) term.paste(text);
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
    if (handleShortcut(e)) return false;
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

  terms.set(id, entry);
  tabOrder.push(id);
  return entry;
}

// 에이전트별 실행 명령. claude는 프로필 시스템, codex/gemini는 각자 CLI의 재개 방식
function commandFor(meta) {
  if (meta.agent === "codex") {
    return { cmd: `codex resume ${meta.session_id}`, profile: { name: "Codex", cmd: "codex" } };
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
// 안내하고 종료). 그 세션을 이 탭에 그대로 붙인다. 래퍼 프로필(예: headroom wrap claude)은
// 인자를 그대로 넘기므로 뒤에 attach를 붙이면 되고, claude가 아닌 명령이면 claude를 직접 부른다.
function attachCommand(short) {
  const p = currentProfile();
  const base = /(^|\s)claude$/.test(p.cmd.trim()) ? p.cmd.trim() : "claude";
  return envPrefix(globalEnv) + `${base} attach ${short}`;
}

async function openSession(meta, focus = true, opts = {}) {
  const id = meta.session_id;
  if (terms.has(id)) return focus && activate(id);

  const name = (sessionTitle(meta) || id.slice(0, 8)).slice(0, 40);
  const title = basename(meta.cwd) + " · " + name.slice(0, 24);
  const entry = makeTerm(id, title, meta.cwd);
  entry.name = name;
  entry.proj = basename(meta.cwd);
  const spec = commandFor(meta);
  entry.profile = spec.profile;
  const attach = !opts.fork && meta.agent === "claude" && meta.bg_running && meta.bg_short;
  entry.spawnCommand = attach
    ? attachCommand(meta.bg_short)
    : opts.fork ? `${spec.cmd} --fork-session` : spec.cmd;
  entry.file = meta.file;
  if (focus) activate(id);
  else renderTabs();
  await spawnInto(id, entry, "실행 실패");
  saveOpenTabs();
}

// 탭에 붙은 명령을 PTY로 띄운다. 실패하면 탭을 "종료됨"으로 두고 사유를 터미널과 토스트에 남긴다.
// 세션 열기·새 세션·재시작이 모두 이 경로를 쓴다.
async function spawnInto(id, t, failLabel) {
  try {
    await invoke("spawn_pty", {
      id, cwd: t.cwd, command: t.spawnCommand || "claude", file: t.file || null, title: t.title,
      cols: t.term.cols, rows: t.term.rows,
    });
  } catch (err) {
    t.exited = true;
    t.term.write(`
[31m${failLabel}: ${err}[0m
`);
    showToast(`⚠ 세션 ${failLabel}`, String(err));
    renderTabs();
  }
}

async function openNewSession(cwd) {
  const id = "new-" + Date.now();
  const entry = makeTerm(id, basename(cwd) + " · 새 세션", cwd);
  entry.name = "새 세션";
  entry.proj = basename(cwd);
  entry.profile = currentProfile();
  entry.spawnCommand = composeCommand(null);
  activate(id);
  await spawnInto(id, entry, "실행 실패");
  addRecentDir(cwd);
  setTimeout(refreshSessions, 4000);
}

async function restartTab(id) {
  const t = terms.get(id);
  if (!t || !t.exited) return;
  t.exited = false;
  t.attention = false;
  t.term.write("\r\n\x1b[38;5;244m── 재시작 ──\x1b[0m\r\n\r\n");
  activate(id);
  await spawnInto(id, t, "재시작 실패");
}

// ---------- 탭 복원 ----------
function saveOpenTabs() {
  const tabs = tabOrder.filter((id) => !id.startsWith("new-"));
  localStorage.setItem("openTabs", JSON.stringify(tabs));
}

let restored = false;
function restoreTabs() {
  if (restored) return;
  restored = true;
  let saved = [];
  try { saved = JSON.parse(localStorage.getItem("openTabs")) || []; } catch { /* 무시 */ }
  const toOpen = saved.map((sid) => sessions.find((s) => s.session_id === sid)).filter(Boolean);
  toOpen.forEach((meta, i) => openSession(meta, i === 0));
}

function activate(id) {
  activeId = id;
  const cur = terms.get(id);
  if (cur) cur.attention = false;
  for (const [tid, t] of terms) {
    t.container.classList.toggle("visible", tid === id);
  }
  emptyState.classList.add("hidden");
  const t = terms.get(id);
  requestAnimationFrame(() => {
    t.fit.fit();
    invoke("resize_pty", { id, cols: t.term.cols, rows: t.term.rows });
    t.term.focus();
  });
  renderTabs();
  renderSidebar();
}

async function closeTab(id) {
  const t = terms.get(id);
  if (!t) return;
  await invoke("kill_pty", { id });
  // WebGL 애드온이 dispose 도중 던진 적이 있다(트레이스의 _isDisposed). 여기서 멈추면
  // terms에 죽은 항목이 남아 그 세션은 사이드바에서 눌러도 "이미 열려 있음"으로
  // 처리돼 아무 일도 안 일어난다. 정리는 무조건 끝까지 간다.
  try { t.term.dispose(); } catch (err) { reportFatal(`term.dispose: ${err && err.stack || err}`); }
  t.container.remove();
  terms.delete(id);
  tabOrder = tabOrder.filter((x) => x !== id);
  saveOpenTabs();
  if (activeId === id) {
    activeId = null;
    const rest = tabOrder;
    if (rest.length) activate(rest[rest.length - 1]);
    else emptyState.classList.remove("hidden");
  }
  renderTabs();
  renderSidebar();
}

function renderTabs() {
  // 드래그 중엔 재렌더 금지 (상태 갱신·이벤트가 드래그를 깨뜨리지 않게).
  // 단 실제로 드래그 중인 탭이 남아 있을 때만 — 플래그가 굳으면 탭 바가 영영 멈춘다.
  if (isDraggingTab) {
    // 넉넉하게 잡는다 — 여기서 가드를 풀면 드래그 중인 요소가 재렌더로 파괴되므로,
    // 실제 드래그를 방해하지 않는 선에서 "끝나지 않은 드래그"만 걸러내는 게 목적이다.
    if (Date.now() - dragStartedAt < 60_000) return;
    isDraggingTab = false;
  }
  tabsEl.innerHTML = "";
  for (const id of tabOrder) {
    const t = terms.get(id);
    if (!t) continue;
    const el = document.createElement("div");
    el.className = "tab" + (id === activeId ? " active" : "") + (t.exited ? " exited" : "") + (t.attention ? " attention" : "");
    el.dataset.id = id;
    const showBadge = t.profile && t.profile.cmd !== "claude";
    const ctxBar = t.ctxPct != null && !t.exited
      ? `<span class="tab-ctx ${t.ctxPct >= 85 ? "hot" : t.ctxPct >= 60 ? "warm" : ""}" style="width:${t.ctxPct}%" title="컨텍스트 ${t.ctxPct}% (${fmtTok(t.ctxTokens || 0)})"></span>`
      : "";
    el.innerHTML = `<span class="tab-dot ${statusClass(t)}" title="${statusLabel(t)}"></span><span class="tab-label"></span>${showBadge ? '<span class="tab-badge"></span>' : ""}${t.proj ? '<span class="tab-proj"></span>' : ""}${t.exited ? '<button class="tab-restart" title="다시 시작">↻</button>' : ""}<button class="tab-close" title="닫기">✕</button>${ctxBar}`;
    // 세션 이름이 앞, 프로젝트는 칩 — 같은 프로젝트 탭이 여럿이어도 구분된다
    el.querySelector(".tab-label").textContent = t.name || t.title;
    el.title = t.title;
    if (t.proj) el.querySelector(".tab-proj").textContent = t.proj;
    if (showBadge) {
      const b = el.querySelector(".tab-badge");
      b.textContent = t.profile.name.slice(0, 10);
      b.title = t.profile.cmd;
    }
    el.onclick = () => { if (!suppressClick) activate(id); };
    el.querySelector(".tab-close").onclick = (e) => { e.stopPropagation(); closeTab(id); };
    const rbtn = el.querySelector(".tab-restart");
    if (rbtn) rbtn.onclick = (e) => { e.stopPropagation(); restartTab(id); };
    makeTabDraggable(el);
    tabsEl.appendChild(el);
  }
}

// 포인터 기반 탭 드래그 — 잡은 탭은 커서를 따라가고, 나머지는 트랜지션으로 밀려남.
// 판정 기준은 드래그 시작 시점의 고정 좌표(rects)라서 진동이 없음.
let suppressClick = false;
let isDraggingTab = false;
let dragStartedAt = 0;

function makeTabDraggable(el) {
  el.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || e.target.classList.contains("tab-close")) return;
    const startX = e.clientX;
    let dragging = false;
    let tabs = [], rects = [], origIndex = 0, newIndex = 0;

    const move = (ev) => {
      if (ev.pointerId !== e.pointerId) return; // window에서 받으므로 다른 포인터를 걸러야 한다
      if (!dragging && Math.abs(ev.clientX - startX) > 6) {
        dragging = true;
        isDraggingTab = true;
        dragStartedAt = Date.now();
        el.setPointerCapture(e.pointerId);
        el.classList.add("dragging");
        tabsEl.classList.add("drag-active");
        tabs = [...tabsEl.querySelectorAll(".tab")];
        rects = tabs.map((t) => t.getBoundingClientRect());
        origIndex = tabs.indexOf(el);
        newIndex = origIndex;
      }
      if (!dragging) return;

      const dx = ev.clientX - startX;
      el.style.transform = `translateX(${dx}px) scale(1.03)`;

      // 시작 시점 좌표 기준으로 목표 인덱스 계산 (고정 기준 → 안정적)
      const myCenter = rects[origIndex].left + rects[origIndex].width / 2 + dx;
      newIndex = 0;
      tabs.forEach((t, i) => {
        if (i === origIndex) return;
        if (rects[i].left + rects[i].width / 2 < myCenter) newIndex++;
      });

      // 나머지 탭들을 밀어냄
      const w = rects[origIndex].width + 4; // 4 = 탭 간격
      tabs.forEach((t, i) => {
        if (i === origIndex) return;
        let shift = 0;
        if (i > origIndex && i <= newIndex) shift = -w;
        else if (i < origIndex && i >= newIndex) shift = w;
        t.style.transform = shift ? `translateX(${shift}px)` : "";
      });
    };

    // commit=false면 순서를 바꾸지 않고 원상복구만 한다 (드래그가 취소된 경우)
    const up = (commit) => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onCancel);
      el.removeEventListener("lostpointercapture", onCancel);
      if (!dragging) return;
      isDraggingTab = false;
      tabsEl.classList.remove("drag-active");
      if (commit) {
        const id = el.dataset.id;
        tabOrder = tabOrder.filter((x) => x !== id);
        tabOrder.splice(newIndex, 0, id);
        suppressClick = true;               // 드래그 직후의 click은 무시
        setTimeout(() => (suppressClick = false), 0);
      }
      renderTabs();                         // 재렌더로 transform 초기화 + 순서 확정
    };
    // pointerup만 듣고 있으면, 창 밖에서 놓거나 포인터가 취소될 때 up이 영영 안 불린다.
    // 그러면 isDraggingTab이 true로 굳어 renderTabs가 계속 early-return하고
    // 탭 바 전체가 갱신을 멈춘다 — 드래그도 클릭도 먹지 않는 것처럼 보인다.
    const onUp = (ev) => { if (ev.pointerId === e.pointerId) up(true); };
    const onCancel = (ev) => { if (!ev || ev.pointerId === e.pointerId) up(false); };

    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onCancel);
    el.addEventListener("lostpointercapture", onCancel);
  });
}

// ---------- PTY 이벤트 ----------
listen("pty-output", (ev) => {
  const { id, data } = ev.payload;
  const t = terms.get(id);
  if (t) {
    const t0 = performance.now();
    t.term.write(b64ToBytes(data));
    const d = performance.now() - t0;
    if (d > 5) uiTrace("term-write", d);
  }
});

listen("pty-exit", (ev) => {
  const { id } = ev.payload;
  const t = terms.get(id);
  if (t) {
    t.exited = true;
    t.term.write("\r\n\x1b[38;5;244m── 프로세스가 종료되었습니다 ──\x1b[0m\r\n");
    renderTabs();
    renderSidebar();
    refreshSessions();
  }
});

// ---------- 사이드바 접기 ----------
// 접힌 상태에서도 되돌아올 수 있어야 하므로 펼치기 버튼은 #main에 따로 둔다.
// 터미널 재적응은 아래 ResizeObserver가 알아서 한다.
let sideCollapsed = localStorage.getItem("sideCollapsed") === "1";
function setSidebarCollapsed(v) {
  sideCollapsed = v;
  localStorage.setItem("sideCollapsed", v ? "1" : "0");
  $("#app").classList.toggle("side-collapsed", v);
  if (!v) renderSidebar(); // 접혀 있는 동안 밀린 목록 갱신을 반영
}
// 초기 적용은 클래스만 바꾼다. setSidebarCollapsed는 펼칠 때 renderSidebar를
// 부르는데, 이 시점엔 profiles가 아직 선언 전이라 그대로 던진다(그러면 스크립트가
// 죽어 창도 안 뜬다). 첫 그리기는 파일 끝의 refreshSessions가 맡는다.
$("#app").classList.toggle("side-collapsed", sideCollapsed);
$("#btn-side-hide").onclick = () => setSidebarCollapsed(true);
$("#btn-side-show").onclick = () => setSidebarCollapsed(false);

// ---------- 리사이즈 ----------
const ro = new ResizeObserver(() => {
  if (!activeId) return;
  const t = terms.get(activeId);
  if (!t) return;
  t.fit.fit();
  invoke("resize_pty", { id: activeId, cols: t.term.cols, rows: t.term.rows });
});
ro.observe(termArea);

// ---------- 실행 프로필 (설정 창에서 관리) ----------
const DEFAULT_PROFILES = [
  { name: "Claude", cmd: "claude", resume: true },
  { name: "Headroom", cmd: "headroom wrap claude", resume: true },
];

function loadProfiles() {
  try {
    const v = JSON.parse(localStorage.getItem("profiles"));
    if (Array.isArray(v) && v.length) return v;
  } catch { /* 무시 */ }
  return DEFAULT_PROFILES.map((x) => ({ ...x }));
}
let profiles = loadProfiles();
let activeProfile = parseInt(localStorage.getItem("profileSel") || "0", 10);
if (isNaN(activeProfile) || activeProfile >= profiles.length) activeProfile = 0;

function currentProfile() {
  return profiles[activeProfile] || DEFAULT_PROFILES[0];
}

// "KEY=VAL;KEY2=VAL2" 형식 문자열을 cmd.exe용 "set KEY=VAL&&set KEY2=VAL2&&" 접두어로 변환
function envPrefix(envStr) {
  if (!envStr) return "";
  let prefix = "";
  for (const pair of envStr.split(";")) {
    const i = pair.indexOf("=");
    if (i <= 0) continue;
    const key = pair.slice(0, i).trim();
    const val = pair.slice(i + 1).trim();
    if (!key) continue;
    prefix += `set ${key}=${val}&&`;
  }
  return prefix;
}

let globalEnv = localStorage.getItem("globalEnv") || "";

// 상태줄 연동: Claude Code가 상태줄 명령에 넘기는 세션 JSON에는 요금제 한도와
// 실제 컨텍스트 윈도우가 들어 있다. 사용자 settings.json은 건드리지 않고,
// CLI Deck이 띄우는 세션에만 --settings 로 우리 설정을 얹는다.
let statusLineOn = localStorage.getItem("statusLine") === "1";
let statusLinePath = "";
async function ensureStatusLinePath() {
  if (!statusLineOn || statusLinePath) return statusLinePath;
  try { statusLinePath = await invoke("statusline_settings_path"); } catch { statusLinePath = ""; }
  return statusLinePath;
}
ensureStatusLinePath();

// 캐시 유지 설정 — localStorage가 원본이고, 시작할 때와 저장할 때 Rust로 밀어넣는다
const KA_DEFAULT = { enabled: false, thresholdSecs: 120, message: 'reply "." only' };
let keepAlive = { ...KA_DEFAULT };
try { keepAlive = { ...KA_DEFAULT, ...(JSON.parse(localStorage.getItem("keepAlive")) || {}) }; } catch { /* 무시 */ }
function pushKeepAlive() {
  invoke("set_keepalive", {
    enabled: !!keepAlive.enabled,
    thresholdSecs: keepAlive.thresholdSecs,
    message: keepAlive.message,
  }).catch(() => {});
}
pushKeepAlive();

// 최종 실행 명령: 전역 env + 프로필 명령 + (재개 시) --resume <세션ID>
function composeCommand(resumeId) {
  const p = currentProfile();
  let cmd = resumeId && p.resume !== false ? `${p.cmd} --resume ${resumeId}` : p.cmd;
  if (statusLineOn && statusLinePath) cmd += ` --settings "${statusLinePath}"`;
  return envPrefix(globalEnv) + cmd;
}

// 설정 모달
function addProfileRow(name = "", cmd = "", resume = true, checked = false) {
  const row = document.createElement("div");
  row.className = "lrow";
  row.innerHTML = `
    <label class="l-active" title="이 프로필 사용"><input type="radio" name="active-profile" /></label>
    <input class="set-input l-name" placeholder="이름" spellcheck="false" />
    <input class="set-input mono l-cmd" placeholder="실행 명령 (예: headroom wrap claude)" spellcheck="false" />
    <label class="l-resume" title="세션 재개 시 --resume <세션ID> 인자를 붙일지"><input type="checkbox" />재개</label>
    <button class="l-del" title="삭제">✕</button>`;
  row.querySelector(".l-active input").checked = checked;
  row.querySelector(".l-name").value = name;
  row.querySelector(".l-cmd").value = cmd;
  row.querySelector(".l-resume input").checked = resume;
  row.querySelector(".l-del").onclick = () => row.remove();
  $("#profile-list").appendChild(row);
}

$("#btn-settings").onclick = () => {
  $("#profile-list").innerHTML = "";
  profiles.forEach((p, i) => addProfileRow(p.name, p.cmd, p.resume !== false, i === activeProfile));
  $("#global-env").value = globalEnv;
  invoke("trace_enabled").then((on) => { $("#opt-trace").checked = !!on; }).catch(() => {});
  $("#opt-statusline").checked = statusLineOn;
  $("#opt-keepalive").checked = !!keepAlive.enabled;
  $("#ka-threshold").value = String(keepAlive.thresholdSecs / 60);
  $("#ka-message").value = keepAlive.message;
  syncKaFields();
  $("#trace-path").textContent = "";
  $("#lmodal-backdrop").classList.remove("hidden");
};
// 캐시 유지를 꺼두면 임계값과 메시지는 아무 데도 쓰이지 않는다 — 만질 수 있게
// 두면 껐다는 사실이 안 보인다.
function syncKaFields() {
  const on = $("#opt-keepalive").checked;
  $("#ka-fields").classList.toggle("off", !on);
  $("#ka-threshold").disabled = !on;
  $("#ka-message").disabled = !on;
}
$("#opt-keepalive").onchange = syncKaFields;

$("#profile-add").onclick = () => addProfileRow();
$("#btn-clear-diag").onclick = async () => {
  try {
    showToast("🧹 정리 완료", await invoke("clear_diagnostics"));
  } catch (e) {
    showToast("⚠ 정리 실패", String(e));
  }
};
$("#lmodal-cancel").onclick = () => closeModal($("#lmodal-backdrop"));
$("#lmodal-save").onclick = () => {
  const rows = [...document.querySelectorAll("#profile-list .lrow")];
  const next = [];
  let nextActive = 0;
  for (const r of rows) {
    const p = {
      name: r.querySelector(".l-name").value.trim(),
      cmd: r.querySelector(".l-cmd").value.trim(),
      resume: r.querySelector(".l-resume input").checked,
    };
    if (!p.name || !p.cmd) continue;
    if (r.querySelector(".l-active input").checked) nextActive = next.length;
    next.push(p);
  }
  profiles = next.length ? next : DEFAULT_PROFILES.map((x) => ({ ...x }));
  activeProfile = Math.min(nextActive, profiles.length - 1);
  globalEnv = $("#global-env").value.trim();
  localStorage.setItem("profiles", JSON.stringify(profiles));
  localStorage.setItem("profileSel", String(activeProfile));
  localStorage.setItem("globalEnv", globalEnv);
  statusLineOn = $("#opt-statusline").checked;
  localStorage.setItem("statusLine", statusLineOn ? "1" : "0");
  ensureStatusLinePath();
  const kaMin = parseFloat($("#ka-threshold").value);
  keepAlive = {
    enabled: $("#opt-keepalive").checked,
    thresholdSecs: Math.round((isNaN(kaMin) ? 2 : Math.min(60, Math.max(0.5, kaMin))) * 60),
    message: $("#ka-message").value.trim() || KA_DEFAULT.message,
  };
  localStorage.setItem("keepAlive", JSON.stringify(keepAlive));
  pushKeepAlive();
  traceOn = $("#opt-trace").checked;
  invoke("set_trace", { enabled: $("#opt-trace").checked })
    .then((path) => {
      if ($("#opt-trace").checked && path) showToast("진단 기록 켜짐", path);
    })
    .catch((e) => showToast("⚠ 진단 설정 실패", String(e)));
  renderSidebar();
  closeModal($("#lmodal-backdrop"));
};
// 설정 모달: 텍스트 입력창에서 Enter = 저장 (한글 조합 중 Enter 제외)
$("#lmodal-backdrop").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.isComposing && e.target.matches("input:not([type=checkbox]):not([type=radio])")) {
    $("#lmodal-save").click();
  }
});

// ---------- UI 바인딩 ----------
$("#btn-refresh").onclick = refreshSessions;
$("#search").oninput = renderSidebar;

// 모달을 닫을 때 터미널로 포커스를 돌려준다. 안 그러면 포커스가 body에 남아
// 키 입력이 터미널로 안 들어가고, 설정·대시보드를 한 번 열었다 닫은 뒤부터
// "입력이 안 먹는" 것처럼 보인다.
function closeModal(el) {
  el.classList.add("hidden");
  const t = activeId ? terms.get(activeId) : null;
  if (t && !t.exited) t.term.focus();
}

// 모달 공통: 배경 클릭 또는 Escape로 닫기
const MODAL_BACKDROPS = ["#lmodal-backdrop", "#dash-backdrop", "#turns-backdrop", "#modal-backdrop"];
for (const sel of MODAL_BACKDROPS) {
  const el = $(sel);
  el.addEventListener("mousedown", (e) => { if (e.target === el) closeModal(el); });
}
window.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  for (const sel of MODAL_BACKDROPS) {
    const el = $(sel);
    if (!el.classList.contains("hidden")) { closeModal(el); return; }
  }
});

// 에이전트 필터
for (const btn of document.querySelectorAll("#agent-filter .af")) {
  btn.onclick = () => {
    agentFilter = btn.dataset.agent;
    document.querySelectorAll("#agent-filter .af").forEach((b) => b.classList.toggle("on", b === btn));
    renderSidebar();
  };
}

// ---------- 새 세션 모달 (폴더 선택 + 최근 폴더) ----------
function getRecentDirs() {
  try { return JSON.parse(localStorage.getItem("recentDirs")) || []; } catch { return []; }
}
function addRecentDir(dir) {
  const list = [dir, ...getRecentDirs().filter((d) => d !== dir)].slice(0, 8);
  localStorage.setItem("recentDirs", JSON.stringify(list));
}
function openNewModal() {
  const wrap = $("#recent-dirs");
  wrap.innerHTML = "";
  for (const d of getRecentDirs()) {
    const chip = document.createElement("button");
    chip.className = "dir-chip";
    chip.textContent = basename(d);
    chip.title = d;
    chip.onclick = () => { $("#modal-path").value = d; };
    chip.ondblclick = () => { $("#modal-backdrop").classList.add("hidden"); openNewSession(d); };
    wrap.appendChild(chip);
  }
  $("#modal-backdrop").classList.remove("hidden");
  $("#modal-path").focus();
}
$("#btn-new").onclick = openNewModal;
$("#modal-browse").onclick = async () => {
  try {
    const d = await window.__TAURI__.dialog.open({ directory: true, defaultPath: $("#modal-path").value });
    if (d) $("#modal-path").value = d;
  } catch { /* 무시 */ }
};
$("#modal-cancel").onclick = () => closeModal($("#modal-backdrop"));
$("#modal-ok").onclick = () => {
  const p = $("#modal-path").value.trim();
  if (p) {
    $("#modal-backdrop").classList.add("hidden");
    openNewSession(p);
  }
};
$("#modal-path").addEventListener("keydown", (e) => {
  if (e.key === "Enter") $("#modal-ok").click();
  if (e.key === "Escape") $("#modal-cancel").click();
});

// ---------- 단축키 ----------
// Ctrl+Tab 탭 순환, Ctrl+1~9 탭 이동, Ctrl+Shift+W 탭 닫기, Ctrl+Shift+N 새 세션
function handleShortcut(e) {
  if (!e.ctrlKey) return false;
  if (e.key === "Tab") {
    const ids = tabOrder;
    if (ids.length < 2) return true;
    const i = ids.indexOf(activeId);
    activate(ids[(i + (e.shiftKey ? -1 : 1) + ids.length) % ids.length]);
    return true;
  }
  if (!e.shiftKey && e.key >= "1" && e.key <= "9") {
    const idx = parseInt(e.key, 10) - 1;
    if (tabOrder[idx]) activate(tabOrder[idx]);
    return true;
  }
  // 한글 IME에서 e.key가 "ㅈ"/"ㅜ"로 들어오므로 물리 키(e.code) 기준
  if (e.shiftKey && e.code === "KeyW") {
    if (activeId) closeTab(activeId);
    return true;
  }
  if (e.shiftKey && e.code === "KeyN") {
    openNewModal();
    return true;
  }
  if (e.shiftKey && e.code === "KeyB") {
    setSidebarCollapsed(!sideCollapsed);
    return true;
  }
  if (!e.shiftKey && e.code === "KeyK") {
    if (sideCollapsed) setSidebarCollapsed(false);
    $("#search").focus();
    $("#search").select();
    return true;
  }
  return false;
}

// 사이드바 키보드 탐색: 검색창에 포커스가 있을 때 ↑↓로 행을 고르고 Enter로 연다.
// Esc는 검색어를 비우고 터미널로 돌아간다. 고른 행은 .kb 클래스로 표시한다.
let kbIndex = -1;
function kbRows() { return [...listEl.querySelectorAll(".session-item")]; }
function kbHighlight(i) {
  const rows = kbRows();
  rows.forEach((r) => r.classList.remove("kb"));
  if (!rows.length) { kbIndex = -1; return; }
  kbIndex = Math.max(0, Math.min(rows.length - 1, i));
  rows[kbIndex].classList.add("kb");
  rows[kbIndex].scrollIntoView({ block: "nearest" });
}
$("#search").addEventListener("keydown", (e) => {
  if (e.isComposing) return;
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    kbHighlight(kbIndex + (e.key === "ArrowDown" ? 1 : -1));
  } else if (e.key === "Enter") {
    e.preventDefault();
    const rows = kbRows();
    const row = rows[kbIndex] || rows[0];
    if (!row) return;
    const s = sessions.find((x) => x.session_id === row.dataset.id);
    if (s) openSession(s);
  } else if (e.key === "Escape") {
    e.preventDefault();
    $("#search").value = "";
    kbIndex = -1;
    renderSidebar();
    const t = terms.get(activeId);
    if (t) t.term.focus();
  }
});
$("#search").addEventListener("input", () => { kbIndex = -1; });
window.addEventListener("keydown", (e) => {
  if (handleShortcut(e)) e.preventDefault();
});

// Ctrl+휠 폰트 크기 조절
let fontSize = parseFloat(localStorage.getItem("fontSize")) || 13.5;
// Ctrl+휠 = 폰트 크기. preventDefault가 필요해 비패시브여야 하는데, 비패시브
// 휠 리스너가 붙어 있으면 평범한 스크롤도 브라우저가 메인 스레드를 기다린다
// (스레드가 바쁘면 휠이 끊긴다). 그래서 Ctrl을 누르고 있는 동안만 붙인다.
const onZoomWheel = (e) => {
  if (!e.ctrlKey) return;
  e.preventDefault();
  fontSize = Math.min(22, Math.max(9, fontSize + (e.deltaY < 0 ? 1 : -1)));
  localStorage.setItem("fontSize", String(fontSize));
  for (const [id, t] of terms) {
    t.term.options.fontSize = fontSize;
    if (id === activeId) {
      t.fit.fit();
      invoke("resize_pty", { id, cols: t.term.cols, rows: t.term.rows });
    }
  }
};
let zoomWheelOn = false;
function setZoomWheel(on) {
  if (on === zoomWheelOn) return;
  zoomWheelOn = on;
  if (on) termArea.addEventListener("wheel", onZoomWheel, { passive: false });
  else termArea.removeEventListener("wheel", onZoomWheel, { passive: false });
}
window.addEventListener("keydown", (e) => { if (e.ctrlKey) setZoomWheel(true); });
window.addEventListener("keyup", (e) => { if (!e.ctrlKey) setZoomWheel(false); });
window.addEventListener("blur", () => setZoomWheel(false));

// ---------- 자동 업데이트 ----------
async function checkUpdate() {
  try {
    const updater = window.__TAURI__ && window.__TAURI__.updater;
    if (!updater) return;
    const update = await updater.check();
    if (!update) return;
    const btn = $("#btn-update");
    btn.textContent = `⬆ v${update.version} 업데이트`;
    btn.classList.remove("hidden");
    btn.onclick = async () => {
      btn.disabled = true;
      btn.textContent = "다운로드 중…";
      try {
        await update.downloadAndInstall();
        await window.__TAURI__.process.relaunch();
      } catch (e) {
        btn.textContent = "업데이트 실패";
        btn.disabled = false;
      }
    };
  } catch { /* 오프라인 등 — 조용히 무시 */ }
}
setTimeout(checkUpdate, 5000);
setInterval(checkUpdate, 6 * 3600 * 1000); // 6시간마다

// 창 표시: WebView 로드 완료 후에 보여주고 포커스 (첫 실행 한글 IME 미연결 버그 회피)
// 여기서 조용히 실패하면 앱이 트레이에서만 열린다. 실패는 반드시 남긴다 —
// 못 띄웠을 때 Rust 쪽 안전망이 2.5초 뒤에 대신 띄운다.
(async () => {
  try {
    const w = window.__TAURI__.window.getCurrentWindow();
    await w.show();
    await w.setFocus();
  } catch (e) {
    invoke("trace_ui", { kind: "window-show-failed", value: String(e) }).catch(() => {});
  }
})();

// ---------- 사용량: 단가표 · 컨텍스트 게이지 · 대시보드 ----------
// USD per MTok [입력, 출력] — 캐시 읽기 0.1×입력, 캐시 쓰기 5분 1.25×/1시간 2×입력
const PRICING = {
  "claude-fable-5": [10, 50],
  "claude-mythos": [10, 50],
  "claude-opus": [5, 25],
  "claude-sonnet-5": [2, 10], // 인트로 단가 (2026-08-31까지)
  "claude-sonnet": [3, 15],
  "claude-haiku": [1, 5],
};
function priceFor(model) {
  for (const k in PRICING) if (model.startsWith(k)) return PRICING[k];
  return [5, 25];
}
function ctxWindowFor(model) {
  return model.includes("haiku") ? 200_000 : 1_000_000;
}
// 코덱스는 구독제로 쓰고 토큰 단가표가 없다. 클로드 단가를 대신 먹이면 그럴듯한
// 가짜 금액이 나오므로, 값을 계산하지 않고 없음으로 둔다 (표에는 "—"로 나간다).
function rowCost(r) {
  if (r.agent && r.agent !== "claude") return null;
  const [i, o] = priceFor(r.model);
  return (r.input * i + r.cache_read * i * 0.1 + r.cache_5m * i * 1.25 + r.cache_1h * i * 2 + r.output * o) / 1e6;
}
function fmtCost(v) {
  return v === null ? "—" : `$${v.toFixed(2)}`;
}
function fmtTok(n) {
  if (n >= 1e9) return (n / 1e9).toFixed(1) + "B";
  if (n >= 1e6) return (n / 1e6).toFixed(1) + "M";
  if (n >= 1e3) return (n / 1e3).toFixed(1) + "K";
  return String(n);
}

// --- 탭 컨텍스트 게이지 ---
// 목록 응답(list_sessions)에 이미 들어 있는 ctx_tokens/ctx_window로 계산한다.
// 예전에는 탭마다 8초 주기로 세션 파일을 다시 읽어 같은 값을 구하고 있었다.
function syncCtxGauges() {
  let changed = false;
  for (const [id, t] of terms) {
    if (id.startsWith("new-")) continue;
    const meta = sessions.find((s) => s.session_id === id);
    if (!meta || !meta.ctx_tokens) continue;
    const win = meta.ctx_window || ctxWindowFor(meta.model || "");
    const pct = Math.min(100, Math.round((meta.ctx_tokens / win) * 100));
    if (pct !== t.ctxPct) {
      t.ctxPct = pct;
      t.ctxTokens = meta.ctx_tokens;
      changed = true;
    }
  }
  return changed;
}

// --- 대시보드 ---
let dashDays = 7;
let dashRows = [];

function renderDash() {
  const cutoff = dashDays
    ? new Date(Date.now() - (dashDays - 1) * 86400_000).toISOString().slice(0, 10)
    : "";
  const rows = dashRows.filter(
    (r) => r.date >= cutoff && r.model && r.input + r.output + r.cache_read + r.cache_5m + r.cache_1h > 0,
  );

  const tot = { input: 0, output: 0, cache_read: 0, cache_w: 0, requests: 0, cost: 0 };
  const byModel = new Map();
  const byProj = new Map();
  let unpriced = 0;
  for (const r of rows) {
    const cost = rowCost(r);
    if (cost === null) unpriced += 1;
    tot.input += r.input;
    tot.output += r.output;
    tot.cache_read += r.cache_read;
    tot.cache_w += r.cache_5m + r.cache_1h;
    tot.requests += r.requests;
    tot.cost += cost || 0;
    const m = byModel.get(r.model) || { tok: 0, out: 0, cost: 0, req: 0, cacheRead: 0, cacheW: 0, input: 0, unpriced: false };
    m.tok += r.input + r.cache_read + r.cache_5m + r.cache_1h;
    m.out += r.output;
    if (cost === null) m.unpriced = true;
    m.cost += cost || 0;
    m.req += r.requests;
    m.cacheRead += r.cache_read;
    m.cacheW += r.cache_5m + r.cache_1h;
    m.input += r.input;
    byModel.set(r.model, m);
    const pName = basename(r.cwd) || r.cwd;
    const p = byProj.get(pName) || { cost: 0, req: 0, unpriced: false };
    if (cost === null) p.unpriced = true;
    p.cost += cost || 0;
    p.req += r.requests;
    byProj.set(pName, p);
  }

  $("#dash-tiles").innerHTML = `
    <div class="tile"><div class="tile-v">$${tot.cost.toFixed(2)}</div><div class="tile-l">추정 비용${unpriced ? " (클로드만)" : ""}</div></div>
    <div class="tile"><div class="tile-v">${tot.requests.toLocaleString()}</div><div class="tile-l">요청</div></div>
    <div class="tile"><div class="tile-v">${fmtTok(tot.input + tot.cache_read + tot.cache_w)}</div><div class="tile-l">입력 토큰 (캐시 포함)</div></div>
    <div class="tile"><div class="tile-v">${fmtTok(tot.output)}</div><div class="tile-l">출력 토큰</div></div>
    <div class="tile"><div class="tile-v">${tot.cache_read + tot.input > 0 ? Math.round((tot.cache_read / (tot.cache_read + tot.cache_w + tot.input)) * 100) : 0}%</div><div class="tile-l">캐시 적중률</div></div>`;

  const mkTable = (headers, rowsHtml) =>
    `<table><thead><tr>${headers.map((h) => `<th>${h}</th>`).join("")}</tr></thead><tbody>${rowsHtml}</tbody></table>`;

  $("#dash-models").innerHTML = mkTable(
    ["모델", "요청", "입력", "출력", "캐시 적중률", "비용"],
    [...byModel.entries()]
      .sort((a, b) => b[1].cost - a[1].cost || b[1].tok - a[1].tok)
      .map(([m, v]) => {
        const denom = v.cacheRead + v.cacheW + v.input;
        const hit = denom > 0 ? Math.round((v.cacheRead / denom) * 100) : 0;
        return `<tr><td>${m}</td><td>${v.req.toLocaleString()}</td><td>${fmtTok(v.tok)}</td><td>${fmtTok(v.out)}</td><td>${hit}%</td><td>${v.unpriced ? "—" : fmtCost(v.cost)}</td></tr>`;
      })
      .join("") || `<tr><td colspan="6">데이터 없음</td></tr>`,
  );

  $("#dash-projects").innerHTML = mkTable(
    ["프로젝트", "요청", "비용"],
    [...byProj.entries()]
      .sort((a, b) => b[1].cost - a[1].cost)
      .slice(0, 12)
      .map(([p, v]) => `<tr><td>${p}</td><td>${v.req.toLocaleString()}</td><td>${v.unpriced ? "—" : fmtCost(v.cost)}</td></tr>`)
      .join("") || `<tr><td colspan="3">데이터 없음</td></tr>`,
  );
}

async function loadDash() {
  $("#dash-tiles").innerHTML = `<div class="tile"><div class="tile-v">…</div><div class="tile-l">집계 중</div></div>`;
  try {
    // dashDays가 0이면 전체 기간. Rust는 파일 mtime으로 먼저 거르므로
    // 넓은 범위를 고르면 다시 불러와야 오래된 파일이 들어온다.
    dashRows = await invoke("usage_stats", { days: dashDays || 0 });
  } catch {
    dashRows = [];
  }
  renderDash();
}

async function openDash() {
  $("#dash-backdrop").classList.remove("hidden");
  await loadDash();
  // headroom 설치 시 절감 통계 표시 (없으면 섹션 숨김)
  try {
    const hr = await invoke("headroom_stats");
    if (hr && hr.lifetime) {
      $("#dash-hr-wrap").classList.remove("hidden");
      $("#dash-hr").innerHTML = `
        <div id="dash-hr-tiles">
          <div class="tile"><div class="tile-v">${fmtTok(hr.lifetime.tokens_saved || 0)}</div><div class="tile-l">절감 토큰 (누적)</div></div>
          <div class="tile"><div class="tile-v">$${(hr.lifetime.compression_savings_usd || 0).toFixed(2)}</div><div class="tile-l">절감 비용 (누적)</div></div>
          <div class="tile"><div class="tile-v">${(hr.lifetime.requests || 0).toLocaleString()}</div><div class="tile-l">프록시 경유 요청</div></div>
        </div>`;
    }
  } catch { /* headroom 없음 */ }
}

$("#btn-dash").onclick = openDash;
$("#dash-close").onclick = () => closeModal($("#dash-backdrop"));
for (const b of document.querySelectorAll("#dash-period .dp")) {
  b.onclick = () => {
    dashDays = parseInt(b.dataset.days, 10);
    document.querySelectorAll("#dash-period .dp").forEach((x) => x.classList.toggle("on", x === b));
    loadDash();
  };
}

// ---------- 토큰 상세 (세션 하나의 턴별 사용량) ----------
// 긴 세션은 응답이 수백 개다. 턴을 그대로 나열하면 아무것도 안 보이므로 기본 화면은
// 누적 곡선 + 질문별 표뿐이고, 질문을 누르면 그 질문이 만든 턴의 흔적이 펼쳐진다.
// 전체 턴 표는 원자료로 접어 둔다.
let turnsData = [];
let turnsTitle = "";
let turnGroups = [];
// 코덱스는 구독제라 토큰 단가가 없다. 금액을 지어내는 대신 같은 자리에 토큰을 쓴다 —
// 어느 질문이 비쌌나를 묻는 화면이므로 단위만 바뀌면 나머지 구성은 그대로 쓸 수 있다.
let turnsPriced = true;
function turnValue(t) {
  return turnsPriced ? turnCost(t) : t.input + t.cache_read + t.output;
}
function fmtVal(v, fine) {
  if (turnsPriced) return `$${v.toFixed(fine ? 3 : 2)}`;
  return fmtTok(v);
}

function turnCost(t) {
  if (!/^claude-/.test(t.model || "")) return 0; // 코덱스 등 단가표가 없는 모델
  const [i, o] = priceFor(t.model || "");
  return (t.input * i + t.cache_read * i * 0.1 + t.cache_5m * i * 1.25 + t.cache_1h * i * 2 + t.output * o) / 1e6;
}
// 캐시가 끊긴 턴: 읽기가 없는데 쓰기는 큰 경우. 첫 턴은 원래 그러므로 제외한다.
function isCacheMiss(t, idx) {
  return idx > 0 && t.cache_read === 0 && t.cache_5m + t.cache_1h > 5000;
}
// 캐시로 읽은 토큰을 정가로 냈다면 얼마였을지와 실제 낸 값의 차이 — 캐시가 벌어준 돈
function turnSaving(t) {
  if (!/^claude-/.test(t.model || "")) return 0;
  const [i] = priceFor(t.model || "");
  return (t.cache_read * i * 0.9) / 1e6;
}
function turnTime(ts) {
  const d = new Date(ts * 1000);
  return `${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")} ` +
    `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}
function firstLine(s, n) {
  const line = String(s || "").split("\n").find((x) => x.trim()) || "";
  return line.length > n ? line.slice(0, n) + "…" : line;
}

async function openTurns(s) {
  hidePreview();
  turnsPriced = (s.agent || "claude") === "claude";
  turnsTitle = sessionTitle(s) || s.session_id.slice(0, 8);
  turnsData = [];
  turnGroups = [];
  $("#turns-sub").textContent = "읽는 중…";
  $("#turns-tiles").innerHTML = "";
  $("#turns-chart").innerHTML = "";
  $("#turns-prompts").innerHTML = "";
  $("#turns-all").innerHTML = "";
  $("#turns-all").classList.add("hidden");
  $("#turns-toggle-all").textContent = "전체 턴 표 보기";
  $("#turns-backdrop").classList.remove("hidden");
  try {
    turnsData = await invoke("session_turns", { file: s.file });
  } catch (err) {
    $("#turns-sub").textContent = `읽기 실패: ${err}`;
    return;
  }
  renderTurns(s);
}

function renderTurns(s) {
  const ts = turnsData;
  if (!ts.length) {
    $("#turns-sub").textContent = `${turnsTitle} — 토큰 기록이 있는 응답이 없습니다`;
    return;
  }
  const cost = ts.map(turnValue);
  const total = cost.reduce((a, b) => a + b, 0);
  const misses = ts.map((t, i) => (isCacheMiss(t, i) ? i : -1)).filter((i) => i >= 0);
  const readTot = ts.reduce((a, t) => a + t.cache_read, 0);
  const writeTot = ts.reduce((a, t) => a + t.cache_5m + t.cache_1h, 0);
  const saved = ts.reduce((a, t) => a + turnSaving(t), 0);
  const inTot = ts.reduce((a, t) => a + t.input, 0);
  const denom = readTot + writeTot + inTot;
  const hit = denom > 0 ? Math.round((readTot / denom) * 100) : 0;

  $("#turns-sub").textContent =
    `${turnsTitle} · ${basename(s.cwd)} · ${modelName(ts[ts.length - 1].model) || ts[ts.length - 1].model}`;

  const tile = (v, l, cls) => `<div class="tile"><div class="tile-v ${cls || ""}">${v}</div><div class="tile-l">${l}</div></div>`;
  $("#turns-tiles").innerHTML =
    tile(fmtVal(total), turnsPriced ? "누적 비용" : "누적 토큰") +
    tile(turnsPriced ? `$${saved.toFixed(2)}` : fmtTok(readTot), turnsPriced ? "캐시가 아낀 돈" : "캐시로 읽은 토큰") +
    tile(`${hit}%`, "캐시 적중률") +
    (turnsPriced
      ? tile(String(misses.length), "캐시 끊김", misses.length ? "hot" : "")
      : tile(String(ts.length), "응답 수"));

  renderCurve(cost, total, misses);
  renderPrompts(cost);
  $("#turns-all").innerHTML = turnTable(ts.map((_, i) => i), cost);
}

// 차트는 두 겹이다. 막대는 그 턴 하나가 쓴 돈, 얇은 선은 거기까지의 누적.
// 막대만 두면 전체가 어디까지 갔는지 모르고, 누적선만 두면 어느 구간이 비쌌는지
// 기울기로 읽어야 해서 눈에 안 들어온다. 막대 높이는 제곱근 축이다 — 상한을 두면
// 비싼 턴들이 전부 같은 높이로 뭉개지고, 선형이면 작은 턴이 전부 바닥에 깔린다.
function renderCurve(cost, total, misses) {
  const n = cost.length;
  const denom = total || 1;
  const peak = Math.max(...cost) || 1;
  const acc = [];
  let run = 0;
  for (const c of cost) {
    run += c;
    acc.push(run);
  }
  const x = (i) => (n > 1 ? (i / (n - 1)) * 100 : 0);
  const y = (i) => 100 - (acc[i] / denom) * 100;
  const bw = n > 1 ? 100 / n : 100;
  const missSet = new Set(misses);
  const bars = cost
    .map((c, i) => {
      const h = Math.max(0.8, Math.sqrt(c / peak) * 100);
      return `<rect class="tc-b${missSet.has(i) ? " miss" : ""}" x="${(i * bw).toFixed(3)}" ` +
        `y="${(100 - h).toFixed(3)}" width="${Math.max(bw * 0.9, 0.12).toFixed(3)}" height="${h.toFixed(3)}" />`;
    })
    .join("");
  const pts = cost.map((_, i) => `${x(i).toFixed(3)},${y(i).toFixed(3)}`).join(" ");
  $("#turns-chart").innerHTML =
    `<svg viewBox="0 0 100 100" preserveAspectRatio="none">${bars}` +
    `<polyline points="${pts}" /></svg>` +
    `<i class="tc-cursor" hidden></i><i class="tc-span" hidden></i><div class="tc-tip" hidden></div>`;
  $("#turns-curve-head").textContent =
    `${turnsPriced ? "비용" : "토큰"} — 막대는 응답 하나(${n}개), 얇은 선은 누적` +
    (misses.length ? `, 빨간 막대는 캐시가 끊긴 턴` : "");

  // 마우스 x를 턴 번호로 바꿔 그 지점의 값을 띄운다 (막대마다 title을 다는 것보다 가볍다)
  const box = $("#turns-chart");
  const tip = box.querySelector(".tc-tip");
  const cur = box.querySelector(".tc-cursor");
  box.onmousemove = (e) => {
    const r = box.getBoundingClientRect();
    const f = Math.min(1, Math.max(0, (e.clientX - r.left) / r.width));
    const i = Math.min(n - 1, Math.floor(f * n));
    const t = turnsData[i];
    cur.hidden = false;
    cur.style.left = `${(i + 0.5) * bw}%`;
    tip.hidden = false;
    tip.innerHTML =
      `<b>${turnTime(t.ts)}</b> · 이 턴 ${fmtVal(cost[i], true)} / 여기까지 ${fmtVal(acc[i])}<br>` +
      `${escapeHtml(firstLine(mdPlain(t.text), 60) || (t.tools[0] ? t.tools[0] : "도구 호출"))}`;
    const w = tip.offsetWidth || 220;
    tip.style.left = `${Math.min(Math.max(e.clientX - r.left - w / 2, 0), r.width - w)}px`;
  };
  box.onmouseleave = () => {
    tip.hidden = true;
    cur.hidden = true;
  };
}

// 질문 줄에 마우스를 올리면 그 질문이 차지한 구간을 차트에 칠해 준다 —
// 표의 한 줄과 차트의 봉우리를 눈으로 잇는 유일한 연결이다.
function highlightSpan(g) {
  const span = $("#turns-chart").querySelector(".tc-span");
  if (!span) return;
  if (!g) {
    span.hidden = true;
    return;
  }
  const n = turnsData.length;
  const bw = n > 1 ? 100 / n : 100;
  const lo = Math.min(...g.idxs);
  const hi = Math.max(...g.idxs);
  span.hidden = false;
  span.style.left = `${lo * bw}%`;
  span.style.width = `${Math.max((hi - lo + 1) * bw, 0.4)}%`;
}

// 도구 호출 루프 때문에 한 질문이 턴 여러 개를 만든다. 그걸 다시 묶어야
// "어떤 질문이 비쌌나"가 보이고, 답변까지 붙여야 그게 무슨 질문이었는지 안다.
function renderPrompts(cost) {
  const by = new Map();
  turnsData.forEach((t, i) => {
    const g = by.get(t.prompt_idx) || { prompt: t.prompt, cost: 0, idxs: [], miss: 0, answer: "" };
    g.cost += cost[i];
    g.idxs.push(i);
    if (isCacheMiss(t, i)) g.miss += 1;
    if (t.text && t.text.trim()) g.answer = t.text; // 마지막 말이 그 질문의 답
    by.set(t.prompt_idx, g);
  });
  turnGroups = [...by.values()].filter((g) => g.prompt).sort((a, b) => b.cost - a.cost).slice(0, 12);
  $("#turns-prompts-head").textContent =
    `질문별 ${turnsPriced ? "비용" : "토큰"} — 많이 쓴 순 상위 ${turnGroups.length}개 ` +
    `(질문 ${by.size}개, 줄을 누르면 무슨 일을 했는지 펼쳐집니다)`;
  if (!turnGroups.length) {
    $("#turns-prompts").innerHTML = `<div class="dash-note">질문 기록이 없습니다</div>`;
    return;
  }
  const rows = turnGroups
    .map((g, gi) => {
      const head =
        `<tr class="tp-row${g.miss ? " turn-miss" : ""}" data-g="${gi}">` +
        `<td class="tp-q" title="${escapeHtml(g.prompt)}">${escapeHtml(firstLine(mdPlain(g.prompt), 70))}</td>` +
        `<td class="tp-a" title="${escapeHtml(g.answer)}">${escapeHtml(firstLine(mdPlain(g.answer), 70) || "—")}</td>` +
        `<td>${g.idxs.length}</td><td>${fmtVal(g.cost)}</td></tr>`;
      const trail =
        `<tr class="tp-detail hidden" data-d="${gi}"><td colspan="4">${trailOf(g, cost)}</td></tr>`;
      return head + trail;
    })
    .join("");
  $("#turns-prompts").innerHTML =
    `<table><thead><tr><th>질문</th><th>답변</th><th>턴</th><th>${turnsPriced ? "비용" : "토큰"}</th></tr></thead><tbody>${rows}</tbody></table>`;
  $("#turns-prompts").querySelectorAll(".tp-row").forEach((tr) => {
    tr.onmouseenter = () => highlightSpan(turnGroups[tr.dataset.g]);
    tr.onmouseleave = () => highlightSpan(null);
    tr.onclick = () => {
      const d = $("#turns-prompts").querySelector(`.tp-detail[data-d="${tr.dataset.g}"]`);
      if (d) d.classList.toggle("hidden");
      tr.classList.toggle("open");
    };
  });
}

// 한 질문이 만든 턴들의 흔적 — 무슨 말을 했고 어떤 도구를 몇 번 불렀는지.
// "Read 40번"만으로는 알 수 없으니 도구 이름에 인자 앞부분을 붙여 둔다.
function trailOf(g, cost) {
  const lines = g.idxs
    .map((i) => {
      const t = turnsData[i];
      const what = t.text && t.text.trim()
        ? `<span class="tr-say">${escapeHtml(firstLine(mdPlain(t.text), 90))}</span>`
        : "";
      const tools = (t.tools || [])
        .map((x) => `<code class="tr-tool">${escapeHtml(x)}</code>`)
        .join(" ");
      if (!what && !tools) return "";
      return `<div class="tr-line${isCacheMiss(t, i) ? " miss" : ""}">` +
        `<span class="tr-cost">${fmtVal(cost[i], true)}</span>${what}${what && tools ? " " : ""}${tools}</div>`;
    })
    .filter(Boolean)
    .join("");
  return lines || `<div class="dash-note">기록된 내용이 없습니다</div>`;
}

function turnTable(idxs, cost) {
  if (!idxs.length) return `<div class="dash-note">해당하는 턴이 없습니다</div>`;
  const rows = idxs
    .map((i) => {
      const t = turnsData[i];
      const miss = isCacheMiss(t, i);
      const what = firstLine(mdPlain(t.text), 40) || (t.tools || []).slice(0, 2).join(", ");
      return `<tr class="${miss ? "turn-miss" : ""}"><td>${turnTime(t.ts)}</td>` +
        `<td class="tp-q" title="${escapeHtml(t.prompt || "")}">${escapeHtml(firstLine(t.prompt, 40))}</td>` +
        `<td class="tp-q" title="${escapeHtml(what)}">${escapeHtml(what)}</td>` +
        `<td>${fmtTok(t.input)}</td>` +
        `<td>${fmtTok(t.cache_read)}</td><td>${fmtTok(t.cache_5m + t.cache_1h)}</td>` +
        `<td>${fmtTok(t.output)}</td><td>${fmtVal(cost[i], true)}</td><td>${miss ? "캐시 끊김" : ""}</td></tr>`;
    })
    .join("");
  return `<table><thead><tr><th>시각</th><th>질문</th><th>한 일</th><th>입력</th><th>캐시 읽기</th>` +
    `<th>캐시 쓰기</th><th>출력</th><th>${turnsPriced ? "비용" : "토큰"}</th><th></th></tr></thead><tbody>${rows}</tbody></table>`;
}

$("#turns-close").onclick = () => closeModal($("#turns-backdrop"));
$("#turns-toggle-all").onclick = () => {
  const el = $("#turns-all");
  const shown = !el.classList.toggle("hidden");
  $("#turns-toggle-all").textContent = shown ? "전체 턴 표 접기" : "전체 턴 표 보기";
};

// ---------- 요금제 한도 위젯 (사이드바 하단) ----------
function fmtRemain(iso) {
  const ms = new Date(iso) - Date.now();
  if (isNaN(ms)) return "";
  if (ms <= 0) return "리셋됨";
  const h = Math.floor(ms / 3600000);
  const m = Math.round((ms % 3600000) / 60000);
  // 주간 창은 최대 168시간이라 시간 단위로만 쓰면 "90시간"처럼 읽기 나쁜 값이 나온다
  if (h >= 24) {
    const d = Math.floor(h / 24);
    const rh = h % 24;
    return rh > 0 ? `${d}일 ${rh}시간` : `${d}일`;
  }
  return h > 0 ? `${h}시간 ${m}분` : `${m}분`;
}

function limitRow(label, w) {
  // 리셋 시각을 지나면 API가 다음 폴링까지 리셋 전 utilization을 그대로 캐시해
  // 내려주는 경우가 있음(headroom도 동일 현상을 관측해 표시 시점에 0으로 보정함) —
  // "85% · 리셋됨" 같은 모순 표시를 막기 위해 리셋 경과 시 0%로 취급한다.
  const resetPassed = w.resets_at && new Date(w.resets_at) - Date.now() <= 0;
  const pct = resetPassed ? 0 : Math.round(w.utilization_pct ?? 0);
  const cls = pct >= 90 ? "hot" : pct >= 70 ? "warm" : "";
  const remain = w.resets_at ? fmtRemain(w.resets_at) : "";
  return `
    <div class="limit-row" title="${label} 한도 ${pct}% 사용 · 리셋: ${w.resets_at || "?"}">
      <span class="limit-label">${label}</span>
      <span class="limit-bar"><span class="limit-fill ${cls}" style="width:${Math.min(100, pct)}%"></span></span>
      <span class="limit-txt">${pct}%${remain ? ` · ${remain}` : ""}</span>
    </div>`;
}

function codexWinLabel(minutes) {
  if (!minutes) return "한도";
  if (minutes >= 1440 * 6) return "주간";
  if (minutes % 60 === 0) return `${minutes / 60}시간`;
  return `${minutes}분`;
}

async function updateLimits(force = false) {
  const wrap = $("#foot-limits");
  const rowsEl = $("#foot-limits-rows");
  let html = "";
  // Claude (OAuth 사용량 API / headroom 폴백)
  try {
    const s = await invoke("subscription_state", { force });
    if (s && s.five_hour) {
      const cg = AGENT_GLYPH.claude;
      html += limitRow(`${cg} 5시간`, s.five_hour) + limitRow(`${cg} 주간`, s.seven_day || {});
      // 모델별 주간 한도 (Fable 등) — 전체 주간과 별도로 소진된다
      for (const w of s.scoped || []) {
        if (w.utilization_pct == null) continue;
        html += limitRow(`${AGENT_GLYPH.claude} 주간 ${w.label}`, w);
      }
    }
  } catch { /* 무시 */ }
  // Codex (최근 rollout의 token_count 이벤트 — 마지막 codex 사용 시점 기준)
  try {
    const c = await invoke("codex_state");
    const rl = c && c.rate_limits;
    for (const key of ["primary", "secondary"]) {
      const w = rl && rl[key];
      if (!w || w.used_percent == null) continue;
      // 리셋 시각이 이미 지난(만료된) 윈도우는 의미 없는 옛 데이터 → 숨김
      if (!w.resets_at || w.resets_at * 1000 < Date.now()) continue;
      html += limitRow(`${AGENT_GLYPH.codex} ${codexWinLabel(w.window_minutes)}`, {
        utilization_pct: w.used_percent,
        resets_at: new Date(w.resets_at * 1000).toISOString(),
      });
    }
  } catch { /* 무시 */ }
  if (html) {
    rowsEl.innerHTML = html;
    wrap.classList.remove("hidden");
  } else {
    wrap.classList.add("hidden");
  }
}
setTimeout(updateLimits, 2500);
setInterval(updateLimits, 120_000);

// 수동 즉시 갱신 — 캐시를 건너뛰고 강제로 다시 조회. 연타로 429를 유발하지 않도록
// 클릭 직후 짧게 비활성화한다.
const btnRefreshLimits = $("#btn-refresh-limits");
btnRefreshLimits.onclick = async () => {
  btnRefreshLimits.disabled = true;
  btnRefreshLimits.classList.add("spinning");
  try {
    await updateLimits(true);
  } finally {
    setTimeout(() => {
      btnRefreshLimits.disabled = false;
      btnRefreshLimits.classList.remove("spinning");
    }, 10_000);
  }
};

// 숨겨져 있는 동안 건너뛴 갱신을 다시 보이는 순간 한 번에 따라잡는다
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) refreshSessions();
});

// 주기적 목록 갱신 (20초)
setInterval(refreshSessions, 20000);
refreshSessions();
