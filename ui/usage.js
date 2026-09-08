// 사용량 화면 — 요금 대시보드, 세션별 토큰 팝업, 사이드바 하단의 요금제 한도.
// main.js에서 갈라져 나온 파일이다. 모듈이 아니라 두 번째 <script>이므로 전역은
// 그대로 공유한다. main.js가 먼저 로드돼야 한다 — 이 파일의 최상위 코드가 $()와
// 그 안의 DOM 헬퍼를 바로 쓴다.

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
// 한 화면에 한 가지만 — 질문 목록이나 질문 하나의 상세 중 하나만 보인다.
// 예전에는 목록 아래로 흔적이 펼쳐져 화면이 계속 길어졌다.
// 선택은 목록 순서가 아니라 질문 번호로 기억한다 (정렬을 바꾸면 순서가 흔들린다).
let selectedPrompt = null;
let promptsByTime = false;
// 기본은 8줄만. 전체 표를 되살리면 다시 빽빽해지므로 필요할 때만 8줄씩 늘린다.
let promptLimit = 0;
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
  selectedPrompt = null;
  promptsByTime = false;
  promptLimit = PROMPT_ROWS;
  $("#turns-sub").textContent = "읽는 중…";
  $("#turns-tiles").innerHTML = "";
  $("#turns-chart").innerHTML = "";
  $("#turns-prompts").innerHTML = "";
  $("#turns-detail").innerHTML = "";
  $("#turns-detail").classList.add("hidden");
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
    tip.innerHTML = `<b>${turnTime(t.ts)}</b> · 이 턴 ${fmtVal(cost[i], true)} / 여기까지 ${fmtVal(acc[i])}`;
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
const PROMPT_ROWS = 8;
let promptTotal = 0;

function renderPrompts(cost) {
  // 상세를 보고 있으면 목록 대신 그것만 그린다
  const picked = selectedPrompt == null ? null : turnGroups.find((g) => g.idx === selectedPrompt);
  if (picked) {
    renderDetail(picked, cost);
    return;
  }
  selectedPrompt = null;
  highlightSpan(null);   // 다시 그리면 hover 해제가 오지 않아 띠가 남는다
  $("#turns-detail").classList.add("hidden");
  $("#turns-prompts").classList.remove("hidden");
  const by = new Map();
  turnsData.forEach((t, i) => {
    const g = by.get(t.prompt_idx) || { idx: t.prompt_idx, prompt: t.prompt, cost: 0, idxs: [], miss: 0, answer: "" };
    g.cost += cost[i];
    g.idxs.push(i);
    if (isCacheMiss(t, i)) g.miss += 1;
    if (t.text && t.text.trim()) g.answer = t.text; // 마지막 말이 그 질문의 답
    by.set(t.prompt_idx, g);
  });
  const all = [...by.values()].filter((g) => g.prompt);
  turnGroups = (promptsByTime
    ? [...all].sort((a, b) => a.idxs[0] - b.idxs[0])
    : [...all].sort((a, b) => b.cost - a.cost)
  ).slice(0, promptLimit || PROMPT_ROWS);
  promptTotal = all.length;
  $("#turns-prompts-head").innerHTML =
    `질문별 ${turnsPriced ? "비용" : "토큰"} — ${promptsByTime ? "시간순" : "많이 쓴 순"} ` +
    `${turnGroups.length}개 / 전체 ${promptTotal}개 ` +
    `<button id="turns-sort" class="btn-ghost">${promptsByTime ? "비용순으로" : "시간순으로"}</button>`;
  $("#turns-sort").onclick = () => {
    promptsByTime = !promptsByTime;
    renderPrompts(cost);
  };
  if (!turnGroups.length) {
    $("#turns-prompts").innerHTML = `<div class="dash-note">질문 기록이 없습니다</div>`;
    return;
  }
  const rows = turnGroups
    .map((g, gi) =>
      `<tr class="tp-row${g.miss ? " turn-miss" : ""}" data-g="${gi}">` +
      `<td class="tp-q" title="${escapeHtml(g.prompt)}">${escapeHtml(firstLine(mdPlain(g.prompt), 60))}</td>` +
      `<td class="tp-a" title="${escapeHtml(g.answer)}">${escapeHtml(firstLine(mdPlain(g.answer), 60) || "—")}</td>` +
      `<td>${g.idxs.length}</td><td>${fmtVal(g.cost)}</td></tr>`)
    .join("");
  const rest = promptTotal - turnGroups.length;
  // 표만 스크롤한다 — "더 보기"가 스크롤 안에 있으면 기본 상태에서도 밀려 잘린다.
  $("#turns-prompts").innerHTML =
    `<div class="tp-scroll"><table><thead><tr><th>질문</th><th>답변</th><th>턴</th>` +
    `<th>${turnsPriced ? "비용" : "토큰"}</th></tr></thead><tbody>${rows}</tbody></table></div>` +
    (rest > 0
      ? `<button id="turns-more" class="btn-ghost tp-more">더 보기 (남은 ${rest}개)</button>`
      : "");
  if (rest > 0) {
    $("#turns-more").onclick = () => {
      promptLimit = turnGroups.length + PROMPT_ROWS;
      renderPrompts(cost);
    };
  }
  $("#turns-prompts").querySelectorAll(".tp-row").forEach((tr) => {
    tr.onmouseenter = () => highlightSpan(turnGroups[tr.dataset.g]);
    tr.onmouseleave = () => highlightSpan(null);
    tr.onclick = () => {
      selectedPrompt = turnGroups[Number(tr.dataset.g)].idx;
      renderPrompts(cost);
    };
  });
}

// 질문 하나만 보는 화면. 질문과 답변은 자르지 않고, 그 아래 무슨 일을 했는지 잇는다.
function renderDetail(g, cost) {
  $("#turns-prompts").classList.add("hidden");
  const d = $("#turns-detail");
  d.classList.remove("hidden");
  $("#turns-prompts-head").innerHTML =
    `<button id="turns-back" class="btn-ghost">← 질문 목록</button>` +
    `<span class="td-sum">턴 ${g.idxs.length}개 · ${fmtVal(g.cost)}` +
    `${g.miss ? " · 캐시 끊김 " + g.miss + "회" : ""}</span>`;
  d.innerHTML =
    `<div class="td-q">${mdInline(g.prompt)}</div>` +
    `<div class="td-trail">${trailOf(g, cost)}</div>` +
    `<div class="td-a-head">답변</div>` +
    `<div class="td-a">${mdToHtml(g.answer || "(답변 없음)")}</div>`;
  $("#turns-back").onclick = () => {
    selectedPrompt = null;
    highlightSpan(null);
    renderPrompts(cost);
  };
  highlightSpan(g);   // 상세를 보는 동안 차트에 그 구간을 붙여 둔다
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
        `<span class="tr-cost">${fmtVal(cost[i], true)}</span>` +
        `<span class="tr-what">${what}${what && tools ? " " : ""}${tools}</span></div>`;
    })
    .filter(Boolean)
    .join("");
  return lines || `<div class="dash-note">기록된 내용이 없습니다</div>`;
}

$("#turns-close").onclick = () => closeModal($("#turns-backdrop"));

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
