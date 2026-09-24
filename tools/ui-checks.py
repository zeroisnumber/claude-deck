"""화면 쪽 회귀 검사 — 헤드리스 크롬에 ui/index.html을 띄우고 Tauri를 흉내 내어 돌린다.

    python tools/ui-checks.py            # 전부
    python tools/ui-checks.py wheel ime  # 이름에 해당 글자가 든 것만

실제 앱 없이 잡을 수 있는 것만 본다. 이 검사들이 잡은 적이 있는 것: 목록 전체가 비는 것,
클릭 사이에 끼는 마우스 신호, 덜 가는 휠, 터미널 둘레의 검은 띠, 두 번 실행되는 단축키.
필요한 것: Chrome, python `websocket-client`.
"""
import base64, json, os, shutil, subprocess, sys, tempfile, time, traceback, urllib.request

import websocket

CHROME = r"C:\Program Files\Google\Chrome\Application\chrome.exe"
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PAGE = "file:///" + os.path.join(ROOT, "ui", "index.html").replace("\\", "/")
PORT = 9471

# 앱 명령을 흉내 낸다. 부른 것은 window.__calls에, PTY로 보낸 것은 window.__sent에 쌓인다.
STUB = r"""
localStorage.clear(); // 앞 검사가 남긴 설정(묻지 않기·폰트 크기 등)이 섞이지 않게
localStorage.setItem('openTabs', JSON.stringify(%(open_tabs)s));
localStorage.setItem('webgl', %(webgl)s);
%(extra_storage)s
window.__calls = []; window.__sent = []; window.__handlers = {}; window.__gen = 0;
const __SESS = %(sessions)s;
window.__TAURI__ = {
  core: { invoke: (c, a) => {
    window.__calls.push([c, a]);
    if (c === 'write_pty') window.__sent.push(a.data);
    if (c === 'list_sessions') return Promise.resolve(__SESS);
    if (c === 'trace_enabled') return Promise.resolve(%(trace)s);
    if (c === 'spawn_pty') return new Promise(r => setTimeout(() => r(++window.__gen), 20));
    return Promise.resolve(null);
  } },
  event: { listen: (name, fn) => { window.__handlers[name] = fn; return Promise.resolve(() => {}); } },
};
"""


def fake_sessions(n=3, over=None):
    over = over or {}
    out = []
    for i in range(n):
        s = {"session_id": f"aaaaaaaa-000{i}-0000-0000-00000000000{i}", "cwd": f"D:\\work\\proj{i}",
             "project": f"proj{i}", "agent": "claude", "title": f"세션 {i}", "mtime": 1e9 - i,
             "file": f"D:\\x\\{i}.jsonl"}
        s.update(over.get(i, {}))
        out.append(s)
    return out


class Page:
    def __init__(self, ws):
        self.ws, self._id = ws, 0

    def cdp(self, method, **params):
        self._id += 1
        self.ws.send(json.dumps({"id": self._id, "method": method, "params": params}))
        while True:
            m = json.loads(self.ws.recv())
            if m.get("id") == self._id:
                if "error" in m:
                    raise RuntimeError(f"{method}: {m['error']}")
                return m.get("result", {})

    def js(self, expr):
        r = self.cdp("Runtime.evaluate", expression=expr, awaitPromise=True, returnByValue=True)
        if "exceptionDetails" in r:
            raise AssertionError("페이지 오류: " + str(r["exceptionDetails"].get("exception", {}).get("description", r))[:400])
        return r["result"].get("value")

    def load(self, sessions=None, open_tabs=(), webgl=False, trace=False, extra_storage=""):
        # 앞 검사의 가짜 앱을 떼고 새로 붙인다 (쌓이면 앞 것이 먼저 돌아 뒤섞인다)
        if getattr(self, "_stub", None):
            self.cdp("Page.removeScriptToEvaluateOnNewDocument", identifier=self._stub)
        self._stub = self.cdp("Page.addScriptToEvaluateOnNewDocument", source=STUB % {
            "open_tabs": json.dumps(list(open_tabs)), "webgl": json.dumps("1" if webgl else "0"),
            "sessions": json.dumps(sessions or [], ensure_ascii=False), "trace": "true" if trace else "false",
            "extra_storage": extra_storage})["identifier"]
        self.cdp("Page.navigate", url=PAGE)
        time.sleep(2.5)

    def key(self, key, code, vk, modifiers=0, text=None):
        down = dict(type="rawKeyDown" if text is None else "keyDown", key=key, code=code,
                    windowsVirtualKeyCode=vk, modifiers=modifiers)
        if text is not None:
            down["text"] = text
        self.cdp("Input.dispatchKeyEvent", **down)
        self.cdp("Input.dispatchKeyEvent", type="keyUp", key=key, code=code, windowsVirtualKeyCode=vk, modifiers=modifiers)

    def rect(self, selector):
        return json.loads(self.js(f"JSON.stringify(document.querySelector({json.dumps(selector)}).getBoundingClientRect())"))

    def term(self, tab="t", cwd="D:/x"):
        self.js(f"makeTerm('{tab}', '{tab}', '{cwd}'); activate('{tab}'); terms.get('{tab}').term.focus(); 'ok'")
        time.sleep(0.6)

    def write(self, text, tab="t"):
        self.js(f"new Promise(r => terms.get('{tab}').term.write({json.dumps(text)}, r))")


