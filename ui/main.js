// Claude Deck — 프런트엔드: 세션 사이드바 + PTY 터미널 관리
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const TERM_THEME = {
  background: "#262624",
  foreground: "#e8e6e3",
  cursor: "#d97757",
  cursorAccent: "#262624",
  selectionBackground: "#4a463f",
  black: "#33312e", red: "#e05d5d", green: "#87b387", yellow: "#d9a057",
  blue: "#6a9bcc", magenta: "#b58dae", cyan: "#6aa8a8", white: "#c9c6c0",
  brightBlack: "#6e6a63", brightRed: "#ef8080", brightGreen: "#a3cba3",
  brightYellow: "#e8b878", brightBlue: "#8cb4dd", brightMagenta: "#cba6c4",
  brightCyan: "#8cc2c2", brightWhite: "#e8e6e3",
};

// ---------- 상태 ----------
let sessions = [];               // 스캔된 세션 메타
const terms = new Map();         // id -> { term, fit, container, title, cwd, exited }
let tabOrder = [];               // 탭 표시 순서 (드래그로 변경 가능)
let activeId = null;

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
  sessions = await invoke("list_sessions");
  renderSidebar();
}

function renderSidebar() {
  const q = $("#search").value.trim().toLowerCase();
  listEl.innerHTML = "";
  let shown = 0;
  for (const s of sessions) {
    const title = s.summary || s.first_prompt || "(내용 없음)";
    const proj = basename(s.cwd);
    if (q && !(title.toLowerCase().includes(q) || proj.toLowerCase().includes(q))) continue;
    shown++;

    const el = document.createElement("div");
    el.className = "session-item" + (s.session_id === activeId ? " active" : "");
    const t = terms.get(s.session_id);
    const dot = t ? `<span class="si-dot ${statusClass(t)}" title="${statusLabel(t)}"></span>` : "";
    el.innerHTML = `
      ${dot}
      <div class="si-title"></div>
      <div class="si-meta">
        <span class="si-proj"></span>
        <span>${timeAgo(s.mtime)}</span>
        <span>· ${s.message_count}</span>
      </div>`;
    el.querySelector(".si-title").textContent = title;
    el.querySelector(".si-proj").textContent = proj;
    el.title = `${s.cwd}\n${s.session_id}`;
    el.onclick = () => openSession(s);
    listEl.appendChild(el);
  }
  const busyN = [...terms.values()].filter((t) => !t.exited && t.busy).length;
  const runN = [...terms.values()].filter((t) => !t.exited).length;
  $("#foot-count").textContent = `세션 ${shown}개 · 실행 ${runN} · 답변중 ${busyN}`;
}

function statusClass(t) {
  return t.exited ? "exited" : t.busy ? "busy" : "idle";
}
function statusLabel(t) {
  return t.exited ? "종료됨" : t.busy ? "답변/작업 중" : "대기 중";
}

// 1초마다 답변중/idle 판정 (최근 2초 내 PTY 출력 = 답변중)
setInterval(() => {
  const now = Date.now();
  let changed = false;
  for (const t of terms.values()) {
    const busy = !t.exited && now - t.lastOutput < 2000;
    if (busy !== t.busy) {
      t.busy = busy;
      changed = true;
    }
  }
  if (changed) {
    renderTabs();
    renderSidebar();
  }
}, 1000);

