// [panes] 탭과 화면 칸. terminal.js 다음.
// 담당: 켤 때 탭 복원·새 탭의 세션 짝 맞추기(adoptFor), 화면 나누기(panes·showTab·focusTab·activate),
//       탭 닫기·탭 줄 그리기·끌어서 순서 바꾸기, PTY 이벤트 수신(출력·세션 이동·파일 끌어 놓기),
//       사이드바 접기, 창 크기·배율 바뀜에 맞추기.

// ---------- 탭 복원 ----------
// 새 세션 탭은 임시 id(new-...)로 산다. 에이전트가 기록 파일을 만들면 그때 진짜 세션
// id가 생기는데, 그걸 붙여두지 않으면 같은 세션이 사이드바에서 "안 열린 것"으로 보여
// 누르는 순간 같은 세션이 하나 더 뜬다(같은 기록 파일에 두 프로세스가 붙는다).
// 탭의 열쇠는 바꾸지 않는다 — PTY 이벤트가 그 id로 오기 때문이다.
function adoptNewTabs() {
  for (const meta of sessions) {
    adoptFor(meta);
  }
}

/// 이 세션이 방금 띄운 새 탭의 것이면 그 탭에 붙이고 탭 id를 돌려준다.
/// 짝은 "같은 폴더 · 같은 에이전트 · 세션보다 먼저 뜬 · 아직 아무 세션도 안 쥔 탭" 중
/// 가장 먼저 뜬 것. 목록(sessions)이 아니라 넘겨받은 세션 하나만 보므로, 폴링이 아직
/// 그 세션을 모르는 순간에 사이드바를 눌러도 짝이 맞는다.
function adoptFor(meta) {
  const [openId] = tabFor(meta.session_id);
  if (openId) return openId;
  const claimed = new Set();
  for (const t of terms.values()) if (t.sessionId) claimed.add(t.sessionId);
  if (claimed.has(meta.session_id)) return null;
  // 후보가 여럿일 수 있다: 같은 폴더에서 새 세션 하나와 복사본 하나를 잇달아 띄우면
  // 둘 다 임시 id에 같은 에이전트·같은 폴더다. 먼저 뜬 탭에 무조건 주면 서로 바뀐다.
  const cand = [];
  for (const id of tabOrder) {
    if (!id.startsWith("new-")) continue;
    const t = terms.get(id);
    if (!t || t.sessionId || t.exited) continue;
    if ((t.agent || "claude") !== (meta.agent || "claude")) continue;
    if (!samePath(meta.cwd, t.cwd)) continue;
    if (meta.mtime < (t.startedAt || 0) - 5) continue;
    if (t.knownAtStart && t.knownAtStart.has(meta.session_id)) continue;
    // pid로 이미 자기 세션을 안 탭은 그 세션만 받는다
    if (t.pidSession && t.pidSession !== meta.session_id) continue;
    cand.push([id, t]);
  }
  if (!cand.length) return null;
  // 0) pid로 확인된 짝이 있으면 그게 답이다
  const byPid = cand.find(([, t]) => t.pidSession === meta.session_id);
  if (byPid) return bindAdopted(byPid[0], meta);
  const copiedFrom = (t) =>
    t.forkOf && t.forkOf.firstPrompt && t.forkOf.firstPrompt === (meta.first_prompt || "");
  // 1) 원본과 첫 질문이 같으면 그 원본을 복사한 탭의 것이다
  let pick = (cand.find(([, t]) => copiedFrom(t)) || [])[0];
  // 2) 아니면 복사본이 아닌 탭에 준다 — 갓 태어난 세션은 복사본 자리를 차지하면 안 된다
  if (!pick) pick = (cand.find(([, t]) => !t.forkOf) || [])[0];
  // 3) 원본에 첫 질문이 없어 비교할 수 없는 복사본 탭에는 붙여 준다 — 근거가 없다고
  //    영영 임시 id로 두면 재시작·복원이 안 되는 탭이 남는다.
  if (!pick) pick = (cand.find(([, t]) => t.forkOf && !t.forkOf.firstPrompt) || [])[0];
  // 4) 첫 질문이 다른 복사본 탭만 남았으면 붙이지 않는다. 그 복사본의 기록은 아직
  //    안 생겼을 뿐이고, 잘못 붙이면 재시작이 남의 세션을 재개한다.
  if (!pick) return null;
  return bindAdopted(pick, meta);
}