def b64(s):
    return base64.b64encode(s.encode()).decode()


CHECKS = []


def check(fn):
    CHECKS.append(fn)
    return fn


# ---------------------------------------------------------------- 검사들

@check
def shortcut_moves_one_tab(p):
    """터미널에 포커스가 있을 때 Ctrl+Tab이 두 번 실행되던 버그"""
    p.load()
    for t in "abc":
        p.term(t)
    p.js("activate('a'); terms.get('a').term.focus()")
    time.sleep(0.3)
    p.key("Tab", "Tab", 9, modifiers=2)
    time.sleep(0.3)
    assert p.js("activeId") == "b", f"Ctrl+Tab 한 번에 {p.js('activeId')}로 갔다"


def mouse_mode(p):
    p.term()
    p.write("\x1b[?1003h\x1b[?1006h")
    time.sleep(0.2)
    r = p.rect(".term-container.visible .xterm")
    return r["x"] + 100, r["y"] + 150


@check
def click_is_not_split_by_a_late_move(p):
    """움직이자마자 누르면 늦게 보낸 움직임이 누르기와 떼기 사이에 끼던 버그"""
    p.load()
    x, y = mouse_mode(p)
    p.js("window.__sent = []")
    for i in range(3):
        p.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=x + i * 30, y=y)
        time.sleep(0.01)
    p.cdp("Input.dispatchMouseEvent", type="mousePressed", x=x + 200, y=y + 60, button="left", clickCount=1, buttons=1)
    time.sleep(0.08)
    p.cdp("Input.dispatchMouseEvent", type="mouseReleased", x=x + 200, y=y + 60, button="left", clickCount=1, buttons=0)
    time.sleep(0.2)
    seq = p.js("window.__sent")
    press = next(i for i, v in enumerate(seq) if v.startswith("\x1b[<0;") and v.endswith("M"))
    release = next(i for i, v in enumerate(seq) if v.startswith("\x1b[<0;") and v.endswith("m"))
    assert seq[press + 1:release] == [], f"클릭 사이에 끼었다: {seq[press + 1:release]}"


@check
def wheel_sends_one_report_per_notch(p):
    """휠을 모아 보내면 이동량이 사라지던 버그 (12칸 → 7칸)"""
    p.load()
    x, y = mouse_mode(p)
    wheel = "window.__sent.filter(s => /\\x1b\\[<6[45];/.test(s)).length"
    p.js("window.__sent = []")
    for _ in range(12):
        p.cdp("Input.dispatchMouseEvent", type="mouseWheel", x=x, y=y, deltaX=0, deltaY=100)
        time.sleep(0.01)
    time.sleep(0.2)
    assert p.js(wheel) == 12, f"보통 휠 12칸이 {p.js(wheel)}번"
    p.js("window.__sent = []")
    for _ in range(10):
        p.cdp("Input.dispatchMouseEvent", type="mouseWheel", x=x, y=y, deltaX=0, deltaY=40)
        time.sleep(0.02)
    time.sleep(0.2)
    assert p.js(wheel) == 4, f"40px×10(4칸)이 {p.js(wheel)}번"