// ---------- 터미널 ----------
function makeTerm(id, title, cwd) {
  const container = document.createElement("div");
  container.className = "term-container";
  termArea.appendChild(container);

  const term = new Terminal({
    fontFamily: '"Cascadia Mono", Consolas, "D2Coding", monospace',
    fontSize: 13.5,
    lineHeight: 1.25,
    letterSpacing: 0,
    cursorBlink: true,
    scrollback: 8000,
    theme: TERM_THEME,
    allowProposedApi: true,
  });
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  term.open(container);

  term.onData((d) => invoke("write_pty", { id, data: d }));

  // Ctrl+V 붙여넣기 / 선택시 Ctrl+C 복사
  term.attachCustomKeyEventHandler((e) => {
    if (e.type !== "keydown") return true;
    if (e.ctrlKey && e.key === "v") {
      navigator.clipboard.readText().then((t) => t && term.paste(t));
      return false;
    }
    if (e.ctrlKey && e.key === "c" && term.hasSelection()) {
      navigator.clipboard.writeText(term.getSelection());
      term.clearSelection();
      return false;
    }
    return true;
  });

  const entry = { term, fit, container, title, cwd, exited: false, busy: false, lastOutput: Date.now(), agent: null, wrapper: null };
  terms.set(id, entry);
  tabOrder.push(id);
  return entry;
}

async function openSession(meta) {
  const id = meta.session_id;
  if (terms.has(id)) return activate(id);

  const title = basename(meta.cwd) + " · " + (meta.summary || meta.first_prompt || id.slice(0, 8)).slice(0, 24);
  const entry = makeTerm(id, title, meta.cwd);
  entry.agent = currentAgent();
  entry.wrapper = currentWrapper();
  activate(id);
  await invoke("spawn_pty", {
    id, cwd: meta.cwd, command: composeCommand(id),
    cols: entry.term.cols, rows: entry.term.rows,
  });
}

async function openNewSession(cwd) {
  const id = "new-" + Date.now();
  const entry = makeTerm(id, basename(cwd) + " · 새 세션", cwd);
  entry.agent = currentAgent();
  entry.wrapper = currentWrapper();
  activate(id);
  await invoke("spawn_pty", {
    id, cwd, command: composeCommand(null),
    cols: entry.term.cols, rows: entry.term.rows,
  });
  setTimeout(refreshSessions, 4000);
}

function activate(id) {
  activeId = id;
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
  t.term.dispose();
  t.container.remove();
  terms.delete(id);
  tabOrder = tabOrder.filter((x) => x !== id);
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
  if (isDraggingTab) return;   // 드래그 중 재렌더 금지 (상태 갱신·이벤트가 드래그를 깨뜨리지 않게)
  tabsEl.innerHTML = "";
  for (const id of tabOrder) {
    const t = terms.get(id);
    if (!t) continue;
    const el = document.createElement("div");
    el.className = "tab" + (id === activeId ? " active" : "") + (t.exited ? " exited" : "");
    el.dataset.id = id;
    const agentBadge = t.agent && t.agent.cmd !== "claude";
    const wrapBadge = t.wrapper && t.wrapper.prefix;
    el.innerHTML = `<span class="tab-dot ${statusClass(t)}" title="${statusLabel(t)}"></span><span class="tab-label"></span>${agentBadge ? '<span class="tab-badge agent"></span>' : ""}${wrapBadge ? '<span class="tab-badge"></span>' : ""}<button class="tab-close" title="닫기">✕</button>`;
    el.querySelector(".tab-label").textContent = t.title;
    if (agentBadge) {
      const b = el.querySelector(".tab-badge.agent");
      b.textContent = t.agent.name.slice(0, 10);
      b.title = t.agent.cmd;
    }
    if (wrapBadge) {
      const b = el.querySelector(".tab-badge:not(.agent)");
      b.textContent = t.wrapper.name.slice(0, 10);
      b.title = t.wrapper.prefix;
    }
    el.onclick = () => { if (!suppressClick) activate(id); };
    el.querySelector(".tab-close").onclick = (e) => { e.stopPropagation(); closeTab(id); };
    makeTabDraggable(el);
    tabsEl.appendChild(el);
  }
}

// 포인터 기반 탭 드래그 — 잡은 탭은 커서를 따라가고, 나머지는 트랜지션으로 밀려남.
// 판정 기준은 드래그 시작 시점의 고정 좌표(rects)라서 진동이 없음.
let suppressClick = false;
let isDraggingTab = false;

