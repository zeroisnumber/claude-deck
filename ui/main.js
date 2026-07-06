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
  restoreTabs(); // 최초 1회만 동작 (이전에 열려 있던 탭 자동 재개)
}

// 별칭 / 핀 (localStorage)
let aliases = {};
try { aliases = JSON.parse(localStorage.getItem("aliases")) || {}; } catch { /* 무시 */ }
let pins = [];
try { pins = JSON.parse(localStorage.getItem("pins")) || []; } catch { /* 무시 */ }

const AGENT_GLYPH = { claude: "✻", codex: "❖", gemini: "✦" };
let agentFilter = "all";

function renderSidebar() {
  const q = $("#search").value.trim().toLowerCase();
  listEl.innerHTML = "";
  let shown = 0;
  const sorted = [...sessions].sort((a, b) => {
    const pa = pins.includes(a.session_id) ? 1 : 0;
    const pb = pins.includes(b.session_id) ? 1 : 0;
    return pb - pa || b.mtime - a.mtime;
  });
  for (const s of sorted) {
    if (agentFilter !== "all" && s.agent !== agentFilter) continue;
    const title = aliases[s.session_id] || s.summary || s.first_prompt || "(내용 없음)";
    const proj = basename(s.cwd);
    if (q && !(title.toLowerCase().includes(q) || proj.toLowerCase().includes(q))) continue;
    shown++;

    const el = document.createElement("div");
    el.className = "session-item" + (s.session_id === activeId ? " active" : "");
    const t = terms.get(s.session_id);
    const dot = t ? `<span class="si-dot ${statusClass(t)}" title="${statusLabel(t)}"></span>` : "";
    const pin = pins.includes(s.session_id) ? `<span class="si-pin">📌</span>` : "";
    const glyph = `<span class="si-agent ${s.agent}" title="${s.agent}">${AGENT_GLYPH[s.agent] || "•"}</span>`;
    el.innerHTML = `
      ${dot}
      <div class="si-title">${pin}${glyph}<span class="si-title-text"></span></div>
      <div class="si-meta">
        <span class="si-proj"></span>
        <span>${timeAgo(s.mtime)}</span>
        <span>· ${s.message_count}</span>
      </div>`;
    el.querySelector(".si-title-text").textContent = title;
    el.querySelector(".si-proj").textContent = proj;
    el.onclick = () => openSession(s);
    el.oncontextmenu = (e) => { e.preventDefault(); showCtxMenu(e, s, el); };
    el.onmouseenter = (e) => schedulePreview(el, s);
    el.onmouseleave = hidePreview;
    listEl.appendChild(el);
  }
  const busyN = [...terms.values()].filter((t) => !t.exited && t.busy).length;
  const runN = [...terms.values()].filter((t) => !t.exited).length;
  const prof = currentProfile();
  const profTag = prof.cmd !== "claude" ? ` · ${prof.name}` : "";
  $("#foot-count").textContent = `세션 ${shown}개 · 실행 ${runN} · 답변중 ${busyN}${profTag}`;
}

// ---------- 세션 우클릭 메뉴 ----------
const ctxMenu = $("#ctx-menu");