function bindAdopted(pick, meta) {
  const t = terms.get(pick);
  t.sessionId = meta.session_id;
  t.file = meta.file;
  // 복사본은 원본의 기록을 그대로 물려받아 제목까지 같다. 갈라져 나온 순간에
  // 이름을 붙여 둬야 사이드바에서 원본과 구분해 쓸 수 있다.
  if (t.forkOf && !aliases[meta.session_id]) {
    const base = (t.name || "").replace(/ \(복사본\)$/, "");
    aliases[meta.session_id] = `${base} (복사본)`.slice(0, 60);
    localStorage.setItem("aliases", JSON.stringify(aliases));
  }
  const name = (sessionTitle(meta) || meta.session_id.slice(0, 8)).slice(0, 40);
  t.name = name;
  t.title = projOf(meta) + " · " + name.slice(0, 24);
  t.proj = projOf(meta);
  // 재시작은 이제 "새 세션 시작"이 아니라 "이 세션 재개"여야 한다. 안 바꾸면 탭 이름은
  // 그대로인데 다른 세션이 뜬다.
  // 재시작은 이제 이 세션의 재개다. 프로필의 "재개" 설정이 꺼져 있어도 붙인다 —
  // 안 그러면 탭 이름은 그대로인데 다른 세션이 뜬다.
  // (스폰 자체가 실패해 exited가 된 탭은 여기까지 오지 않으므로 원래 명령으로 재시도한다.)
  t.spawnCommand = (meta.agent || "claude") === "claude"
    ? composeCommand(meta.session_id, t.profile, true)
    : commandFor(meta).cmd;
  // 완료 판정을 이 세션의 상태·기록 파일로 하게 한다 (그전에는 출력 밀도 추정뿐이다)
  detach("bindSession", invoke("bind_session", {
    id: pick, sessionId: meta.session_id, file: meta.file || "", title: t.title || null,
  }));
  saveOpenTabs();
  renderTabs();
  return pick;
}

// 기록 파일은 첫 질문을 보낸 뒤에야 생긴다. 20초 폴링만 기다리면 사이드바에 그 세션이
// 늦게 뜨고, 탭도 그때까지 임시 id로 남는다. 짝이 맞을 때까지 몇 번만 앞당겨 확인한다 —
// 계속 훑지 않도록 간격을 늘리고, 붙으면 멈춘다.
let ADOPT_CATCHUP_MS = [3000, 8000, 15000, 30000, 60000];

function catchUpAdoption(tabId, step = 0) {
  const t0 = terms.get(tabId);
  if (!t0 || t0.sessionId || t0.exited) return;
  if (step >= ADOPT_CATCHUP_MS.length) {
    // 첫 질문이 언제 올지는 모른다. 여기서 포기하고, 턴이 시작되면 다시 건다.
    t0.catchingUp = false;
    return;
  }
  t0.catchingUp = true;
  setTimeout(async () => {
    const t = terms.get(tabId);
    if (!t || t.sessionId || t.exited) return;   // 닫혔거나 이미 붙었거나 죽었다
    // 폴링이 이미 돌고 있으면 그 결과를 받는다 — 그게 파일 생기기 전에 읽었더라도
    // 다음 차례가 덮는다.
    await refreshSessions();
    const now = terms.get(tabId);
    if (now && !now.sessionId) catchUpAdoption(tabId, step + 1);
    else if (now) now.catchingUp = false;
  }, ADOPT_CATCHUP_MS[step]);
}

// 기록 파일은 첫 질문을 보내야 생긴다. 그 순간(턴 시작)에 따라잡기를 다시 건다 —
// 스폰 직후 몇 초만 보고 그만두면, 한참 있다 질문한 세션은 20초 폴링을 기다리게 된다.
function noteTurnStart(tabId) {
  const t = terms.get(tabId);
  if (!t || t.sessionId || t.exited || t.catchingUp) return;
  if (!tabId.startsWith("new-")) return;
  catchUpAdoption(tabId, 0);
}

function samePath(a, b) {
  const norm = (x) => String(x || "").replace(/[\\/]+/g, "\\").replace(/\\$/, "").toLowerCase();
  return norm(a) === norm(b);
}

/// 세션 id로 열린 탭 찾기 — 새 세션 탭은 열쇠가 임시 id라 Map 조회만으로는 못 찾는다.
function tabFor(sessionId) {
  const direct = terms.get(sessionId);
  if (direct) return [sessionId, direct];
  for (const [id, t] of terms) {
    if (t.sessionId === sessionId) return [id, t];
  }
  return [null, null];
}