@check
def mouse_moves_are_thinned_but_end_where_the_mouse_stopped(p):
    """마우스를 스치면 칸마다 신호가 가서 클로드가 키 입력을 미루던 것"""
    p.load()
    x, y = mouse_mode(p)
    p.js("window.__sent = []")
    for i in range(60):
        p.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=x + i * 6, y=y)
        time.sleep(0.008)
    time.sleep(0.2)
    moves = p.js("window.__sent.filter(s => /\\x1b\\[<3[5-9];/.test(s))")
    assert len(moves) < 45, f"60번 움직임에 신호 {len(moves)}개 — 솎이지 않았다"
    col = p.js("(() => { const t = terms.get('t'); const c = t.term._core._renderService.dimensions.css.cell; "
               f"const s = t.term.element.querySelector('.xterm-screen').getBoundingClientRect(); "
               f"return Math.floor(({x + 59 * 6} - s.left) / c.width) + 1 }})()")
    assert moves[-1].split(";")[1] == str(col), f"마지막 신호 {moves[-1]!r}가 멈춘 칸({col})이 아니다"


@check
def plain_scroll_still_works_outside_mouse_mode(p):
    """마우스 신호 모드가 아닐 때 휠은 xterm 자체 스크롤이어야 한다"""
    p.load()
    p.term()
    p.js("(async () => { const t = terms.get('t').term; for (let i = 0; i < 300; i++) await new Promise(r => t.write('line ' + i + '\\r\\n', r)); })()")
    time.sleep(0.3)
    before = p.js("terms.get('t').term.buffer.active.viewportY")
    r = p.rect(".term-container.visible .xterm")
    for _ in range(5):
        p.cdp("Input.dispatchMouseEvent", type="mouseWheel", x=r["x"] + 100, y=r["y"] + 100, deltaX=0, deltaY=-100)
        time.sleep(0.03)
    time.sleep(0.3)
    assert p.js("terms.get('t').term.buffer.active.viewportY") < before, "위로 스크롤되지 않았다"


@check
def restore_asks_and_opens_only_the_picked(p):
    """켤 때 지난 탭을 전부 띄우지 않고 묻는다. 위험한 id는 빈 탭 없이 건너뛴다."""
    sess = fake_sessions(3, {2: {"session_id": "evil&calc"}})
    p.load(sessions=sess, open_tabs=[s["session_id"] for s in sess])
    assert p.js("window.__calls.filter(c => c[0] === 'spawn_pty').length") == 0, "묻기 전에 띄웠다"
    assert p.js("document.querySelectorAll('.restore-row').length") == 3, "카드에 후보가 없다"
    p.js("document.querySelectorAll('.restore-row input')[1].click(); document.querySelector('.restore-ok').click()")
    time.sleep(1.0)
    spawned = p.js("window.__calls.filter(c => c[0] === 'spawn_pty').map(c => c[1].id)")
    assert spawned == [sess[0]["session_id"]], f"띄운 것 {spawned}"
    assert p.js("[...terms.keys()]") == [sess[0]["session_id"]], "위험한 id가 빈 탭을 남겼다"


@check
def sidebar_renders_with_ping_and_markdown_summaries(p):
    """요약이 캐시 유지 핑이면 숨기다가 목록 전체가 비던 버그, 마크다운 기호 노출"""
    sess = fake_sessions(3, {0: {"bg_detail": 'reply "." only', "bg_state": "done"},
                             1: {"bg_detail": "**부채 배치**를 넣었다", "bg_state": "done"}})
    p.load(sessions=sess)
    assert p.js("document.querySelectorAll('.session-item').length") == 3, "목록이 비었다"
    details = p.js("[...document.querySelectorAll('.si-bg-detail')].map(e => e.textContent)")
    assert details == ["부채 배치를 넣었다"], f"요약 칸: {details}"


