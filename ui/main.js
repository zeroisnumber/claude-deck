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
  const prof = currentProfile();
  const profTag = prof.cmd !== "claude" ? ` · ${prof.name}` : "";
  $("#foot-count").textContent = `세션 ${shown}개 · 실행 ${runN} · 답변중 ${busyN}${profTag}`;
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

  // 한글 IME 조합 확정이 간헐적으로 중복 전달되는 xterm.js 버그 완화:
  // 동일한 한글 포함 청크가 25ms 내 연속 도착하면 중복으로 보고 무시
  let lastData = "", lastDataTime = 0;
  term.onData((d) => {
    const now = performance.now();
    if (d === lastData && now - lastDataTime < 25 && /[ㄱ-힝]/.test(d)) {
      lastDataTime = now;
      return;
    }
    lastData = d;
    lastDataTime = now;
    invoke("write_pty", { id, data: d });
  });

  // 선택 상태에서 Ctrl+C = 복사 (Ctrl+V는 브라우저 네이티브 paste에 맡김 — 중복 방지)
  term.attachCustomKeyEventHandler((e) => {
    if (e.type !== "keydown") return true;
    if (e.ctrlKey && e.key === "c" && term.hasSelection()) {
      navigator.clipboard.writeText(term.getSelection());
      term.clearSelection();
      return false;
    }
    return true;
  });

  const entry = { term, fit, container, title, cwd, exited: false, busy: false, lastOutput: Date.now(), profile: null };
  terms.set(id, entry);
  tabOrder.push(id);
  return entry;
}

async function openSession(meta) {
  const id = meta.session_id;
  if (terms.has(id)) return activate(id);

  const title = basename(meta.cwd) + " · " + (meta.summary || meta.first_prompt || id.slice(0, 8)).slice(0, 24);
  const entry = makeTerm(id, title, meta.cwd);
  entry.profile = currentProfile();
  activate(id);
  await invoke("spawn_pty", {
    id, cwd: meta.cwd, command: composeCommand(id),
    cols: entry.term.cols, rows: entry.term.rows,
  });
}

async function openNewSession(cwd) {
  const id = "new-" + Date.now();
  const entry = makeTerm(id, basename(cwd) + " · 새 세션", cwd);
  entry.profile = currentProfile();
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
    const showBadge = t.profile && t.profile.cmd !== "claude";
    el.innerHTML = `<span class="tab-dot ${statusClass(t)}" title="${statusLabel(t)}"></span><span class="tab-label"></span>${showBadge ? '<span class="tab-badge"></span>' : ""}<button class="tab-close" title="닫기">✕</button>`;
    el.querySelector(".tab-label").textContent = t.title;
    if (showBadge) {
      const b = el.querySelector(".tab-badge");
      b.textContent = t.profile.name.slice(0, 10);
      b.title = t.profile.cmd;
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