function saveOpenTabs() {
  const tabs = tabOrder
    .map((id) => (terms.get(id) || {}).sessionId || id)
    .filter((id) => !id.startsWith("new-"));
  localStorage.setItem("openTabs", JSON.stringify(tabs));
}

let restored = false;
function restoreTabs() {
  if (restored) return;
  restored = true;
  let saved = [];
  try { saved = JSON.parse(localStorage.getItem("openTabs")) || []; } catch { /* 무시 */ }
  const toOpen = saved.map((sid) => sessions.find((s) => s.session_id === sid)).filter(Boolean);
  if (!toOpen.length) return;
  // 묻지 않고 바로 띄우면 탭마다 에이전트가 하나씩 뜬다(클로드 하나가 1GB 가까이 쓴다).
  // 오늘 이어서 할 것만 고르게 한다. 예전처럼 바로 열고 싶으면 한 번 끄면 된다.
  if (localStorage.getItem("restoreAsk") === "0") return openRestored(toOpen);
  askRestore(toOpen);
}

function openRestored(list) {
  const opening = list.map((meta, i) =>
    detach(`restoreTab:${meta.session_id.slice(0, 8)}`, openSession(meta, i === 0)));
  // 나눈 화면이면 다 열린 뒤 칸 배치를 지난번대로 맞춘다
  Promise.allSettled(opening).then(arrangeSavedPanes);
}

function askRestore(list) {
  const card = document.createElement("div");
  card.className = "restore-card";
  card.innerHTML = `<div class="restore-title">지난번에 열려 있던 세션 ${list.length}개</div>
    <div class="restore-list"></div>
    <label class="restore-remember"><input type="checkbox"> 다음부터 묻지 않고 바로 열기</label>
    <div class="restore-actions">
      <button class="btn-ghost btn-sm restore-skip">열지 않기</button>
      <button class="btn-accent btn-sm restore-ok">이어서 열기</button>
    </div>`;
  const box = card.querySelector(".restore-list");
  for (const meta of list) {
    const row = document.createElement("label");
    row.className = "restore-row";
    row.innerHTML = `<input type="checkbox" checked><span class="restore-name"></span><span class="restore-proj"></span>`;
    row.querySelector(".restore-name").textContent = (sessionTitle(meta) || meta.session_id.slice(0, 8)).slice(0, 60);
    row.querySelector(".restore-proj").textContent = projOf(meta);
    row.meta = meta;
    box.appendChild(row);
  }
  const done = () => card.remove();
  card.querySelector(".restore-ok").onclick = () => {
    if (card.querySelector(".restore-remember input").checked) localStorage.setItem("restoreAsk", "0");
    const picked = [...box.children].filter((r) => r.querySelector("input").checked).map((r) => r.meta);
    done();
    openRestored(picked);
  };
  card.querySelector(".restore-skip").onclick = done;
  // 지난번 탭을 묻는 카드가 있으면 그게 먼저다 (최근 세션 목록은 CSS가 감춘다)
  emptyState.querySelector(".empty-inner").prepend(card);
}

// 탭을 화면에 띄우고(showTab) 입력을 그 탭으로 보낸다(focusTab).
// "보이는 탭"과 "입력을 받는 탭(activeId)"은 지금은 같지만 화면을 나누면 갈라진다 —
// 그래서 따로 둔다. 보이는지를 물을 때는 isShown, 입력 대상은 activeId를 쓴다.
function activate(id) {
  showTab(id);
  focusTab(id);
  renderTabs();
  renderSidebar();
}

// ---------- 화면 나누기 ----------
// 칸마다 탭 하나. 칸 위치는 CSS 격자 대신 여기서 계산해 각 터미널에 직접 준다 — 경계를
// 끌어 비율을 바꾸기 쉽고, 2×2의 가로 경계가 두 토막으로 갈리지 않는다.
// 입력은 한 칸(focusPane)으로만 간다. 그 칸의 탭이 activeId다.
const LAYOUT_PANES = { "1": 1, h2: 2, v2: 2, "4": 4 };
let layout = LAYOUT_PANES[localStorage.getItem("layout")] ? localStorage.getItem("layout") : "1";
let panes = [];          // 칸 번호 → 탭 id (null = 빈 칸)
let focusPane = 0;
let split = { x: 0.5, y: 0.5 };
try { Object.assign(split, JSON.parse(localStorage.getItem("split") || "{}")); } catch { /* 기본값 */ }
// 지난번 칸 배치. 복원된 탭이 열리는 대로 제자리에 앉힌다 (한 번 읽고 끝)
let savedPanes = [];
try { savedPanes = JSON.parse(localStorage.getItem("panes") || "[]"); } catch { /* 없음 */ }
let splitDragging = false;