@check
def session_moved_banner_and_reopen(p):
    """세션이 다른 프로세스로 넘어가면 띠, 다시 열기는 끄고 띄운다"""
    p.load()
    p.term("tab-a")
    p.js("window.__handlers['session-moved']({payload: {id: 'tab-a', from: 1, to: 2}})")
    time.sleep(0.3)
    assert p.js("!!document.querySelector('.term-moved')"), "띠가 없다"
    p.js("window.__calls = []; document.querySelector('.term-moved .moved-reopen').click()")
    time.sleep(0.6)
    assert p.js("window.__calls.map(c => c[0])")[:2] == ["kill_pty", "spawn_pty"], p.js("window.__calls.map(c => c[0])")
    p.js("window.__handlers['session-moved']({payload: {id: 'tab-a', from: 1, to: 2}})")
    time.sleep(0.2)
    p.js("window.__handlers['session-moved']({payload: {id: 'tab-a', from: 1, to: 0}})")
    time.sleep(0.2)
    assert not p.js("!!document.querySelector('.term-moved')"), "주인이 돌아와도 띠가 남았다"


@check
def stale_output_from_a_killed_run_is_dropped(p):
    """다시 연 탭에 끈 프로세스의 출력이 섞이던 것"""
    p.load()
    p.term("tab-a")
    p.js("spawnInto('tab-a', terms.get('tab-a'), 'x')")
    time.sleep(0.2)
    out = lambda gen, text: p.js(f"window.__handlers['pty-output']({{payload: {{id: 'tab-a', generation: {gen}, data: '{b64(text)}'}}}})")
    p.js("reopenMoved('tab-a')")
    time.sleep(0.4)
    out(1, "STALE")
    out(2, "FRESH")
    time.sleep(0.3)
    text = p.js("(() => { const b = terms.get('tab-a').term.buffer.active; let s = ''; for (let y = 0; y < 8; y++) s += b.getLine(y).translateToString(true); return s })()")
    assert "FRESH" in text and "STALE" not in text, text


@check
def terminal_rows_fit_at_every_window_height(p):
    """여백이 행 수 계산에서 빠져 마지막 줄이 창 밖으로 넘치던 버그"""
    p.load()
    p.term()
    bad = []
    for h in range(600, 760, 7):
        p.cdp("Emulation.setDeviceMetricsOverride", width=1280, height=h, deviceScaleFactor=1, mobile=False)
        time.sleep(0.2)
        r = p.js("(() => { const t = terms.get('t'); t.fit.fit(); "
                 "return [Math.round(t.term.element.querySelector('.xterm-screen').getBoundingClientRect().bottom), "
                 "Math.round(document.querySelector('#term-area').getBoundingClientRect().bottom)] })()")
        if r[0] > r[1]:
            bad.append(h)
    p.cdp("Emulation.clearDeviceMetricsOverride")
    assert not bad, f"넘친 창 높이: {bad}"


@check
def no_black_band_around_the_terminal(p):
    """여백 자리에 스크롤 영역의 기본 배경(#000)이 드러나던 버그"""
    p.load()
    p.term()
    vp, body = p.js("(() => { const x = terms.get('t').term.element; "
                    "return [getComputedStyle(x.querySelector('.xterm-viewport')).backgroundColor, "
                    "getComputedStyle(document.body).backgroundColor] })()")
    assert vp == body, f"스크롤 영역 {vp}, 바탕 {body}"


