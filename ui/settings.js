// [settings] 설정 창과 새 세션 창. panes.js 다음.
// 담당: 실행 프로필·환경변수·캐시 유지·상태줄 연동·GPU·Ctrl+클릭 열기 규칙·업데이트 채널 설정,
//       에이전트 버전·변경 내역·번역, 단추 연결, 새 세션 창(폴더 고르기·최근 폴더).

// ---------- 실행 프로필 (설정 창에서 관리) ----------
const DEFAULT_PROFILES = [
  { name: "Claude", cmd: "claude", resume: true },
  // codex/gemini는 각자 CLI의 재개 방식이 달라 --resume을 붙이지 않는다.
  { name: "Codex", cmd: "codex", resume: false },
  { name: "Gemini", cmd: "gemini", resume: false },
];

function loadProfiles() {
  try {
    const v = JSON.parse(localStorage.getItem("profiles"));
    if (Array.isArray(v) && v.length) return dropHeadroom(seedAgents(v));
  } catch { /* 무시 */ }
  // 새 설치는 기본 프로필에 이미 들어 있으니 채운 것으로 친다. 표시를 남기지 않으면
  // 나중에 지운 프로필이 다음 실행에서 한 번 되살아난다.
  localStorage.setItem("agentProfilesSeeded", "1");
  localStorage.setItem("headroomProfileRemoved", "1");
  return DEFAULT_PROFILES.map((x) => ({ ...x }));
}

// 헤드룸 연동을 걷어내면서 기본 프로필에서도 뺐다. 예전에 기본값으로 깔려 저장된
// 프로필도 한 번 지운다 — 직접 만든 래퍼는 명령이 다르므로 그대로 둔다.
function dropHeadroom(list) {
  if (localStorage.getItem("headroomProfileRemoved")) return list;
  localStorage.setItem("headroomProfileRemoved", "1");
  const keep = list.filter((p) => (p.cmd || "").trim() !== "headroom wrap claude");
  if (keep.length === list.length || !keep.length) return list;
  // 고른 프로필이 밀리지 않게 위치를 다시 잡는다
  const sel = parseInt(localStorage.getItem("profileSel") || "0", 10);
  const idx = keep.indexOf(list[sel]);
  localStorage.setItem("profileSel", String(idx >= 0 ? idx : 0));
  localStorage.setItem("profiles", JSON.stringify(keep));
  return keep;
}