function paneCount() { return LAYOUT_PANES[layout]; }

function paneRects() {
  const { x, y } = split;
  switch (layout) {
    case "h2": return [[0, 0, x, 1], [x, 0, 1 - x, 1]];
    case "v2": return [[0, 0, 1, y], [0, y, 1, 1 - y]];
    case "4": return [[0, 0, x, y], [x, 0, 1 - x, y], [0, y, x, 1 - y], [x, y, 1 - x, 1 - y]];
    default: return [[0, 0, 1, 1]];
  }
}

// 칸 배열을 화면에 반영한다: 보일 터미널·위치·빈 칸 안내·경계·포커스 표시
function applyPanes() {
  const n = paneCount();
  panes = panes.slice(0, n);
  while (panes.length < n) panes.push(null);
  if (focusPane >= n) focusPane = 0;
  shownIds = panes.filter(Boolean);
  const rects = paneRects();
  const place = (el, i) => {
    if (n === 1) { el.style.left = el.style.top = el.style.right = el.style.bottom = ""; return; }
    const [l, t, w, h] = rects[i];
    // 경계 막대(4px) 자리를 양쪽이 2px씩 비운다
    el.style.left = `calc(${l * 100}% + ${l > 0 ? 2 : 0}px)`;
    el.style.top = `calc(${t * 100}% + ${t > 0 ? 2 : 0}px)`;
    el.style.right = `calc(${(1 - l - w) * 100}% + ${l + w < 1 ? 2 : 0}px)`;
    el.style.bottom = `calc(${(1 - t - h) * 100}% + ${t + h < 1 ? 2 : 0}px)`;
  };
  for (const [tid, t] of terms) {
    const i = panes.indexOf(tid);
    t.container.classList.toggle("visible", i >= 0);
    if (i >= 0) place(t.container, i);
  }
  termArea.querySelectorAll(".pane-empty").forEach((el) => el.remove());
  if (terms.size && n > 1) {
    panes.forEach((tid, i) => {
      if (tid) return;
      const el = document.createElement("div");
      el.className = "pane-empty";
      el.dataset.pane = i;
      el.textContent = "탭을 고르면 이 칸에 열립니다";
      // 빈 칸을 골랐으면 키 입력이 옆 칸 터미널로 새지 않게 포커스를 거둔다
      el.onmousedown = (e) => { e.preventDefault(); document.activeElement?.blur(); focusPane = i; markPaneFocus(); };
      place(el, i);
      termArea.appendChild(el);
    });
  }
  // 탭이 하나도 없으면 빈 화면(안내·복원 카드)이 그대로 보여야 한다 — 칸 장식은 끈다
  const on = n > 1 && terms.size > 0;
  const gx = $("#gutter-x"), gy = $("#gutter-y");
  gx.classList.toggle("hidden", !(on && layout !== "v2"));
  gy.classList.toggle("hidden", !(on && layout !== "h2"));
  gx.style.left = `calc(${split.x * 100}% - 2px)`;
  gy.style.top = `calc(${split.y * 100}% - 2px)`;
  termArea.classList.toggle("split", on);
  markPaneFocus();
  document.querySelectorAll("#layout-btns button").forEach((b) =>
    b.classList.toggle("on", b.dataset.layout === layout));
  if (terms.size) localStorage.setItem("panes", JSON.stringify(panes));
}

function markPaneFocus() {
  const on = paneCount() > 1;
  for (const [tid, t] of terms) t.container.classList.toggle("pane-focused", on && panes[focusPane] === tid);
  termArea.querySelectorAll(".pane-empty").forEach((el) =>
    el.classList.toggle("pane-focused", on && +el.dataset.pane === focusPane));
}

// 안 보이는 탭 중 가장 최근에 연 것
function spareTab() {
  return [...tabOrder].reverse().find((x) => !panes.includes(x) && terms.get(x) && !terms.get(x).disposed);
}