@check
def ime_orphan_reattaches_once_and_drops_duplicates(p):
    """조합 없이 한글이 오면 입력기가 떨어진 것 — 두 번째에 재연결, 50ms 안 같은 글자는 버림"""
    p.load(trace=True)
    p.term("i")
    trigger = "terms.get('i').term._core.coreService.triggerDataEvent"
    p.js(f"{trigger}('나', true); {trigger}('나', true)")
    time.sleep(0.3)
    assert p.js("window.__sent") == ["나"], f"보낸 것 {p.js('window.__sent')}"
    assert p.js("window.__calls.filter(c => c[0] === 'rebind_ime').length") == 0, "한 번에 재연결했다(붙여넣기·음성 입력도 걸린다)"
    p.js(f"{trigger}('다', true)")
    time.sleep(0.3)
    assert p.js("window.__calls.filter(c => c[0] === 'rebind_ime').length") == 1, "두 번째에도 재연결하지 않았다"
    time.sleep(1.6)
    p.js("window.__calls = []; pasting = true; try { terms.get('i').term.paste('라'); } finally { pasting = false; }")
    time.sleep(0.2)
    assert not p.js("window.__calls.some(c => c[0] === 'trace_ui' && String(c[1] && c[1].kind).startsWith('ime:orphan'))"), "붙여넣기를 떨어진 입력기로 봤다"
    p.js("document.querySelector('#search').focus(); document.querySelector('#focus-sink').focus()")
    time.sleep(0.1)
    got = p.js("document.activeElement.className || document.activeElement.id || document.activeElement.tagName")
    assert got == "xterm-helper-textarea", f"포커스 받침이 터미널로 돌려주지 않았다 (지금 {got})"


@check
def ime_composition_is_sent_once(p):
    """정상 조합은 한 번만 나가고 재연결을 부르지 않는다"""
    p.load(trace=True)
    p.term("i")
    for x in ["ㄱ", "가"]:
        p.cdp("Input.imeSetComposition", text=x, selectionStart=1, selectionEnd=1)
        time.sleep(0.1)
    p.cdp("Input.insertText", text="가")
    time.sleep(0.3)
    assert p.js("window.__sent") == ["가"], p.js("window.__sent")
    assert p.js("window.__calls.filter(c => c[0] === 'rebind_ime').length") == 0


@check
def keyboard_pick_survives_a_rerender(p):
    """사이드바에서 고른 줄을 번호로 기억해 새로고침 뒤 엉뚱한 세션을 열던 것"""
    p.load(sessions=fake_sessions(3))
    p.js("document.querySelector('#search').focus()")
    for _ in range(2):
        p.key("ArrowDown", "ArrowDown", 40)
    picked = p.js("document.querySelector('.session-item.kb')?.dataset.id")
    p.js("renderSidebar()")
    assert p.js("document.querySelector('.session-item.kb')?.dataset.id") == picked, "다시 그리니 표시가 사라졌다"
    p.key("Enter", "Enter", 13)
    time.sleep(0.5)
    spawned = p.js("window.__calls.filter(c => c[0] === 'spawn_pty').map(c => c[1].id)")
    assert spawned == [picked], f"고른 {picked} 대신 {spawned}"


@check
def typing_right_after_opening_reaches_the_terminal(p):
    """세션을 열자마자 친 키가 터미널로 가야 한다"""
    p.load(sessions=fake_sessions(2))
    r = p.rect(".session-item")
    p.cdp("Input.dispatchMouseEvent", type="mousePressed", x=r["x"] + 40, y=r["y"] + 10, button="left", clickCount=1, buttons=1)
    p.cdp("Input.dispatchMouseEvent", type="mouseReleased", x=r["x"] + 40, y=r["y"] + 10, button="left", clickCount=1, buttons=0)
    time.sleep(0.1)
    p.key("a", "KeyA", 65, text="a")
    time.sleep(0.3)
    assert "a" in p.js("window.__sent"), f"보낸 것 {p.js('window.__sent')}"


@check
def loading_hint_until_the_first_frame(p):
    """뜨는 동안 안내, 첫 화면이 오면 거둔다. 입력이 전달된다는 말은 클로드에서만."""
    p.load()
    p.term("tab-a")
    p.js("terms.get('tab-a').agent = 'claude'; spawnInto('tab-a', terms.get('tab-a'), 'x')")
    time.sleep(0.2)
    hint = p.js("document.querySelector('.term-loading')?.textContent")
    assert hint and "그대로 전달" in hint, f"안내 {hint!r}"
    p.js(f"window.__handlers['pty-output']({{payload: {{id: 'tab-a', generation: 1, data: '{b64('x' * 1500)}'}}}})")
    time.sleep(0.2)
    assert not p.js("!!document.querySelector('.term-loading')"), "첫 화면 뒤에도 남았다"
    p.term("tab-b")
    p.js("terms.get('tab-b').agent = 'codex'; spawnInto('tab-b', terms.get('tab-b'), 'x')")
    time.sleep(0.2)
    hint = p.js("document.querySelector('.term-container.visible .term-loading')?.textContent")
    assert hint and "그대로 전달" not in hint, f"확인 안 한 에이전트에 약속했다: {hint!r}"


