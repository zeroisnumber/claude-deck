// [app] 마지막에 붙는 것들. settings.js 다음, usage.js 앞.
// 담당: 단축키(Ctrl+Tab·Alt+1~4 등)·사이드바 키보드 탐색, 자동 업데이트 확인, 첫 목록 불러오기.

// ---------- 단축키 ----------
// Ctrl+Tab 탭 순환, Ctrl+1~9 탭 이동, Ctrl+Shift+W 탭 닫기, Ctrl+Shift+N 새 세션
function handleShortcut(e) {
  // Alt+1~4: 나눈 화면의 그 칸으로 입력을 옮긴다
  if (e.altKey && !e.ctrlKey && !e.shiftKey && /^Digit[1-4]$/.test(e.code) && paneCount() > 1) {
    const i = +e.code.slice(5) - 1;
    if (i >= paneCount()) return true;
    focusPane = i;
    if (panes[i]) activate(panes[i]);
    else markPaneFocus();
    return true;
  }
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
    if (activeId) detach("closeTab:shortcut", closeTab(activeId));
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
  // Ctrl+Shift+M: 입력이 이상할 때 그 순간을 진단 기록에 찍는다
  if (e.shiftKey && e.code === "KeyM") {
    const mark = activeId && entry_markers.get(activeId);
    if (!traceOn) showToast("진단 기록이 꺼져 있습니다", "설정에서 켜야 남습니다");
    else if (!mark) showToast("열린 탭이 없습니다", "탭에서 이상할 때 누르세요");
    else if (mark) { mark(); showToast("지금 화면을 기록했습니다", "언제 찍었는지 알려 주세요"); }
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
// 고른 행은 번호가 아니라 세션 id로 기억한다. 목록은 20초마다·상태가 바뀔 때마다 새로
// 그려지고 최근 순으로 다시 정렬된다 — 번호로 기억하면 그 사이 표시가 사라지거나,
// Enter가 고른 것과 다른 세션을 연다. (kbId는 맨 위 listEl 옆에 둔다 — renderSidebar가
// 이 줄보다 먼저 불릴 수 있다)
function kbRows() { return [...listEl.querySelectorAll(".session-item")]; }
function kbCurrent(rows) { return rows.findIndex((r) => r.dataset.id === kbId); }
function kbHighlight(i) {
  const rows = kbRows();
  rows.forEach((r) => r.classList.remove("kb"));
  if (!rows.length) { kbId = null; return; }
  const k = Math.max(0, Math.min(rows.length - 1, i));
  kbId = rows[k].dataset.id;
  rows[k].classList.add("kb");
  rows[k].scrollIntoView({ block: "nearest" });
}
// 새로 그린 목록에 고른 표시를 다시 얹는다 (renderSidebar 끝에서 부른다)
function kbRestore() {
  if (!kbId) return;
  const row = listEl.querySelector(`.session-item[data-id="${CSS.escape(kbId)}"]`);
  if (row) row.classList.add("kb");
  else kbId = null;
}
$("#search").addEventListener("keydown", (e) => {
  if (e.isComposing) return;
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    const rows = kbRows();
    const cur = kbCurrent(rows);
    kbHighlight(cur < 0 ? (e.key === "ArrowDown" ? 0 : rows.length - 1) : cur + (e.key === "ArrowDown" ? 1 : -1));
  } else if (e.key === "Enter") {
    e.preventDefault();
    const rows = kbRows();
    const row = rows[kbCurrent(rows)] || rows[0];
    if (!row) return;
    const s = sessions.find((x) => x.session_id === row.dataset.id);
    if (s) detach("openSession:search", openSession(s));
  } else if (e.key === "Escape") {
    e.preventDefault();
    $("#search").value = "";
    kbId = null;
    renderSidebar();
    const t = terms.get(activeId);
    if (t) t.term.focus();
  }
});
$("#search").addEventListener("input", () => { kbId = null; });
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
// 확인 경로는 둘뿐이다: 시작 직후와 6시간 주기, 그리고 설정의 "업데이트 확인".
// 세션 갱신(20초)에 얹지 않는다 — 사무실처럼 여러 대가 같은 IP를 쓰면 그 IP의 호출만
// 쌓이고, 얻는 것은 "몇 시간 일찍 안다" 정도다.
// 채널: 끄면 정식판만(GitHub의 latest), 켜면 beta-channel 릴리스의 manifest —
// 거기엔 베타와 정식판 가운데 더 새로운 쪽이 실린다. 주소 선택은 Rust가 한다
// (JS updater.check()는 확인할 주소를 바꿀 수 없다).
let betaUpdates = localStorage.getItem("betaUpdates") === "1";
let offeredVersion = null; // 버튼에 띄운 판 — 버튼 글자에서 다시 읽지 않는다(베타 판에는 글자가 섞인다)
let offeredBeta = false;   // 그 판을 어느 채널에서 찾았나
// loud=true(설정의 "지금 확인")면 실패를 던진다. 자동 확인은 조용히 넘긴다.
async function checkUpdate(beta = betaUpdates, loud = false) {
  const btn = $("#btn-update");
  if (!btn.classList.contains("hidden")) {
    // 이미 같은 채널의 버튼이 떠 있으면 더 물어볼 게 없다. 채널이 바뀌었으면 앞 제안을
    // 거둔다 — 남겨 두면 베타를 꺼도 베타 설치 버튼이 남는다.
    if (offeredBeta === beta) return offeredVersion || "";
    if (btn.disabled) return null; // 받는 중이면 건드리지 않는다
    resetUpdateOffer();
  }
  try {
    let version;
    try {
      version = await invoke("check_update", { beta });
    } catch (e) {
      // 확인 경로를 Rust로 옮긴 지 얼마 안 됐다. 거기서 실패하면 정식 채널만큼은 예전
      // JS 경로로 한 번 더 본다 — 새 경로가 잘못돼도 업데이트가 끊기지 않게.
      uiTrace("update-check-failed", String(e));
      if (!beta) return await legacyCheckUpdate(btn);
      throw e;
    }
    if (typeof version !== "string" || !version) return null;
    offeredVersion = version;
    offeredBeta = beta;
    btn.textContent = `⬆ v${version} 업데이트`;
    btn.classList.remove("hidden");
    btn.onclick = async () => {
      btn.disabled = true;
      btn.textContent = "다운로드 중…";
      try {
        // 받고 설치한 뒤 Rust가 다시 띄운다 (Windows는 설치 프로그램이 앱을 끝낸다)
        await invoke("install_update");
      } catch (e) {
        btn.textContent = "업데이트 실패";
        btn.title = String(e);
        btn.disabled = false;
      }
    };
    return version;
  } catch (e) {
    if (loud) throw e; // 자동 확인은 오프라인 등을 조용히 무시한다
  }
  return null;
}
// 예전 확인 경로(JS 업데이트 플러그인). 정식 채널만 볼 수 있다 — 안전망으로만 쓴다.
async function legacyCheckUpdate(btn) {
  const updater = window.__TAURI__ && window.__TAURI__.updater;
  if (!updater) return null;
  const update = await updater.check();
  if (!update) return null;
  offeredVersion = update.version;
  offeredBeta = false;
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
      btn.title = String(e);
      btn.disabled = false;
    }
  };
  return update.version;
}