// 빈 칸을 안 보이는 최근 탭으로 채운다
function fillEmptyPanes() {
  if (paneCount() === 1) return;
  panes.forEach((tid, i) => {
    if (tid) return;
    const pick = spareTab();
    if (pick) { panes[i] = pick; ensureWebgl(pick); }
  });
  applyPanes();
}

// 켤 때 복원된 탭들을 지난번 칸으로. 탭이 하나씩 열리며 아무 칸에나 앉았던 것을 다 열린
// 뒤 한 번에 제자리로 옮긴다(자리를 맞바꾼다). 이번에 안 연 탭의 칸은 남은 탭으로 채운다.
function arrangeSavedPanes() {
  if (paneCount() === 1) return;
  savedPanes.slice(0, paneCount()).forEach((id, i) => {
    if (!id || !terms.has(id) || panes[i] === id) return;
    const j = panes.indexOf(id);
    if (j >= 0) panes[j] = panes[i];
    panes[i] = id;
    ensureWebgl(id);
  });
  focusPane = Math.max(0, panes.indexOf(activeId));
  fillEmptyPanes();
  // 처음 연 탭이 지난번엔 화면에 없었으면 자리를 내줬다 — 입력은 보이는 탭으로 옮긴다
  if (activeId && !isShown(activeId) && panes.some(Boolean)) {
    focusTab(panes.find(Boolean));
    renderTabs();
    renderSidebar();
  }
}

function setLayout(next) {
  if (!LAYOUT_PANES[next] || next === layout) return;
  layout = next;
  localStorage.setItem("layout", next);
  const n = paneCount();
  // 줄일 때 입력 중인 탭은 남긴다
  if (activeId && panes.indexOf(activeId) >= n) panes[n - 1] = activeId;
  panes = panes.slice(0, n);
  focusPane = Math.max(0, panes.indexOf(activeId));
  applyPanes();
  fillEmptyPanes();
  for (const id of shownIds) ensureWebgl(id);
  renderTabs(); // 다른 칸에 뜬 탭 표시
}

// 경계 끌기. 끄는 동안에는 칸 위치만 옮기고 터미널 크기는 놓을 때 한 번 맞춘다 —
// 프레임마다 맞추면 에이전트가 그때마다 화면 전체를 다시 그린다.
function dragGutter(el, axis) {
  el.addEventListener("pointerdown", (e) => {
    e.preventDefault();
    el.setPointerCapture(e.pointerId);
    splitDragging = true;
    const move = (ev) => {
      const r = termArea.getBoundingClientRect();
      const v = axis === "x" ? (ev.clientX - r.left) / r.width : (ev.clientY - r.top) / r.height;
      split[axis] = Math.min(0.8, Math.max(0.2, v));
      applyPanes();
    };
    const up = () => {
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
      el.removeEventListener("pointercancel", up);
      splitDragging = false;
      localStorage.setItem("split", JSON.stringify(split));
      refitShown();
    };
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
    el.addEventListener("pointercancel", up);
  });
}

function refitShown() {
  for (const [id, t] of shownTabs()) {
    t.fit.fit();
    invoke("resize_pty", { id, cols: t.term.cols, rows: t.term.rows });
  }
}

function showTab(id) {
  const t0 = performance.now();
  // 이미 어느 칸에 떠 있으면 그 칸으로 옮겨 갈 뿐이다. 아니면 입력 중인 칸에 띄운다.
  const at = panes.indexOf(id);
  if (at >= 0) focusPane = at;
  else panes[focusPane] = id;
  applyPanes();
  emptyState.classList.add("hidden");
  // 묻는 카드가 떠 있는데 사이드바에서 직접 골라 열었으면 그걸로 답한 것이다.
  // 남겨 두면 탭을 다 닫았을 때 지난 질문이 다시 나타난다.
  emptyState.querySelector(".restore-card")?.remove();
  ensureWebgl(id);
  const t = terms.get(id);
  const sync = performance.now() - t0;
  requestAnimationFrame(() => {
    // 이 프레임 사이에 탭이 닫혔을 수 있다. 버려진 터미널에 fit()을 걸면 xterm이
    // _isDisposed를 읽다 던진다(트레이스에 20건 남아 있던 예외).
    if (terms.get(id) !== t || t.disposed) return;
    const f0 = performance.now();
    t.fit.fit();
    invoke("resize_pty", { id, cols: t.term.cols, rows: t.term.rows });
    // 탭 전환 때 60~90ms씩 멈칫하는 게 어느 단계인지 가르려고 잰다(전환·GPU 준비 | 맞추기)
    const fit = performance.now() - f0;
    if (traceOn && sync + fit > 5) uiTrace("activate", `${Math.round(sync)}|${Math.round(fit)}`);
  });
}

