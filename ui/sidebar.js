// [sidebar] 왼쪽 세션 목록. core.js 다음.
// 담당: 목록 그리기(renderSidebar·sessionRow·램프·프로젝트 묶음·대화 본문 검색), 빈 화면의 최근 세션,
//       우클릭 메뉴, 이름 바꾸기, 마크다운 미리보기 카드, 알림(showToast·notifyDone)·pty-state 수신.

// ---------- 사이드바 ----------
// 폴링이 겹치면 느린 디스크에서 스캔이 쌓여 더 느려진다(학습 중 실측: 한 번에 34초).
// 진행 중이면 그냥 건너뛴다.
let refreshing = null;

async function refreshSessions() {
  // 겹치면 새로 훑지 않고 진행 중인 것에 붙는다. 그냥 무시하면 await한 쪽이
  // "갱신됐겠지" 하고 낡은 목록으로 판단한다. 붙는 쪽은 그 갱신이 자기 호출보다
  // 먼저 읽은 데이터를 받을 수 있다 — 방금 만든 세션은 다음 폴링이나 클릭 시점에 잡힌다.
  if (refreshing) return refreshing;
  refreshing = refreshSessionsInner().finally(() => { refreshing = null; });
  return refreshing;
}

async function refreshSessionsInner() {
  // 창이 안 보이면 목록을 새로 읽지 않는다 — 전 프로젝트 폴더를 훑고 사이드바를
  // 통째로 다시 만드는 작업이라, 보이지도 않는 동안 20초마다 할 이유가 없다.
  // 다시 보이는 순간 아래 visibilitychange가 한 번 당겨 실행한다.
  if (document.hidden && restored) return;
  const t0 = performance.now();
  sessions = await invoke("list_sessions");
  adoptNewTabs();
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
    // 모르는 값은 잡 폴더의 state.json에서 온 날것이다 — 이 문자열은 사이드바 innerHTML에
    // 들어가므로 이스케이프한다(CSP가 없어 스크립트가 실행되면 탭을 띄울 수도 있다).
    const bg = BG_STATE[s.bg_state] || { cls: "run", label: escapeHtml(String(s.bg_state)) };
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
// 컨텍스트가 차서 이어 붙거나 포크로 갈라진 세션은 앞부분이 통째로 같아서 제목도 같다
// (실측: 한 폴더에 같은 줄 3개). 제목이 겹칠 때만 세션 id 앞 4자리를 덧붙인다 —
// 안 겹치는 줄에는 아무것도 붙지 않는다.
let dupTitles = new Set();

function markDuplicateTitles(list) {
  const seen = new Map();
  for (const s of list) {
    const k = sessionTitle(s) || "";
    if (!k) continue;
    seen.set(k, (seen.get(k) || 0) + 1);
  }
  dupTitles = new Set([...seen].filter(([, n]) => n > 1).map(([k]) => k));
}

// 세션 줄 셋째 칸은 클로드가 잡 상태에 적는 한 줄 요약이다. 마지막 입력이 캐시 유지
// 핑이면 클로드가 그 핑을 요약으로 적어서 "reply "." only"가 줄마다 보였다 — 숨긴다.
// 요약은 마크다운이라 **굵게** 같은 기호가 그대로 보였다 — 글자만 남긴다.
function bgDetailText(d) {
  const t = mdPlain(String(d || "")).trim();
  if (!t) return "";
  // keepAlive는 파일 아래쪽에서 선언된다 — 그보다 먼저 목록을 그리는 경로가 있어도 깨지지 않게
  let ping = "";
  try { ping = (keepAlive.message || "").trim(); } catch { /* 아직 선언 전 */ }
  if (ping && t === mdPlain(ping).trim()) return "";
  return t;
}

function sessionRow(s, child) {
  const el = document.createElement("div");
  el.className = "session-item" + (s.session_id === activeId ? " active" : "") + (child ? " child" : "");
  el.dataset.id = s.session_id;
  const [, t] = tabFor(s.session_id);
  const title = sessionTitle(s) || "(내용 없음)";
  // 포크된 세션인데 이름에 표식이 없으면 붙여 준다 (원본이 목록에 없어 들여쓰기가 안 될 때도 보이게)
  const fork = s.parent_id && !title.includes("⑂") ? `<span class="si-fork" title="포크된 세션">⑂</span>` : "";

  const st = sessionStatus(s, t);
  // 사람의 답을 기다리는 세션은 줄 전체로 드러낸다 (열린 탭이든 백그라운드 잡이든)
  if ((t && t.waiting && !t.exited) || (!t && s.bg_running && s.bg_state === "blocked")) el.classList.add("wait");
  const unread = t && t.attention && !t.exited;
  const slot = st.cls ? `<span class="si-status ${st.cls}${unread ? " unread" : ""}" title="${unread ? "응답 완료 — 아직 안 봄" : st.label}"></span>` : "";
  const pin = pins.includes(s.session_id) ? PIN_SVG : "";
  const glyph = `<span class="si-agent ${escapeHtml(s.agent)}" title="${escapeHtml(s.agent)}">${AGENT_GLYPH[s.agent] || "•"}</span>`;
  const badge = st.badge ? `<span class="si-bg ${st.badge.cls}" title="${st.label}">${st.badge.label}</span>` : "";
  const expEpoch = s.cache_last_ts && s.cache_ttl_secs ? s.cache_last_ts + s.cache_ttl_secs : null;
  const ttl = expEpoch ? `<span class="si-ttl" data-exp="${expEpoch}" title="프롬프트 캐시 남은 TTL"></span>` : "<span></span>";
  const ctxText = s.ctx_tokens
    ? `<span class="si-ctx" title="마지막 응답 시점 컨텍스트 토큰 수">${fmtTok(s.ctx_tokens)} ctx</span>`
    : "<span></span>";

  // 왼쪽 램프 칸 + 본문. 램프는 줄마다 같은 자리라 위에서 아래로 훑으면 누가 나를
  // 부르는지 보인다. 프로젝트는 묶음 머리말에 있으므로 줄에는 없다.
  // 메타 줄은 고정 칸(시각 | ctx | TTL)이라 행마다 자리가 같다.
  el.innerHTML = `
    <div class="si-slot">${slot}</div>
    <div class="si-body">
      <div class="si-title">${pin}${glyph}${fork}<span class="si-title-text"></span>${badge}</div>
      <div class="si-meta">
        <span class="si-time">${timeAgo(s.mtime)}</span>
        ${ctxText}
        ${ttl}
      </div>
      ${bgDetailText(s.bg_detail) ? `<div class="si-bg-detail"></div>` : ""}
    </div>`;
  el.querySelector(".si-title-text").textContent = title;
  if (dupTitles.has(sessionTitle(s))) {
    const tag = document.createElement("span");
    tag.className = "si-dup";
    tag.textContent = s.session_id.slice(0, 4);
    tag.title = "같은 이름이 여럿이라 세션 id를 함께 표시합니다";
    el.querySelector(".si-title").appendChild(tag);
  }
  const detail = bgDetailText(s.bg_detail);
  if (detail) el.querySelector(".si-bg-detail").textContent = detail;

  el.onclick = () => detach("openSession", openSession(s));
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
  if (parent) items.push(["원본 세션 열기", () => detach("openSession:parent", openSession(parent))]);
  // 복사본은 아무 claude 세션에서나 갈라져 나올 수 있다 (codex/gemini는 --fork-session이 없다)
  if (s.agent === "claude") items.push(["복사본으로 열기", () => detach("openSession:fork", openSession(s, true, { fork: true }))]);
  return items;
}

const STATUS_FILTERS = {
  "!": (s, t) => (t && t.busy && !t.waiting && !t.exited) || (!t && s.bg_running && s.bg_state === "working"),
  "@": (s, t) => (t && (t.attention || t.waiting)) || (!t && s.bg_running && s.bg_state === "blocked"),
  "#": (s, t) => !!t,
  "&": (s, t) => !t && s.bg_state === "failed",
};

const EMPTY_SESSION_GRACE_S = 600;
function isEmptySession(s, nowSec) {
  // 답이 하나라도 있으면(명령으로 시작해 제목이 없는 세션) 내용이 있는 것이다
  return !sessionTitle(s) && !s.last_text && !s.bg_running && !pins.includes(s.session_id)
    && nowSec - s.mtime > EMPTY_SESSION_GRACE_S && !tabFor(s.session_id)[0];
}

function renderSidebar() {
  // 접혀 있으면 그리지 않는다 — 20초 폴링마다 보이지도 않는 DOM을 통째로 다시
  // 만들 이유가 없다. 펼칠 때 setSidebarCollapsed가 한 번 다시 그린다.
  renderEmptyRecent(); // 빈 화면은 사이드바를 접어도 보인다
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

  const nowSec = Date.now() / 1000;
  const visible = sessions.filter((s) => {
    if (agentFilter !== "all" && s.agent !== agentFilter) return false;
    // 질문 한 번 없이 끝난 세션(띄웠다 닫음, 명령만 쳐 봄)은 "(내용 없음)"으로 쌓이기만
    // 한다. 열려 있거나 방금 생긴 것(아직 첫 질문 전), 고정·백그라운드는 남긴다.
    if (!q && !statusKey && isEmptySession(s, nowSec)) return false;
    if (statusKey && !STATUS_FILTERS[statusKey](s, tabFor(s.session_id)[1])) return false;
    if (!q) return true;
    // 제목·프로젝트에 더해 백그라운드 상태 문구도 검색 대상에 넣는다
    const hay = [
      sessionTitle(s),
      projOf(s),
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

  markDuplicateTitles(sorted);

  // 백그라운드 세션은 원본 아래로 모은다 (원본이 목록에 있을 때만)
  const shown = new Set(sorted.map((s) => s.session_id));
  const kids = new Map();
  for (const s of sorted) {
    if (s.parent_id && shown.has(s.parent_id)) {
      if (!kids.has(s.parent_id)) kids.set(s.parent_id, []);
      kids.get(s.parent_id).push(s);
    }
  }

  // 프로젝트로 묶는다. 고정한 세션은 맨 위에 따로 모으고, 프로젝트는 가장 최근에 쓴
  // 세션 순서대로 놓는다(sorted가 이미 고정 → 최근 순이라 처음 나온 순서가 곧 그 순서다).
  // 검색·필터 중에도 묶음은 그대로 두고, 남은 줄이 없는 프로젝트는 머리말도 빠진다.
  const groups = new Map();
  for (const s of sorted) {
    if (s.parent_id && shown.has(s.parent_id)) continue; // 부모 밑에서 그린다
    const key = pins.includes(s.session_id) ? "\u0000pin" : projOf(s) || "(폴더 없음)";
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(s);
  }
  let count = 0;
  for (const [key, list] of groups) {
    const head = document.createElement("div");
    head.className = "si-group";
    head.textContent = key === "\u0000pin" ? "고정" : key;
    listEl.appendChild(head);
    for (const s of list) {
      listEl.appendChild(sessionRow(s, false));
      count++;
      for (const k of kids.get(s.session_id) || []) {
        listEl.appendChild(sessionRow(k, true));
        count++;
      }
    }
  }

  // 검색어가 대화 본문에만 있는 세션은 따로 묶어 찾은 자리를 한 줄 보여 준다.
  // 본문 검색은 기록 파일을 훑으므로(1.2GB에 0.2초) 글자를 칠 때마다가 아니라 잠깐 멈추면 돈다.
  const cq = statusKey ? "" : q;
  scheduleContentSearch(cq);
  if (cq && contentHits.q === cq) {
    const extra = contentHits.hits
      .filter((h) => !shown.has(h.session_id))
      .map((h) => [sessions.find((s) => s.session_id === h.session_id), h])
      .filter(([s]) => s && (agentFilter === "all" || s.agent === agentFilter))
      .sort(([a], [b]) => b.mtime - a.mtime); // 사이드바의 다른 줄처럼 최근 것부터
    if (extra.length) {
      const head = document.createElement("div");
      head.className = "si-group";
      head.textContent = "대화 내용에서";
      listEl.appendChild(head);
      for (const [s, h] of extra) {
        const row = sessionRow(s, false);
        const snip = document.createElement("div");
        snip.className = "si-snippet";
        snip.textContent = `${h.role === "user" ? "나" : "답"}  ${h.snippet}`;
        row.querySelector(".si-body").appendChild(snip);
        listEl.appendChild(row);
        count++;
      }
    }
  }

  if (performance.now() - sbT0 > 5) uiTrace("sidebar", performance.now() - sbT0);
  const busyN = [...terms.values()].filter((t) => !t.exited && t.busy).length;
  const runN = [...terms.values()].filter((t) => !t.exited).length;
  const bgN = sessions.filter((s) => s.bg_running).length;
  const prof = currentProfile();
  // 가운데점으로 잇지 않고 칸으로 나눈다 — 숫자가 앞에 와서 훑을 때 숫자만 읽힌다
  const parts = [`세션 ${count}개`, `실행 ${runN}`, `답변 중 ${busyN}`];
  if (bgN) parts.push(`백그라운드 ${bgN}`);
  if (prof.cmd !== "claude") parts.push(prof.name);
  const foot = $("#foot-count");
  foot.innerHTML = "";
  for (const p of parts) {
    const span = document.createElement("span");
    span.textContent = p;
    foot.appendChild(span);
  }
  kbRestore();
  updateTtlBadges();
}

// 대화 본문 검색. 같은 검색어로는 한 번만 돌고(목록은 20초마다 다시 그려진다), 늦게 온
// 답은 버린다(그 사이 검색어가 바뀌었다).
let contentHits = { q: "", hits: [] };
let contentTimer = 0;
let contentToken = 0;
const CONTENT_SEARCH_DELAY_MS = 350;
function scheduleContentSearch(q) {
  if (q === contentHits.q) return;
  clearTimeout(contentTimer);
  const token = ++contentToken;
  contentHits = { q, hits: [] };
  if ([...q].length < 2) return;
  contentTimer = setTimeout(async () => {
    const files = sessions.filter((s) => s.file).map((s) => [s.session_id, s.file]);
    const hits = await invoke("search_sessions", { query: q, files }).catch(() => []);
    if (token !== contentToken) return;
    contentHits = { q, hits: hits || [] };
    if (contentHits.hits.length) renderSidebar();
  }, CONTENT_SEARCH_DELAY_MS);
}

// 빈 화면의 "이어서 할 세션": 최근 세션 몇 개를 바로 열 수 있게. 빈 화면이 보일 때만 그린다.
const EMPTY_RECENT_N = 5;
function renderEmptyRecent() {
  const box = $("#empty-recent");
  if (!box || emptyState.classList.contains("hidden")) return;
  const nowSec = Date.now() / 1000;
  const list = sessions
    .filter((s) => !isEmptySession(s, nowSec) && !tabFor(s.session_id)[0])
    .sort((a, b) => b.mtime - a.mtime)
    .slice(0, EMPTY_RECENT_N);
  box.innerHTML = "";
  for (const s of list) {
    const b = document.createElement("button");
    b.className = "er-row";
    b.innerHTML = `<span class="si-agent ${escapeHtml(s.agent)}">${AGENT_GLYPH[s.agent] || "•"}</span>` +
      `<span class="er-title"></span><span class="er-proj"></span><span class="er-time">${timeAgo(s.mtime)}</span>`;
    b.querySelector(".er-title").textContent = (sessionTitle(s) || s.session_id.slice(0, 8)).slice(0, 80);
    b.querySelector(".er-proj").textContent = projOf(s);
    b.onclick = () => detach("openSession:empty", openSession(s));
    box.appendChild(b);
  }
  emptyState.classList.toggle("no-recent", !list.length);
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
  // 메뉴는 글자만 — 이모지 아이콘은 글꼴마다 색과 크기가 달라 줄이 들쭉날쭉했다.
  // 되돌릴 수 없는 삭제는 선으로 떼어 맨 아래에 붉게 둔다.
  const items = [
    ...bgMenuItems(s),
    ["토큰 상세", () => openTurns(s)],
    [pinned ? "핀 해제" : "핀 고정", () => {
      pins = pinned ? pins.filter((x) => x !== s.session_id) : [...pins, s.session_id];
      localStorage.setItem("pins", JSON.stringify(pins));
      renderSidebar();
    }],
    ["이름 바꾸기", () => startRename(s, itemEl)],
    ["폴더 열기", () => {
      invoke("open_path", { path: s.cwd }).catch((e) => showToast("⚠ 폴더 열기 실패", String(e)));
    }],
    ["로그 파일 열기", () => {
      invoke("open_log_file", { file: s.file }).catch((e) => showToast("⚠ 로그 열기 실패", String(e)));
    }],
    ["세션 ID 복사", () => {
      const cm = window.__TAURI__ && window.__TAURI__.clipboardManager;
      Promise.resolve(cm ? cm.writeText(s.session_id) : navigator.clipboard.writeText(s.session_id))
        .then(() => showToast("세션 ID를 복사했습니다", s.session_id))
        .catch((e) => showToast("⚠ 복사 실패", String(e)));
    }],
    ["세션 삭제", async () => {
      try {
        const ok = await window.__TAURI__.dialog.confirm(
          `이 세션 기록을 영구 삭제할까요?\n\n${sessionTitle(s) || s.session_id}`,
          { title: "세션 삭제", kind: "warning" });
        if (!ok) return;
        const [openTab] = tabFor(s.session_id);
        if (openTab) await closeTab(openTab);
        await invoke("delete_session", { file: s.file });
        refreshSessions();
      } catch { /* 무시 */ }
    }],
  ];
  for (const [label, fn] of items) {
    if (label === "세션 삭제") {
      const sep = document.createElement("div");
      sep.className = "ctx-sep";
      ctxMenu.appendChild(sep);
    }
    const d = document.createElement("div");
    d.className = "ctx-item" + (label === "세션 삭제" ? " danger" : "");
    d.textContent = label;
    d.onclick = () => { hideCtxMenu(); fn(); };
    ctxMenu.appendChild(d);
  }
  ctxMenu.classList.remove("hidden");
  // 실제 크기를 재서 창 밖으로 나가지 않게 (항목 수가 세션마다 다르다)
  const { offsetWidth: mw, offsetHeight: mh } = ctxMenu;
  ctxMenu.style.left = Math.max(8, Math.min(e.clientX, window.innerWidth - mw - 8)) + "px";
  ctxMenu.style.top = Math.max(8, Math.min(e.clientY, window.innerHeight - mh - 8)) + "px";
}
function hideCtxMenu() { ctxMenu.classList.add("hidden"); }
window.addEventListener("click", hideCtxMenu);
window.addEventListener("blur", hideCtxMenu);

function startRename(s, itemEl) {
  const span = itemEl.querySelector(".si-title-text");
  const input = document.createElement("input");
  input.className = "si-rename";
  // 보이는 이름 그대로로 시작한다 — 클로드가 붙인 이름(s.title)을 건너뛰면 칸이 비어 보이고,
  // 뭔가 치고 나가면 그 이름을 덮어썼다.
  const original = sessionTitle(s);
  input.value = original;
  span.replaceWith(input);
  isRenaming = true; // 재렌더가 이 input을 지워버리지 않게 (20초 폴링이 있어 확정적으로 발생)
  input.focus();
  input.select();
  const finish = (save) => {
    if (!isRenaming) return; // commit과 blur가 겹쳐 두 번 들어오는 경우
    isRenaming = false;
    if (save) {
      const v = input.value.trim();
      if (!v) delete aliases[s.session_id];
      else if (v !== original) {
        aliases[s.session_id] = v; // 그대로 두고 나가면 별칭을 만들지 않는다
        // 클로드 세션은 세션 자체의 이름도 바꾼다(/rename과 같은 기록) — 클로드의 /resume
        // 목록에도 같은 이름이 보인다. 그러면 앱 별칭은 필요 없다. 못 쓰면 별칭으로 남긴다.
        if ((s.agent || "claude") === "claude" && s.file) {
          invoke("rename_session", { file: s.file, title: v })
            .then(() => {
              s.title = v;
              delete aliases[s.session_id];
              localStorage.setItem("aliases", JSON.stringify(aliases));
              renderSidebar();
            })
            .catch((e) => showToast("⚠ 세션 이름은 바꾸지 못했습니다", `이 앱에서만 바뀐 이름으로 보입니다 — ${e}`));
        }
      }
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
    previewCard.querySelector(".pv-title").textContent = sessionTitle(s);
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
// kind: "wait"(사람이 답해야 함 — 호박색, 저절로 사라지지 않음) · "done"(완료 — 초록) · 없음
function showToast(title, body, onClick, kind) {
  // 제목 앞의 이모지(✋ ✻ ⚠)는 떼고, 종류는 사이드바 램프와 같은 색 점으로 보인다
  // (호박 = 입력 필요, 초록 = 끝남, 빨강 = 실패). 이모지는 글꼴마다 모양·색이 제각각이었다.
  const raw = String(title);
  if (!kind && raw.startsWith("⚠")) kind = "warn";
  const el = document.createElement("div");
  el.className = "toast" + (kind ? " " + kind : "");
  el.innerHTML = `<div class="toast-title"></div><div class="toast-body"></div>`;
  el.querySelector(".toast-title").textContent = raw.replace(/^[^\p{L}\p{N}]+/u, "") || raw;
  el.querySelector(".toast-body").textContent = body;
  el.onclick = () => { el.remove(); if (onClick) onClick(); };
  $("#toasts").appendChild(el);
  if (kind === "wait") return; // 답을 기다리는 알림은 볼 때까지 남긴다
  setTimeout(() => {
    el.classList.add("fade");
    setTimeout(() => el.remove(), 400);
  }, 6000);
}

function notifyDone(id, t) {
  // 화면에 떠 있고 창에 포커스가 있으면 사용자가 이미 보고 있음 — 알림 불필요.
  // 입력 중이 아닌 칸이면 그 칸 테두리만 잠깐 깜빡여 끝났다는 걸 알린다.
  if (isShown(id) && document.hasFocus()) {
    if (id !== activeId) {
      t.container.classList.remove("pane-flash");
      void t.container.offsetWidth; // 연달아 와도 다시 깜빡이게
      t.container.classList.add("pane-flash");
    }
    return;
  }
  t.attention = true;
  showToast("✻ 응답 완료", t.title, () => activate(id), "done");
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
  if (working) noteTurnStart(id);
  if (!working && !t.exited && notify !== false) notifyDone(id, t);
  // 권한 확인·질문으로 멈춘 세션: 보고 있지 않으면 완료 알림과 같은 방식으로 알린다
  if (t.waiting && !wasWaiting && !(isShown(id) && document.hasFocus())) {
    t.attention = true;
    showToast("✋ 입력 필요", t.title, () => activate(id), "wait");
  }
  renderTabs();
  renderSidebar();
});