function resetUpdateOffer() {
  const btn = $("#btn-update");
  btn.classList.add("hidden");
  btn.disabled = false;
  btn.onclick = null;
  offeredVersion = null;
}
setTimeout(checkUpdate, 5000);
setInterval(checkUpdate, 6 * 3600 * 1000); // 6시간마다
// 그 사이에 확인하고 싶으면 설정의 "지금 확인"을 누른다 — 폴링에 얹어 자동으로 두드리지
// 않는다. 확인은 GitHub 릴리스의 latest.json을 받아오는 네트워크 호출이다.

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

// 숨겨져 있는 동안 건너뛴 갱신을 다시 보이는 순간 한 번에 따라잡는다
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) detach("refresh:visible", refreshSessions());
});

// 주기적 목록 갱신 (20초). setInterval이 아니라 끝난 뒤에 다시 예약한다 —
// 스캔이 20초보다 오래 걸려도 큐가 쌓이지 않고, 느린 동안은 저절로 뜸해진다.
(function pollLoop() {
  setTimeout(async () => {
    try { await refreshSessions(); } catch (err) {
      reportFatal(`refresh:poll: ${err && err.stack ? err.stack : err}`);
    } finally { pollLoop(); }
  }, 20000);
})();
// 첫 갱신은 usage.js까지 로드된 뒤에 돈다 — refreshSessions가 그 파일의
// syncCtxGauges와 fmtTok을 부르는데, 여기서 바로 부르면 아직 정의 전이다.
// 이미 로드가 끝난 뒤라면 DOMContentLoaded는 다시 오지 않으므로 바로 부른다.
if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", refreshSessions);
} else {
  refreshSessions();
}