function focusTab(id) {
  activeId = id;
  const t = terms.get(id);
  if (t) t.attention = false;
  const at = panes.indexOf(id);
  if (at >= 0) focusPane = at;
  markPaneFocus();
  // showTab의 맞추기 뒤에 돈다 (같은 프레임, 등록 순서)
  requestAnimationFrame(() => {
    if (terms.get(id) !== t || !t || t.disposed) return;
    t.term.focus();
  });
}

async function closeTab(id) {
  const t = terms.get(id);
  if (!t) return;
  await invoke("kill_pty", { id });
  // WebGL 애드온이 dispose 도중 던진 적이 있다(트레이스의 _isDisposed). 여기서 멈추면
  // terms에 죽은 항목이 남아 그 세션은 사이드바에서 눌러도 "이미 열려 있음"으로
  // 처리돼 아무 일도 안 일어난다. 정리는 무조건 끝까지 간다.
  t.disposed = true;
  try { t.term.dispose(); } catch (err) { reportFatal(`term.dispose: ${err && err.stack || err}`); }
  ro.unobserve(t.container);
  t.container.remove();
  if (t.gen != null) lastGen.set(id, t.gen);
  terms.delete(id);
  entry_markers.delete(id);
  tabOrder = tabOrder.filter((x) => x !== id);
  glOrder = glOrder.filter((x) => x !== id);
  saveOpenTabs();
  // 닫힌 칸은 안 보이던 최근 탭으로 채운다. 입력 중이 아닌 칸이면 포커스는 그대로 둔다.
  const pi = panes.indexOf(id);
  if (pi >= 0) panes[pi] = null;
  const spare = spareTab();
  if (activeId === id) {
    activeId = null;
    if (spare) { if (pi >= 0) focusPane = pi; activate(spare); }
    else if (panes.some(Boolean)) activate(panes.find(Boolean));
    else { applyPanes(); emptyState.classList.remove("hidden"); }
  } else {
    if (pi >= 0 && spare) { panes[pi] = spare; ensureWebgl(spare); }
    applyPanes();
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
    el.className = "tab" + (id === activeId ? " active" : isShown(id) ? " shown" : "") + (t.exited ? " exited" : "") +
      (t.waiting && !t.exited ? " wait" : t.attention ? " attention" : "");
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
    el.querySelector(".tab-close").onclick = (e) => { e.stopPropagation(); detach("closeTab", closeTab(id)); };
    const rbtn = el.querySelector(".tab-restart");
    if (rbtn) rbtn.onclick = (e) => { e.stopPropagation(); restartTab(id); };
    makeTabDraggable(el);
    tabsEl.appendChild(el);
  }
  // 칩이 먼저 줄어들고(CSS), 그래도 넘치면 보고 있지 않은 탭의 칩을 뺀다
  tabsEl.classList.remove("compact");
  if (tabsEl.scrollWidth > tabsEl.clientWidth) tabsEl.classList.add("compact");
  // 한두 글자만 남은 칩("s", "ti")은 뜻이 없고 어색하다 — 잘리면 아예 숨긴다
  for (const chip of tabsEl.querySelectorAll(".tab-proj")) {
    chip.classList.remove("clipped");
    if (chip.scrollWidth > chip.clientWidth + 1) chip.classList.add("clipped");
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
        saveOpenTabs();                     // 안 하면 다음에 켤 때 옛 순서로 돌아간다
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
  const { id, data, generation } = ev.payload;
  const t = terms.get(id);
  // 다시 열거나 재시작한 탭에는 끈 프로세스가 마저 흘린 출력이 늦게 도착한다.
  // 실행 번호는 계속 커지므로, 끈 실행의 번호 이하는 버린다.
  if (t && t.deadGen != null && generation <= t.deadGen) return;
  if (t) {
    const t0 = performance.now();
    const bytes = b64ToBytes(data);
    if (traceOn && isShown(id)) frameOutBytes += bytes.length;
    // 클로드가 첫 화면을 그리면(한 번에 1KB 넘게 온다) 안내를 거둔다
    if (t.loadingEl && bytes.length > 1000) hideLoading(t);
    t.term.write(bytes);
    const d = performance.now() - t0;
    if (d > 5) uiTrace("term-write", d);
  }
});