function makeTabDraggable(el) {
  el.addEventListener("pointerdown", (e) => {
    if (e.button !== 0 || e.target.classList.contains("tab-close")) return;
    const startX = e.clientX;
    let dragging = false;
    let tabs = [], rects = [], origIndex = 0, newIndex = 0;

    const move = (ev) => {
      if (!dragging && Math.abs(ev.clientX - startX) > 6) {
        dragging = true;
        isDraggingTab = true;
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

    const up = () => {
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
      if (!dragging) return;
      isDraggingTab = false;
      tabsEl.classList.remove("drag-active");
      const id = el.dataset.id;
      tabOrder = tabOrder.filter((x) => x !== id);
      tabOrder.splice(newIndex, 0, id);
      suppressClick = true;                 // 드래그 직후의 click은 무시
      setTimeout(() => (suppressClick = false), 0);
      renderTabs();                         // 재렌더로 transform 초기화 + 순서 확정
    };

    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
  });
}

// ---------- PTY 이벤트 ----------
listen("pty-output", (ev) => {
  const { id, data } = ev.payload;
  const t = terms.get(id);
  if (t) {
    t.term.write(b64ToBytes(data));
    t.lastOutput = Date.now();
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

// ---------- 리사이즈 ----------
const ro = new ResizeObserver(() => {
  if (!activeId) return;
  const t = terms.get(activeId);
  if (!t) return;
  t.fit.fit();
  invoke("resize_pty", { id: activeId, cols: t.term.cols, rows: t.term.rows });
});
ro.observe(termArea);

// ---------- 에이전트 / 래퍼 (직교하는 두 축) ----------
// 에이전트 = 무엇을 실행하나 (claude, codex …).  래퍼 = 어떻게 실행하나 (headroom wrap …)
const DEFAULT_AGENTS = [{ name: "Claude", cmd: "claude", resume: true }];
const DEFAULT_WRAPPERS = [
  { name: "없음", prefix: "" },
  { name: "Headroom", prefix: "headroom wrap" },
];

function loadList(key, fallback) {
  try {
    const v = JSON.parse(localStorage.getItem(key));
    if (Array.isArray(v) && v.length) return v;
  } catch { /* 무시 */ }
  return fallback.map((x) => ({ ...x }));
}
let agents = loadList("agents", DEFAULT_AGENTS);
let wrappers = loadList("wrappers", DEFAULT_WRAPPERS);

const agentSelect = $("#agent-select");
const wrapperSelect = $("#wrapper-select");

function fillSelect(sel, items, titleFn, savedKey) {
  sel.innerHTML = "";
  items.forEach((x, i) => {
    const o = document.createElement("option");
    o.value = i;
    o.textContent = x.name;
    o.title = titleFn(x);
    sel.appendChild(o);
  });
  const saved = parseInt(localStorage.getItem(savedKey) || "0", 10);
  sel.value = String(Math.min(isNaN(saved) ? 0 : saved, items.length - 1));
}
function renderSelectors() {
  fillSelect(agentSelect, agents, (a) => a.cmd, "agentSel");
  fillSelect(wrapperSelect, wrappers, (w) => w.prefix || "(래퍼 없음)", "wrapperSel");
}
renderSelectors();
agentSelect.onchange = () => localStorage.setItem("agentSel", agentSelect.value);
wrapperSelect.onchange = () => localStorage.setItem("wrapperSel", wrapperSelect.value);

function currentAgent() {
  return agents[parseInt(agentSelect.value, 10)] || DEFAULT_AGENTS[0];
}
function currentWrapper() {
  return wrappers[parseInt(wrapperSelect.value, 10)] || DEFAULT_WRAPPERS[0];
}

// 최종 실행 명령 합성: [래퍼 접두사] + 에이전트 명령 + [--resume <세션ID>]
function composeCommand(resumeId) {
  const a = currentAgent();
  const w = currentWrapper();
  let cmd = a.cmd;
  if (resumeId && a.resume !== false) cmd += ` --resume ${resumeId}`;
  if (w.prefix) cmd = `${w.prefix} ${cmd}`;
  return cmd;
}

// 편집 모달
function addAgentRow(name = "", cmd = "", resume = true) {
  const row = document.createElement("div");
  row.className = "lrow";
  row.innerHTML = `
    <input class="l-name" placeholder="이름" spellcheck="false" />
    <input class="l-cmd" placeholder="명령 (예: claude, codex)" spellcheck="false" />
    <label class="l-resume" title="세션 재개 시 --resume <세션ID> 인자를 붙일지"><input type="checkbox" />resume</label>
    <button class="l-del" title="삭제">✕</button>`;
  row.querySelector(".l-name").value = name;
  row.querySelector(".l-cmd").value = cmd;
  row.querySelector(".l-resume input").checked = resume;
  row.querySelector(".l-del").onclick = () => row.remove();
  $("#agent-list").appendChild(row);
}
function addWrapperRow(name = "", prefix = "") {
  const row = document.createElement("div");
  row.className = "lrow";
  row.innerHTML = `
    <input class="l-name" placeholder="이름" spellcheck="false" />
    <input class="l-cmd" placeholder="접두사 (예: headroom wrap)" spellcheck="false" />
    <button class="l-del" title="삭제">✕</button>`;
  row.querySelector(".l-name").value = name;
  row.querySelector(".l-cmd").value = prefix;
  row.querySelector(".l-del").onclick = () => row.remove();
  $("#wrapper-list").appendChild(row);
}

$("#btn-launcher-edit").onclick = () => {
  $("#agent-list").innerHTML = "";
  $("#wrapper-list").innerHTML = "";
  for (const a of agents) addAgentRow(a.name, a.cmd, a.resume !== false);
  for (const w of wrappers) addWrapperRow(w.name, w.prefix);
  $("#lmodal-backdrop").classList.remove("hidden");
};
$("#agent-add").onclick = () => addAgentRow();
$("#wrapper-add").onclick = () => addWrapperRow();
$("#lmodal-cancel").onclick = () => $("#lmodal-backdrop").classList.add("hidden");
$("#lmodal-save").onclick = () => {
  const nextAgents = [...document.querySelectorAll("#agent-list .lrow")]
    .map((r) => ({
      name: r.querySelector(".l-name").value.trim(),
      cmd: r.querySelector(".l-cmd").value.trim(),
      resume: r.querySelector(".l-resume input").checked,
    }))
    .filter((a) => a.name && a.cmd);
  const nextWrappers = [...document.querySelectorAll("#wrapper-list .lrow")]
    .map((r) => ({
      name: r.querySelector(".l-name").value.trim(),
      prefix: r.querySelector(".l-cmd").value.trim(),
    }))
    .filter((w) => w.name);
  agents = nextAgents.length ? nextAgents : DEFAULT_AGENTS.map((x) => ({ ...x }));
  wrappers = nextWrappers.length ? nextWrappers : DEFAULT_WRAPPERS.map((x) => ({ ...x }));
  localStorage.setItem("agents", JSON.stringify(agents));
  localStorage.setItem("wrappers", JSON.stringify(wrappers));
  renderSelectors();
  $("#lmodal-backdrop").classList.add("hidden");
};

// ---------- UI 바인딩 ----------
$("#btn-refresh").onclick = refreshSessions;
$("#search").oninput = renderSidebar;

$("#btn-new").onclick = () => {
  $("#modal-backdrop").classList.remove("hidden");
  $("#modal-path").focus();
};
$("#modal-cancel").onclick = () => $("#modal-backdrop").classList.add("hidden");
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

// Ctrl+Tab 탭 순환
window.addEventListener("keydown", (e) => {
  if (e.ctrlKey && e.key === "Tab") {
    e.preventDefault();
    const ids = tabOrder;
    if (ids.length < 2) return;
    const i = ids.indexOf(activeId);
    activate(ids[(i + (e.shiftKey ? -1 : 1) + ids.length) % ids.length]);
  }
});

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

// 주기적 목록 갱신 (20초)
setInterval(refreshSessions, 20000);
refreshSessions();