@check
def turn_chart_groups_thousands_of_bars(p):
    """턴 7857개를 막대 하나씩 그리느라 화면이 멈추던 것"""
    p.load()
    r = p.js("""(() => {
      const n = 7857, cost = [], misses = [];
      for (let i = 0; i < n; i++) cost.push(Math.abs(Math.sin(i)) * (i % 97 ? 3 : 50));
      for (let i = 0; i < n; i += 211) misses.push(i);
      renderCurve(cost, cost.reduce((a, b) => a + b, 0), misses);
      return [document.querySelectorAll('#turns-chart rect').length,
              document.querySelectorAll('#turns-chart rect.miss').length, misses.length];
    })()""")
    assert r[0] <= 400, f"막대 {r[0]}개"
    assert r[1] == r[2], f"끊긴 턴 {r[2]}개 중 빨간 막대 {r[1]}개"


@check
def webgl_is_kept_on_recent_tabs_only(p):
    """탭마다 WebGL을 붙이면 16개쯤에서 브라우저가 컨텍스트를 버린다"""
    p.load(webgl=True)
    for i in range(8):
        p.term(f"t{i}")
    on = p.js("[...terms.entries()].filter(([k, t]) => t.webgl).map(([k]) => k)")
    if not on:
        return "skip: 이 크롬에서 WebGL을 못 씀"
    assert len(on) <= 4, f"GPU 붙은 탭 {on}"
    p.js("activate('t0')")
    time.sleep(0.4)
    assert p.js("!!terms.get('t0').webgl"), "다시 본 탭에 GPU가 안 붙었다"


# ---------------------------------------------------------------- 실행

def main():
    want = sys.argv[1:]
    chosen = [c for c in CHECKS if not want or any(w in c.__name__ for w in want)]
    profile = tempfile.mkdtemp(prefix="deck-ui-checks-")
    chrome = subprocess.Popen([
        CHROME, "--headless=new", f"--remote-debugging-port={PORT}", f"--user-data-dir={profile}",
        "--window-size=1280,800", "--hide-scrollbars", "--force-device-scale-factor=1",
        "--use-angle=swiftshader", "--enable-unsafe-swiftshader",
        "--allow-file-access-from-files", "--remote-allow-origins=*", "about:blank",
    ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    failed = 0
    try:
        target = None
        for _ in range(60):
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{PORT}/json") as r:
                    target = [x for x in json.load(r) if x["type"] == "page"][0]
                break
            except Exception:
                time.sleep(0.3)
        ws = websocket.create_connection(target["webSocketDebuggerUrl"], timeout=60)
        p = Page(ws)
        p.cdp("Page.enable")
        p.cdp("Runtime.enable")
        # 헤드리스는 창에 포커스가 없다고 보고 focus 이벤트를 안 보낸다. 실제 앱은 사용자가
        # 치는 동안 창에 포커스가 있으니 그렇게 흉내 낸다.
        p.cdp("Emulation.setFocusEmulationEnabled", enabled=True)
        for c in chosen:
            try:
                note = c(p)
                print(f"통과  {c.__name__}" + (f"  ({note})" if note else ""))
            except Exception as e:
                failed += 1
                print(f"실패  {c.__name__}: {e}")
                if not isinstance(e, AssertionError):
                    traceback.print_exc()
        ws.close()
    finally:
        chrome.terminate()
        try:
            chrome.wait(timeout=10)
        except Exception:
            chrome.kill()
        shutil.rmtree(profile, ignore_errors=True)
    print(f"\n{len(chosen) - failed}/{len(chosen)} 통과")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