// 탐색기에서 파일을 끌어다 놓으면 놓은 칸의 터미널에 경로를 붙여넣는다(Ctrl+V 이미지와
// 같은 길 — 클로드는 이미지 경로를 첨부로 바꾼다). 칸 밖(사이드바 등)에 놓으면 입력 중인
// 탭으로. 끄는 동안 놓일 칸에 테두리를 띄운다. 위치는 물리 픽셀로 온다.
function dropTarget(pos) {
  if (!pos) return activeId;
  const dpr = window.devicePixelRatio || 1;
  const el = document.elementFromPoint(pos.x / dpr, pos.y / dpr);
  const box = el && el.closest(".term-container");
  if (box) for (const [id, t] of terms) if (t.container === box) return id;
  return activeId;
}
function markDrop(id) {
  for (const [tid, t] of terms) t.container.classList.toggle("drop-target", tid === id);
}
function quotePath(p) {
  return /\s/.test(p) ? `"${p}"` : p;
}
listen("tauri://drag-over", (ev) => markDrop(dropTarget(ev.payload && ev.payload.position)));
listen("tauri://drag-leave", () => markDrop(null));
listen("tauri://drag-drop", (ev) => {
  markDrop(null);
  const { paths, position } = ev.payload || {};
  const id = dropTarget(position);
  const t = id && terms.get(id);
  if (!t || t.disposed || t.exited || !paths || !paths.length) return;
  if (id !== activeId) activate(id);
  pasting = true;
  try { t.term.paste(paths.map(quotePath).join(" ")); } finally { pasting = false; }
  t.term.focus();
});

// 새 세션 탭의 클로드가 어느 세션을 잡았는지 Rust가 pid로 알아냈다. 기록 파일이 생긴
// 순서로 짝을 맞추면 같은 폴더의 새 탭 둘이 서로 바뀔 수 있다 — 이게 정답이다.
listen("pty-session", (ev) => {
  const { id, session_id } = ev.payload;
  const t = terms.get(id);
  // 복사본은 처음에 원본 id를 적을 수 있어 기존 방식(첫 질문 비교)에 맡긴다
  if (!t || t.sessionId || t.forkOf) return;
  t.pidSession = session_id;
  adoptNewTabs(); // 목록이 이미 그 세션을 알면 바로 붙는다
});

// 같은 세션을 다른 프로세스가 이어받으면(클로드가 스스로 다시 띄우거나 다른
// 터미널에서 --resume 했을 때) 이 탭의 화면을 그리고 키를 읽던 쪽이 사라진다.
// 겉보기에는 멀쩡해서 한참 뒤에야 알아채므로, 알게 된 순간 띠를 띄운다.
listen("session-moved", (ev) => {
  const { id, from, to } = ev.payload;
  const t = terms.get(id);
  if (!t) return;
  // to가 0이면 주인이 우리 쪽으로 돌아왔다는 뜻 — 경고를 거둔다
  if (!to) {
    if (t.movedBar) { t.movedBar.remove(); t.movedBar = null; }
    return;
  }
  if (t.movedBar) return;
  const bar = document.createElement("div");
  bar.className = "term-moved";
  bar.innerHTML = `<span class="moved-text"></span>
    <button class="btn-accent btn-sm moved-reopen">다시 열기</button>
    <button class="btn-ghost btn-sm moved-kill">그 프로세스도 종료</button>
    <button class="btn-ghost btn-sm moved-hide">닫기</button>`;
  bar.querySelector(".moved-text").innerHTML =
    `이 세션을 다른 프로세스가 이어받았습니다. 이 터미널은 더 이상 세션과 연결되어 있지 않습니다.
     <span class="moved-sub">(${from} → ${to})</span>`;
  bar.querySelector(".moved-reopen").onclick = () => reopenMoved(id);
  bar.querySelector(".moved-kill").onclick = () => killOwner(id, to);
  bar.querySelector(".moved-hide").onclick = () => { bar.remove(); t.movedBar = null; };
  t.container.appendChild(bar);
  t.movedBar = bar;
  t.movedPid = to;
  showToast("⚠ 세션이 옮겨갔습니다", `${t.title} — 탭에서 다시 열 수 있습니다`, () => activate(id), "wait");
});