// 코덱스·제미나이 프로필을 늦게 추가했다. 이미 프로필을 저장해 둔 설치본에도 한 번은
// 넣어 준다 — 없으면 새 세션을 그 에이전트로 시작할 방법이 아예 없다. 지운 사람에게
// 다시 강요하지 않도록 딱 한 번만 한다.
function seedAgents(list) {
  if (localStorage.getItem("agentProfilesSeeded")) return list;
  localStorage.setItem("agentProfilesSeeded", "1");
  const add = DEFAULT_PROFILES.filter(
    (d) => (d.name === "Codex" || d.name === "Gemini") &&
      !list.some((p) => (p.cmd || "").trim() === d.cmd));
  if (!add.length) return list;
  const merged = [...list, ...add.map((x) => ({ ...x }))];
  localStorage.setItem("profiles", JSON.stringify(merged));
  return merged;
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
// GPU를 오래 붙잡는 작업(학습 등)과 같은 기기에서 돌 때는 CPU 렌더러가 덜 끊긴다.
// 기본은 켬 — 평소에는 GPU 쪽이 확실히 빠르다.
let webglOn = localStorage.getItem("webgl") !== "0";
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
// force=true면 프로필의 "재개" 설정과 무관하게 --resume을 붙인다. 그 설정은 사이드바에서
// 세션을 여는 방식에 대한 것이고, 이미 그 세션이 된 탭을 다시 띄우는 것과는 다르다.
function composeCommand(resumeId, prof, force) {
  const p = prof || currentProfile();
  let cmd = resumeId && (force || p.resume !== false) ? `${p.cmd} --resume ${safeId(resumeId)}` : p.cmd;
  // --settings는 claude만 안다. codex는 모르는 인자라 아예 뜨지 않는다.
  if (statusLineOn && statusLinePath && agentOf(p) === "claude") cmd += ` --settings "${statusLinePath}"`;
  return envPrefix(globalEnv) + cmd;
}

// 설정 모달
function addProfileRow(name = "", cmd = "", resume = true, checked = false) {
  const row = document.createElement("div");
  row.className = "lrow";
  row.innerHTML = `
    <label class="l-active" title="이 프로필 사용"><input type="radio" name="active-profile" /></label>
    <input class="set-input l-name" placeholder="이름" spellcheck="false" />
    <input class="set-input mono l-cmd" placeholder="실행 명령 (예: my-proxy run claude)" spellcheck="false" />
    <label class="l-resume" title="세션 재개 시 --resume <세션ID> 인자를 붙일지"><input type="checkbox" />재개</label>
    <button class="l-del" title="삭제">✕</button>`;
  row.querySelector(".l-active input").checked = checked;
  row.querySelector(".l-name").value = name;
  row.querySelector(".l-cmd").value = cmd;
  row.querySelector(".l-resume input").checked = resume;
  row.querySelector(".l-del").onclick = () => row.remove();
  $("#profile-list").appendChild(row);
}

// 자동 확인은 6시간마다다. 그 사이에 새 버전이 나왔는지 지금 보고 싶을 때 쓴다.
$("#btn-check-update").onclick = async (e) => {
  const btn = e.currentTarget;
  const el = $("#update-state");
  btn.disabled = true;            // 연타로 두드리지 않게
  el.textContent = "확인 중…";
  try {
    // 저장 전이라도 지금 켜 둔 채널로 본다 — 스위치를 켜고 바로 누르는 경우가 흔하다
    const found = await checkUpdate($("#opt-beta").checked, true);
    el.textContent = found ? `v${found} 있음 — 창 위 버튼으로 설치` : "최신 버전입니다";
  } catch (err) {
    // 오프라인이나 GitHub 오류를 "최신"으로 보이면 안 된다
    el.textContent = "확인 실패 — 잠시 후 다시";
    el.title = String(err);
  } finally {
    btn.disabled = false;
  }
};

// 설정의 "Ctrl+클릭으로 파일 열기" 줄. 깔려 있는 편집기만 고를 수 있게 한다.
const OPEN_HOW_LABEL = {
  auto: "자동 (VS Code 우선)", default: "기본 프로그램", vscode: "VS Code",
  rider: "Rider", vs: "Visual Studio", reveal: "탐색기에서 위치만",
};
let editorsAvail = null;
async function renderOpenRules() {
  const box = $("#open-rules");
  if (!editorsAvail) editorsAvail = (await invoke("list_editors").catch(() => null)) || {};
  box.innerHTML = "";
  openRules.forEach((r, i) => {
    const row = document.createElement("div");
    row.className = "open-rule";
    row.innerHTML = `<span class="or-name"></span><input class="set-input mono or-exts" spellcheck="false" /><select class="set-input or-how"></select>`;
    row.querySelector(".or-name").textContent = r.name;
    const ext = row.querySelector(".or-exts");
    ext.value = r.exts === "*" ? "나머지 전부" : r.exts;
    ext.disabled = r.exts === "*";
    ext.title = "확장자를 띄어 쓰거나 쉼표로 나눠 적습니다";
    const sel = row.querySelector(".or-how");
    for (const [k, label] of Object.entries(OPEN_HOW_LABEL)) {
      if ((k === "vscode" || k === "rider" || k === "vs") && !editorsAvail[k] && r.how !== k) continue;
      const o = document.createElement("option");
      o.value = k;
      o.textContent = label;
      sel.appendChild(o);
    }
    sel.value = r.how;
    row.dataset.i = i;
    box.appendChild(row);
  });
}
function saveOpenRules() {
  const rows = [...document.querySelectorAll("#open-rules .open-rule")];
  if (rows.length !== openRules.length) return;
  rows.forEach((row, i) => {
    if (openRules[i].exts !== "*") openRules[i].exts = row.querySelector(".or-exts").value.trim();
    openRules[i].how = row.querySelector(".or-how").value;
  });
  localStorage.setItem("openRules", JSON.stringify(openRules.map(({ exts, how }) => ({ exts, how }))));
}

$("#btn-settings").onclick = () => {
  $("#profile-list").innerHTML = "";
  profiles.forEach((p, i) => addProfileRow(p.name, p.cmd, p.resume !== false, i === activeProfile));
  $("#global-env").value = globalEnv;
  invoke("trace_enabled").then((on) => { $("#opt-trace").checked = !!on; }).catch(() => {});
  $("#update-state").textContent = "";
  loadAgentVersions();
  $("#opt-webgl").checked = webglOn;
  renderOpenRules();
  $("#opt-beta").checked = betaUpdates;
  $("#opt-statusline").checked = statusLineOn;
  $("#opt-restore-ask").checked = localStorage.getItem("restoreAsk") !== "0";
  $("#opt-keepalive").checked = !!keepAlive.enabled;
  $("#ka-threshold").value = String(keepAlive.thresholdSecs / 60);
  $("#ka-message").value = keepAlive.message;
  syncKaFields();
  $("#lmodal-backdrop").classList.remove("hidden");
};
// 새 판에 뭐가 바뀌었는지 — 설치판 다음부터 최신판까지를 버전별로 접어 보인다(최신만
// 펼침). 한 판이 수만 자인 경우도 있어 칸 안에서 스크롤한다. 누를 때만 받아 온다.
// 변경 내역 한 판을 한국어로 — 클로드(Haiku)가 번역하고, 한 번 한 것은 저장해 둔다(Rust).
// 펼쳐 본 판만 번역한다: 긴 판은 수만 자라 1분 가까이 걸린다. "원문"으로 되돌린다.
function translateButton(agentName, c, body) {
  const b = document.createElement("button");
  b.className = "btn-ghost btn-sm agent-translate";
  b.textContent = "번역";
  let original = null;
  let translated = null;
  b.onclick = async () => {
    if (original !== null) { // 지금 번역을 보고 있다 → 원문으로
      body.innerHTML = original;
      original = null;
      b.textContent = "번역";
      return;
    }
    if (translated === null) {
      b.disabled = true;
      b.textContent = (c.notes || "").length > 8000 ? "번역 중… (길어서 1분쯤)" : "번역 중…";
      try {
        translated = await invoke("translate_changelog", { name: agentName, version: c.version, text: c.notes || "" });
      } catch (e) {
        b.disabled = false;
        b.textContent = "번역";
        showToast("⚠ 번역하지 못했습니다", String(e));
        return;
      }
      b.disabled = false;
    }
    original = body.innerHTML;
    body.innerHTML = mdToHtml(translated); // 번역도 마크다운 — 먼저 이스케이프한다
    b.textContent = "원문";
  };
  if (!(c.notes || "").trim()) b.hidden = true;
  return b;
}

// recent=true면 최신판을 쓰고 있을 때 — 새로 받을 건 없지만 지금 판까지 무엇이
// 바뀌었는지는 볼 수 있어야 한다. 설치판까지의 최근 판들(백엔드가 15개, 여기서 5개)을 보인다.
function changelogButton(a, box, row, recent = false) {
  const label = recent ? "최근 변경" : "변경 내용";
  const b = document.createElement("button");
  b.className = "btn-ghost btn-sm";
  b.textContent = label;
  b.onclick = async () => {
    const open = row.nextElementSibling && row.nextElementSibling.classList.contains("agent-changes");
    if (open) { row.nextElementSibling.remove(); b.textContent = label; return; }
    const panel = document.createElement("div");
    panel.className = "agent-changes";
    panel.textContent = "받는 중…";
    row.after(panel);
    b.textContent = "접기";
    let list;
    try {
      list = await invoke("agent_changelog", recent
        ? { name: a.name, from: "", to: a.installed }
        : { name: a.name, from: a.installed, to: a.latest });
      if (recent && Array.isArray(list)) list = list.slice(0, 5);
    } catch (e) {
      panel.textContent = "받지 못했습니다: " + e;
      return;
    }
    if (!Array.isArray(list) || !list.length) { panel.textContent = "공개된 변경 내용이 없습니다"; return; }
    panel.textContent = "";
    list.forEach((c, i) => {
      const d = document.createElement("details");
      if (i === 0) d.open = true;
      const sum = document.createElement("summary");
      sum.textContent = "v" + c.version;
      const body = document.createElement("div");
      body.className = "agent-changes-body";
      body.innerHTML = mdToHtml(c.notes || "(설명 없음)"); // mdToHtml이 먼저 이스케이프한다
      d.append(sum, translateButton(a.name, c, body), body);
      panel.appendChild(d);
    });
  };
  return b;
}

// 설정을 열 때마다 새로 본다. 에이전트마다 --version 한 번과 npm 레지스트리 한 번이라
// 1~2초 걸리므로 창은 먼저 열고 나중에 채운다.
async function loadAgentVersions() {
  const box = $("#agent-versions");
  box.innerHTML = '<span class="lmodal-hint">확인 중…</span>';
  let list = [];
  try { list = await invoke("agent_versions"); } catch (e) {
    box.innerHTML = "";
    box.textContent = "확인하지 못했습니다: " + e;
    return;
  }
  if (!Array.isArray(list)) list = []; // 응답 모양이 다르면 빈 카드로 멈추지 않게
  box.innerHTML = "";
  for (const a of list) {
    const row = document.createElement("div");
    row.className = "agent-row";
    row.innerHTML = `<span class="agent-name"></span><span class="agent-ver mono"></span><span class="agent-state"></span>`;
    row.querySelector(".agent-name").textContent = a.name;
    const ver = row.querySelector(".agent-ver");
    const state = row.querySelector(".agent-state");
    if (!a.installed) {
      ver.textContent = "—";
      state.textContent = "설치 안 됨";
      state.className = "agent-state dim";
    } else {
      ver.textContent = a.installed;
      if (!a.latest) {
        state.textContent = "최신판 확인 실패";
        state.className = "agent-state dim";
      } else if (a.update_available) {
        state.textContent = `${a.latest} 있음` + (a.channel !== "latest" ? ` (${a.channel})` : "");
        state.className = "agent-state new";
        row.appendChild(changelogButton(a, box, row));
        if (a.can_update) {
          const b = document.createElement("button");
          b.className = "btn-ghost btn-sm";
          b.textContent = "업데이트";
          b.onclick = async () => {
            b.disabled = true;
            b.textContent = "받는 중…";
            try {
              const out = await invoke("update_agent", { name: a.name });
              showToast(`${a.name} 업데이트`, out || "완료 — 새로 여는 세션부터 새 판을 씁니다", null, "done");
            } catch (e) {
              showToast(`⚠ ${a.name} 업데이트 실패`, String(e));
            }
            loadAgentVersions();
          };
          row.appendChild(b);
        } else {
          // npm 전역 설치가 아니면(설치 프로그램·winget 등) 앱이 올리지 않는다 — npm으로
          // 올리면 두 벌이 생길 수 있다. 흔한 명령을 복사해 준다.
          const b = document.createElement("button");
          b.className = "btn-ghost btn-sm";
          b.textContent = "명령 복사";
          b.title = a.update_cmd;
          b.onclick = () => {
            const cm = window.__TAURI__ && window.__TAURI__.clipboardManager;
            (cm ? cm.writeText(a.update_cmd) : navigator.clipboard.writeText(a.update_cmd))
              .then(() => showToast("명령을 복사했습니다", a.update_cmd))
              .catch(() => showToast("⚠ 복사하지 못했습니다", a.update_cmd));
          };
          row.appendChild(b);
        }
      } else {
        state.textContent = "최신";
        state.className = "agent-state ok";
        row.appendChild(changelogButton(a, box, row, true));
      }
    }
    box.appendChild(row);
  }
}

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
  const webglWas = webglOn;
  webglOn = $("#opt-webgl").checked;
  localStorage.setItem("webgl", webglOn ? "1" : "0");
  if (webglOn !== webglWas) applyRenderer();
  saveOpenRules();
  const betaWas = betaUpdates;
  betaUpdates = $("#opt-beta").checked;
  localStorage.setItem("betaUpdates", betaUpdates ? "1" : "0");
  if (betaUpdates !== betaWas) checkUpdate(); // 새 채널로 바로 본다 (앞 채널 제안은 거기서 거둔다)
  localStorage.setItem("restoreAsk", $("#opt-restore-ask").checked ? "1" : "0");
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
// 어떤 에이전트로 시작할지는 세션마다 다르다. 설정을 열어 기본 프로필을 바꾸게 하는 대신
// 모달에서 고르게 한다 — 기본값은 설정에서 고른 프로필.
let newProfileIdx = 0;

function renderNewProfiles() {
  const wrap = $("#new-profiles");
  wrap.innerHTML = "";
  profiles.forEach((p, i) => {
    const chip = document.createElement("button");
    chip.className = "dir-chip" + (i === newProfileIdx ? " on" : "");
    chip.textContent = p.name || p.cmd;
    chip.title = p.cmd;
    chip.onclick = () => { newProfileIdx = i; renderNewProfiles(); };
    wrap.appendChild(chip);
  });
}

function openNewModal() {
  newProfileIdx = activeProfile;
  renderNewProfiles();
  const wrap = $("#recent-dirs");
  wrap.innerHTML = "";
  for (const d of getRecentDirs()) {
    const chip = document.createElement("button");
    chip.className = "dir-chip";
    chip.textContent = basename(d);
    chip.title = d;
    chip.onclick = () => { $("#modal-path").value = d; hideMissing(); };
    chip.ondblclick = () => { $("#modal-path").value = d; detach("startNew:recent", startNew(d)); };
    wrap.appendChild(chip);
  }
  hideMissing();
  $("#modal-backdrop").classList.remove("hidden");
  $("#modal-path").focus();
}

// 없는 폴더로 시작하면 예전에는 백엔드가 말없이 홈에서 띄웠다. 이제는 여기서 먼저
// 보고, 없으면 만들지 묻는다 — 오타 난 경로에 폴더가 생기면 안 되니 저절로 만들지 않는다.
function hideMissing() { $("#modal-missing").classList.add("hidden"); }
async function startNew(raw) {
  // 탐색기의 "경로로 복사"는 따옴표를 붙인다
  const p = raw.trim().replace(/^"(.*)"$/, "$1").trim();
  if (!p) return;
  let ok = false;
  try { ok = await invoke("dir_exists", { path: p }); } catch { /* 모르면 띄워 보고 오류를 보인다 */ ok = true; }
  // 네트워크 경로면 확인이 몇 초 걸린다. 그사이 경로를 고쳤으면 옛 경로로 안내하거나
  // "만들고 시작"을 옛 경로에 걸면 안 된다 — 오타가 폴더가 된다.
  if ($("#modal-path").value !== raw) return;
  if (!ok) {
    $("#modal-missing-text").textContent = `폴더가 없습니다: ${p}`;
    $("#modal-create").onclick = async () => {
      try {
        await invoke("create_dir", { path: p });
      } catch (err) {
        $("#modal-missing-text").textContent = String(err);
        return;
      }
      launchNew(p);
    };
    $("#modal-missing").classList.remove("hidden");
    return;
  }
  launchNew(p);
}
function launchNew(p) {
  hideMissing();
  $("#modal-backdrop").classList.add("hidden");
  detach("openNewSession", openNewSession(p, profiles[newProfileIdx]));
}
$("#modal-path").addEventListener("input", hideMissing);
$("#btn-new").onclick = openNewModal;
$("#empty-new").onclick = openNewModal;
$("#modal-browse").onclick = async () => {
  try {
    const d = await window.__TAURI__.dialog.open({ directory: true, defaultPath: $("#modal-path").value });
    if (d) $("#modal-path").value = d;
  } catch { /* 무시 */ }
};
$("#modal-cancel").onclick = () => closeModal($("#modal-backdrop"));
$("#modal-ok").onclick = () => detach("startNew", startNew($("#modal-path").value));
$("#modal-path").addEventListener("keydown", (e) => {
  if (e.key === "Enter") $("#modal-ok").click();
  if (e.key === "Escape") $("#modal-cancel").click();
});