function showCtxMenu(e, s, itemEl) {
  hidePreview();
  const pinned = pins.includes(s.session_id);
  ctxMenu.innerHTML = "";
  const items = [
    [pinned ? "📌 핀 해제" : "📌 핀 고정", () => {
      pins = pinned ? pins.filter((x) => x !== s.session_id) : [...pins, s.session_id];
      localStorage.setItem("pins", JSON.stringify(pins));
      renderSidebar();
    }],
    ["✏️ 이름 바꾸기", () => startRename(s, itemEl)],
    ["🗑️ 세션 삭제", async () => {
      try {
        const ok = await window.__TAURI__.dialog.confirm(
          `이 세션 기록을 영구 삭제할까요?\n\n${aliases[s.session_id] || s.summary || s.first_prompt || s.session_id}`,
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
  ctxMenu.style.top = Math.min(e.clientY, window.innerHeight - 120) + "px";
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
  input.focus();
  input.select();
  const commit = () => {
    const v = input.value.trim();
    if (v) aliases[s.session_id] = v;
    else delete aliases[s.session_id];
    localStorage.setItem("aliases", JSON.stringify(aliases));
    renderSidebar();
  };
  input.onkeydown = (e) => {
    if (e.key === "Enter") commit();
    if (e.key === "Escape") renderSidebar();
    e.stopPropagation();
  };
  input.onblur = commit;
  input.onclick = (e) => e.stopPropagation();
}

// ---------- hover 미리보기 ----------
const previewCard = $("#preview-card");
let previewTimer = null;

function schedulePreview(el, s) {
  clearTimeout(previewTimer);
  previewTimer = setTimeout(() => {
    if (!s.last_text && !s.first_prompt) return;
    previewCard.innerHTML = `<div class="pv-title"></div><div class="pv-body"></div><div class="pv-meta"></div>`;
    previewCard.querySelector(".pv-title").textContent = aliases[s.session_id] || s.summary || s.first_prompt || "";
    previewCard.querySelector(".pv-body").textContent = s.last_text || "(응답 없음)";
    previewCard.querySelector(".pv-meta").textContent = `${s.cwd} · ${s.message_count}개 메시지`;
    previewCard.classList.remove("hidden");
    const r = el.getBoundingClientRect();
    previewCard.style.left = r.right + 8 + "px";
    previewCard.style.top = Math.min(r.top, window.innerHeight - 180) + "px";
  }, 350);
}
function hidePreview() {
  clearTimeout(previewTimer);
  previewCard.classList.add("hidden");
}

function statusClass(t) {
  return t.exited ? "exited" : t.busy ? "busy" : "idle";
}
function statusLabel(t) {
  return t.exited ? "종료됨" : t.busy ? "답변/작업 중" : "대기 중";
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

async function notifyDone(id, t) {
  // 활성 탭 + 창 포커스 상태면 사용자가 이미 보고 있음 — 알림 불필요
  const watching = id === activeId && document.hasFocus();
  if (watching) return;
  t.attention = true;
  showToast("✻ 응답 완료", t.title, () => activate(id));
  if (!document.hasFocus()) {
    try {
      const n = window.__TAURI__ && window.__TAURI__.notification;
      if (!n) return;
      let ok = await n.isPermissionGranted();
      if (!ok) ok = (await n.requestPermission()) === "granted";
      if (ok) n.sendNotification({ title: "✻ Claude 응답 완료", body: t.title });
    } catch { /* 무시 */ }
  }
}

// 1초마다 답변중/idle 판정 (최근 2.5초 내 "자발적 출력" = 답변중 — 타이핑 에코 제외)
setInterval(() => {
  const now = Date.now();
  let changed = false;
  for (const [id, t] of terms) {
    const busy = !t.exited && now - t.lastAuto < 2500;
    if (busy !== t.busy) {
      if (busy) {
        t.busySince = now;
      } else if (now - (t.busySince || now) > 5000) {
        // 5초 이상 작업하다 멈춤 = 응답 완료로 판정 (타이핑 에코 등 짧은 활동은 제외)
        notifyDone(id, t);
      }
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
  term.open(container);

  term.onData((d) => {
    const t = terms.get(id);
    if (t) t.lastInput = Date.now();
    invoke("write_pty", { id, data: d });
  });

  // 선택 상태에서 Ctrl+C = 복사 (Ctrl+V는 브라우저 네이티브 paste에 맡김 — 중복 방지)
  // 앱 단축키(Ctrl+Tab/1~9/Shift+W/Shift+N)는 터미널이 먹지 않게 가로챔
  term.attachCustomKeyEventHandler((e) => {
    if (e.type !== "keydown") return true;
    if (handleShortcut(e)) return false;
    if (e.ctrlKey && e.key === "c" && term.hasSelection()) {
      navigator.clipboard.writeText(term.getSelection());
      term.clearSelection();
      return false;
    }
    return true;
  });

  const entry = { term, fit, container, title, cwd, exited: false, busy: false, lastOutput: Date.now(), profile: null, lastInput: 0, lastAuto: 0 };
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

async function openSession(meta, focus = true) {
  const id = meta.session_id;
  if (terms.has(id)) return focus && activate(id);

  const title = basename(meta.cwd) + " · " + (aliases[id] || meta.summary || meta.first_prompt || id.slice(0, 8)).slice(0, 24);
  const entry = makeTerm(id, title, meta.cwd);
  const spec = commandFor(meta);
  entry.profile = spec.profile;
  entry.spawnCommand = spec.cmd;
  if (focus) activate(id);
  else renderTabs();
  await invoke("spawn_pty", {
    id, cwd: meta.cwd, command: entry.spawnCommand,
    cols: entry.term.cols, rows: entry.term.rows,
  });
  saveOpenTabs();
}

async function openNewSession(cwd) {
  const id = "new-" + Date.now();
  const entry = makeTerm(id, basename(cwd) + " · 새 세션", cwd);
  entry.profile = currentProfile();
  entry.spawnCommand = composeCommand(null);
  activate(id);
  await invoke("spawn_pty", {
    id, cwd, command: entry.spawnCommand,
    cols: entry.term.cols, rows: entry.term.rows,
  });
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
  await invoke("spawn_pty", {
    id, cwd: t.cwd, command: t.spawnCommand || "claude",
    cols: t.term.cols, rows: t.term.rows,
  });
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
  t.term.dispose();
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
  if (isDraggingTab) return;   // 드래그 중 재렌더 금지 (상태 갱신·이벤트가 드래그를 깨뜨리지 않게)
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
    el.innerHTML = `<span class="tab-dot ${statusClass(t)}" title="${statusLabel(t)}"></span><span class="tab-label"></span>${showBadge ? '<span class="tab-badge"></span>' : ""}${t.exited ? '<button class="tab-restart" title="다시 시작">↻</button>' : ""}<button class="tab-close" title="닫기">✕</button>${ctxBar}`;
    el.querySelector(".tab-label").textContent = t.title;
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
    const now = Date.now();
    t.lastOutput = now;
    // 최근 입력의 에코가 아닌 "자발적 출력"만 활동으로 인정 (타이핑은 busy 아님)
    if (now - t.lastInput > 800) t.lastAuto = now;
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

// 최종 실행 명령: 프로필 명령 + (재개 시) --resume <세션ID>
function composeCommand(resumeId) {
  const p = currentProfile();
  return resumeId && p.resume !== false ? `${p.cmd} --resume ${resumeId}` : p.cmd;
}

// 설정 모달
function addProfileRow(name = "", cmd = "", resume = true, checked = false) {
  const row = document.createElement("div");
  row.className = "lrow";
  row.innerHTML = `
    <label class="l-active" title="이 프로필 사용"><input type="radio" name="active-profile" /></label>
    <input class="l-name" placeholder="이름" spellcheck="false" />
    <input class="l-cmd" placeholder="실행 명령 (예: headroom wrap claude)" spellcheck="false" />
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
  $("#lmodal-backdrop").classList.remove("hidden");
};
$("#profile-add").onclick = () => addProfileRow();
$("#lmodal-cancel").onclick = () => $("#lmodal-backdrop").classList.add("hidden");
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
  localStorage.setItem("profiles", JSON.stringify(profiles));
  localStorage.setItem("profileSel", String(activeProfile));
  renderSidebar();
  $("#lmodal-backdrop").classList.add("hidden");
};

// ---------- UI 바인딩 ----------
$("#btn-refresh").onclick = refreshSessions;
$("#search").oninput = renderSidebar;

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
  if (e.shiftKey && e.key.toLowerCase() === "w") {
    if (activeId) closeTab(activeId);
    return true;
  }
  if (e.shiftKey && e.key.toLowerCase() === "n") {
    openNewModal();
    return true;
  }
  return false;
}
window.addEventListener("keydown", (e) => {
  if (handleShortcut(e)) e.preventDefault();
});

// Ctrl+휠 폰트 크기 조절
let fontSize = parseFloat(localStorage.getItem("fontSize")) || 13.5;
termArea.addEventListener("wheel", (e) => {
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
}, { passive: false });

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
(async () => {
  try {
    const w = window.__TAURI__.window.getCurrentWindow();
    await w.show();
    await w.setFocus();
  } catch { /* 무시 */ }
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
function rowCost(r) {
  const [i, o] = priceFor(r.model);
  return (r.input * i + r.cache_read * i * 0.1 + r.cache_5m * i * 1.25 + r.cache_1h * i * 2 + r.output * o) / 1e6;
}
function fmtTok(n) {
  if (n >= 1e9) return (n / 1e9).toFixed(1) + "B";
  if (n >= 1e6) return (n / 1e6).toFixed(1) + "M";
  if (n >= 1e3) return (n / 1e3).toFixed(1) + "K";
  return String(n);
}

// --- 탭 컨텍스트 게이지: 세션 jsonl의 마지막 usage에서 컨텍스트 크기 조회 ---
async function updateCtxGauges() {
  let changed = false;
  for (const [id, t] of terms) {
    if (t.exited || id.startsWith("new-")) continue;
    const meta = sessions.find((s) => s.session_id === id);
    if (!meta || meta.agent !== "claude") continue;
    try {
      const u = await invoke("session_usage", { file: meta.file });
      if (u) {
        const pct = Math.min(100, Math.round((u.context_tokens / ctxWindowFor(u.model)) * 100));
        if (pct !== t.ctxPct) {
          t.ctxPct = pct;
          t.ctxTokens = u.context_tokens;
          changed = true;
        }
      }
    } catch { /* 무시 */ }
  }
  if (changed) renderTabs();
}
setInterval(updateCtxGauges, 8000);
setTimeout(updateCtxGauges, 3000);

// --- 대시보드 ---
let dashDays = 7;
let dashRows = [];

function renderDash() {
  const cutoff = new Date(Date.now() - (dashDays - 1) * 86400_000).toISOString().slice(0, 10);
  const rows = dashRows.filter(
    (r) => r.date >= cutoff && r.model && r.input + r.output + r.cache_read + r.cache_5m + r.cache_1h > 0,
  );

  const tot = { input: 0, output: 0, cache_read: 0, cache_w: 0, requests: 0, cost: 0 };
  const byModel = new Map();
  const byProj = new Map();
  for (const r of rows) {
    const cost = rowCost(r);
    tot.input += r.input;
    tot.output += r.output;
    tot.cache_read += r.cache_read;
    tot.cache_w += r.cache_5m + r.cache_1h;
    tot.requests += r.requests;
    tot.cost += cost;
    const m = byModel.get(r.model) || { tok: 0, out: 0, cost: 0, req: 0 };
    m.tok += r.input + r.cache_read + r.cache_5m + r.cache_1h;
    m.out += r.output;
    m.cost += cost;
    m.req += r.requests;
    byModel.set(r.model, m);
    const pName = basename(r.cwd) || r.cwd;
    const p = byProj.get(pName) || { cost: 0, req: 0 };
    p.cost += cost;
    p.req += r.requests;
    byProj.set(pName, p);
  }

  $("#dash-tiles").innerHTML = `
    <div class="tile"><div class="tile-v">$${tot.cost.toFixed(2)}</div><div class="tile-l">추정 비용</div></div>
    <div class="tile"><div class="tile-v">${tot.requests.toLocaleString()}</div><div class="tile-l">요청</div></div>
    <div class="tile"><div class="tile-v">${fmtTok(tot.input + tot.cache_read + tot.cache_w)}</div><div class="tile-l">입력 토큰 (캐시 포함)</div></div>
    <div class="tile"><div class="tile-v">${fmtTok(tot.output)}</div><div class="tile-l">출력 토큰</div></div>
    <div class="tile"><div class="tile-v">${tot.cache_read + tot.input > 0 ? Math.round((tot.cache_read / (tot.cache_read + tot.cache_w + tot.input)) * 100) : 0}%</div><div class="tile-l">캐시 적중률</div></div>`;

  const mkTable = (headers, rowsHtml) =>
    `<table><thead><tr>${headers.map((h) => `<th>${h}</th>`).join("")}</tr></thead><tbody>${rowsHtml}</tbody></table>`;

  $("#dash-models").innerHTML = mkTable(
    ["모델", "요청", "입력", "출력", "비용"],
    [...byModel.entries()]
      .sort((a, b) => b[1].cost - a[1].cost)
      .map(([m, v]) => `<tr><td>${m}</td><td>${v.req.toLocaleString()}</td><td>${fmtTok(v.tok)}</td><td>${fmtTok(v.out)}</td><td>$${v.cost.toFixed(2)}</td></tr>`)
      .join("") || `<tr><td colspan="5">데이터 없음</td></tr>`,
  );

  $("#dash-projects").innerHTML = mkTable(
    ["프로젝트", "요청", "비용"],
    [...byProj.entries()]
      .sort((a, b) => b[1].cost - a[1].cost)
      .slice(0, 12)
      .map(([p, v]) => `<tr><td>${p}</td><td>${v.req.toLocaleString()}</td><td>$${v.cost.toFixed(2)}</td></tr>`)
      .join("") || `<tr><td colspan="3">데이터 없음</td></tr>`,
  );
}

async function openDash() {
  $("#dash-backdrop").classList.remove("hidden");
  $("#dash-tiles").innerHTML = `<div class="tile"><div class="tile-v">…</div><div class="tile-l">집계 중</div></div>`;
  try {
    dashRows = await invoke("usage_stats", { days: 30 });
  } catch {
    dashRows = [];
  }
  renderDash();
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
$("#dash-close").onclick = () => $("#dash-backdrop").classList.add("hidden");
for (const b of document.querySelectorAll("#dash-period .dp")) {
  b.onclick = () => {
    dashDays = parseInt(b.dataset.days, 10);
    document.querySelectorAll("#dash-period .dp").forEach((x) => x.classList.toggle("on", x === b));
    renderDash();
  };
}

// ---------- 요금제 한도 위젯 (사이드바 하단) ----------
function fmtRemain(iso) {
  const ms = new Date(iso) - Date.now();
  if (isNaN(ms)) return "";
  if (ms <= 0) return "리셋됨";
  const h = Math.floor(ms / 3600000);
  const m = Math.round((ms % 3600000) / 60000);
  return h > 0 ? `${h}시간 ${m}분` : `${m}분`;
}

function limitRow(label, w) {
  const pct = Math.round(w.utilization_pct ?? 0);
  const cls = pct >= 90 ? "hot" : pct >= 70 ? "warm" : "";
  const remain = w.resets_at ? fmtRemain(w.resets_at) : "";
  return `
    <div class="limit-row" title="${label} 한도 ${pct}% 사용 · 리셋: ${w.resets_at || "?"}">
      <span class="limit-label">${label}</span>
      <span class="limit-bar"><span class="limit-fill ${cls}" style="width:${Math.min(100, pct)}%"></span></span>
      <span class="limit-txt">${pct}%${remain ? ` · ${remain}` : ""}</span>
    </div>`;
}

async function updateLimits() {
  const el = $("#foot-limits");
  try {
    const s = await invoke("subscription_state");
    if (!s || !s.five_hour) {
      el.classList.add("hidden");
      return;
    }
    el.innerHTML = limitRow("5시간", s.five_hour) + limitRow("주간", s.seven_day || {});
    el.classList.remove("hidden");
  } catch {
    el.classList.add("hidden");
  }
}
setTimeout(updateLimits, 2500);
setInterval(updateLimits, 120_000);

// 주기적 목록 갱신 (20초)
setInterval(refreshSessions, 20000);
refreshSessions();