// 우리 쪽 프로세스를 정리하고 같은 세션을 다시 띄운다. 대화는 --resume으로 이어진다.
async function reopenMoved(id) {
  const t = terms.get(id);
  if (!t) return;
  if (t.movedBar) { t.movedBar.remove(); t.movedBar = null; }
  if (t.gen != null) t.deadGen = t.gen;
  await invoke("kill_pty", { id });
  t.exited = false;
  t.term.write("\r\n\x1b[38;5;244m── 다시 열기 ──\x1b[0m\r\n\r\n");
  activate(id);
  await spawnInto(id, t, "다시 열기 실패");
}

// 세션을 가져간 프로세스를 끝낸다. 남겨 두면 다시 열어도 그쪽이 세션을 쥐고 있다.
// 누르기 전에는 아무것도 하지 않는다 — 우리가 띄우지 않은 프로세스다.
async function killOwner(id, pid) {
  try {
    await invoke("kill_session_owner", { id, pid });
    showToast("프로세스를 종료했습니다", `${pid} — 이제 다시 열면 됩니다`);
  } catch (err) {
    showToast("⚠ 종료하지 못했습니다", String(err));
  }
}

listen("pty-exit", (ev) => {
  const { id } = ev.payload;
  const t = terms.get(id);
  if (t) {
    t.exited = true;
    hideLoading(t);
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
// 경계를 끄는 동안은 맞추지 않는다 — 놓을 때 dragGutter가 한 번 맞춘다.
// 칸 크기가 그대로인 탭은 건너뛴다(다른 칸이 바뀔 때마다 모든 에이전트가 다시 그리지 않게).
const ro = new ResizeObserver(() => {
  if (splitDragging) return;
  for (const [id, t] of shownTabs()) {
    const { cols, rows } = t.term;
    t.fit.fit();
    // 크기가 그대로면 알리지 않는다. 탭을 띄울 때(안 보이다 보임)도 이게 불리는데, 그때는
    // showTab이 이미 알렸다 — 또 보내면 에이전트가 화면을 한 번 더 다시 그린다.
    if (t.term.cols !== cols || t.term.rows !== rows) {
      invoke("resize_pty", { id, cols: t.term.cols, rows: t.term.rows });
    }
  }
});
ro.observe(termArea);

document.querySelectorAll("#layout-btns button").forEach((b) => {
  b.onclick = () => setLayout(b.dataset.layout);
});
dragGutter($("#gutter-x"), "x");
dragGutter($("#gutter-y"), "y");
applyPanes();

// 배율이 다른 모니터로 옮기면 창의 CSS 크기는 거의 그대로라 위 ResizeObserver가 안 불린다.
// xterm은 스스로 글자를 다시 재고 선명하게 그리지만(matchMedia resolution + resize),
// WebGL은 글자 폭을 기기 픽셀에 맞춰 반올림하므로 칸의 CSS 폭이 바뀐다(125%: 7→7.2px).
// 칸 수를 다시 맞추지 않으면 오른쪽 열이 창 밖으로 잘리고 PTY는 옛 크기로 남는다.
// xterm의 리스너가 먼저 돌아 치수를 갱신하도록 한 프레임 뒤에 맞춘다.
let lastDpr = window.devicePixelRatio;
function onDprMaybeChanged() {
  if (window.devicePixelRatio === lastDpr) return;
  lastDpr = window.devicePixelRatio;
  requestAnimationFrame(() => {
    for (const [id, t] of shownTabs()) {
      const { cols, rows } = t.term;
      t.fit.fit();
      if (t.term.cols !== cols || t.term.rows !== rows) {
        invoke("resize_pty", { id, cols: t.term.cols, rows: t.term.rows });
      }
    }
  });
}
(function watchDpr() {
  // 쿼리는 현재 배율에만 맞으므로 바뀔 때마다 새로 건다. matchMedia가 없는 런타임
  // (기동 검사의 node 등)에서는 창 크기 변경(resize)만으로 둔다 — 여기서 던지면
  // 이 파일의 나머지가 멈추고, 뒤 파일들이 그 선언을 못 찾는다.
  if (typeof matchMedia !== "function") return;
  matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`).addEventListener(
    "change", () => { onDprMaybeChanged(); watchDpr(); }, { once: true });
})();
window.addEventListener("resize", onDprMaybeChanged);

