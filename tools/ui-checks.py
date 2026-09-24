"""화면 쪽 회귀 검사 — 헤드리스 크롬에 ui/index.html을 띄우고 Tauri를 흉내 내어 돌린다.

    python tools/ui-checks.py            # 전부
    python tools/ui-checks.py wheel ime  # 이름에 해당 글자가 든 것만
    python tools/ui-checks.py --csp      # tauri.conf.json의 CSP를 걸고 전부 (위반이 하나라도 나면 실패)

--csp는 ui/를 로컬 HTTP로 띄우고 CSP를 응답 헤더로 보낸다 — 윈도우의 Tauri가
http://tauri.localhost에 거는 방식과 같다. 가짜 앱(STUB)은 Tauri의 초기화 스크립트처럼
CSP 밖에서 돈다. 포트가 겹치면 UI_CHECKS_PORT / UI_CHECKS_HTTP_PORT로 바꾼다.

실제 앱 없이 잡을 수 있는 것만 본다. 이 검사들이 잡은 적이 있는 것: 목록 전체가 비는 것,
클릭 사이에 끼는 마우스 신호, 덜 가는 휠, 터미널 둘레의 검은 띠, 두 번 실행되는 단축키.
필요한 것: Chrome, python `websocket-client`.
"""
import base64, functools, http.server, json, os, shutil, subprocess, sys, tempfile, threading, time, traceback, urllib.request

import websocket

def find_chrome():
    """CHROME 환경 변수가 있으면 그것, 없으면 흔히 깔리는 자리를 차례로 본다 (CI 러너 포함)."""
    env = os.environ.get("CHROME")
    if env:
        if not os.path.isfile(env):
            sys.exit(f"CHROME={env} 가 가리키는 파일이 없다")
        return env
    tail = os.path.join("Google", "Chrome", "Application", "chrome.exe")
    candidates = [os.path.join(os.environ[v], tail)
                  for v in ("ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA") if os.environ.get(v)]
    candidates.append(os.path.join(r"C:\Program Files", tail))
    for c in candidates:
        if os.path.isfile(c):
            return c
    for name in ("chrome", "google-chrome", "chromium"):
        found = shutil.which(name)
        if found:
            return found
    sys.exit("크롬을 찾지 못했다. CHROME 환경 변수로 chrome.exe 경로를 알려 준다. 찾아본 곳:\n  " + "\n  ".join(candidates))


ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PAGE = "file:///" + os.path.join(ROOT, "ui", "index.html").replace("\\", "/")
def free_port():
    # 고정 번호를 쓰면 동시에 돈 다른 검사의 크롬에 붙어 서로의 창을 끈다
    import socket
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


PORT = int(os.environ.get("UI_CHECKS_PORT") or free_port())
HTTP_PORT = int(os.environ.get("UI_CHECKS_HTTP_PORT") or free_port())
CSP = None   # --csp일 때 tauri.conf.json의 정책 문자열


def conf_csp():
    if os.environ.get("UI_CHECKS_CSP"):   # 정책을 바꿔 가며 볼 때
        return os.environ["UI_CHECKS_CSP"]
    with open(os.path.join(ROOT, "src-tauri", "tauri.conf.json"), encoding="utf-8") as f:
        csp = json.load(f)["app"]["security"].get("csp")
    if isinstance(csp, dict):   # Tauri는 지시어 맵 형식도 받는다
        csp = "; ".join(f"{k} {v if isinstance(v, str) else ' '.join(v)}" for k, v in csp.items())
    if not csp:
        sys.exit("tauri.conf.json에 csp가 없다")
    # 여기서는 정책을 그대로 건다. Tauri는 index.html에 <style>·인라인 <script>·http <script src>가
    # 있으면 nonce를 덧붙이는데, style-src에 nonce가 붙는 순간 'unsafe-inline'이 무시되어
    # xterm이 깨진다. 그런 태그가 생기면 이 검사는 실제 앱과 달라지므로 먼저 멈춘다.
    with open(os.path.join(ROOT, "ui", "index.html"), encoding="utf-8") as f:
        html = f.read()
    import re
    if re.search(r"<style[\s>]|<script(?![^>]*\ssrc=)[^>]*>|<script[^>]*\ssrc=[\"']?http", html, re.I):
        sys.exit("index.html에 <style>/인라인 <script>/http 스크립트가 있다 — Tauri가 CSP에 nonce를 붙여 "
                 "style-src 'unsafe-inline'이 꺼진다. 파일로 빼거나 dangerousDisableAssetCspModification을 보라.")
    return csp


def serve_ui(csp):
    class H(http.server.SimpleHTTPRequestHandler):
        def end_headers(self):
            self.send_header("Content-Security-Policy", csp)
            self.send_header("Cache-Control", "no-store")
            super().end_headers()

        def log_message(self, *a):
            pass

    srv = http.server.ThreadingHTTPServer(("127.0.0.1", HTTP_PORT),
                                          functools.partial(H, directory=os.path.join(ROOT, "ui")))
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv

# 앱 명령을 흉내 낸다. 부른 것은 window.__calls에, PTY로 보낸 것은 window.__sent에 쌓인다.
# CSP 위반은 window.__csp에 — 문서가 파싱되기 전에 걸어야 막힌 <script src>도 잡힌다.
STUB = r"""
window.__csp = [];
document.addEventListener('securitypolicyviolation', e => window.__csp.push(
  [e.effectiveDirective, e.blockedURI, (e.sample || '').slice(0, 80), e.sourceFile + ':' + e.lineNumber].join(' | ')));
localStorage.clear(); // 앞 검사가 남긴 설정(묻지 않기·폰트 크기 등)이 섞이지 않게
localStorage.setItem('openTabs', JSON.stringify(%(open_tabs)s));
localStorage.setItem('webgl', %(webgl)s);
%(extra_storage)s
window.__calls = []; window.__sent = []; window.__handlers = {}; window.__gen = 0;
const __SESS = %(sessions)s;
const __REPLIES = %(replies)s; // 검사마다 정한 명령 응답 (없으면 null)
window.__TAURI__ = {
  core: { invoke: (c, a) => {
    window.__calls.push([c, a]);
    if (c === 'write_pty') window.__sent.push(a.data);
    // 검사가 정한 답이 늘 먼저다 — 아래 기본값이 가로채지 않게
    if (c in __REPLIES) return Promise.resolve(__REPLIES[c]);
    if (c === 'list_sessions') return Promise.resolve(__SESS);
    if (c === 'trace_enabled') return Promise.resolve(%(trace)s);
    if (c === 'agent_versions') return Promise.resolve([]);
    if (c === 'check_update') {
      if (window.__updateFails) return Promise.reject('updater error');
      return Promise.resolve(window.__updateFor ? window.__updateFor(a.beta) : null);
    }
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
        self.violations = []

    def harvest(self):
        """지금 문서에서 난 CSP 위반을 모아 둔다 (다음 load가 문서를 갈아엎기 전에)."""
        try:
            got = self.js("window.__csp ? window.__csp.splice(0) : []") or []
        except Exception:
            got = []
        self.violations += got
        return got

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

    def load(self, sessions=None, open_tabs=(), webgl=False, trace=False, extra_storage="", replies=None):
        self.harvest()
        # 앞 검사의 가짜 앱을 떼고 새로 붙인다 (쌓이면 앞 것이 먼저 돌아 뒤섞인다)
        if getattr(self, "_stub", None):
            self.cdp("Page.removeScriptToEvaluateOnNewDocument", identifier=self._stub)
        self._stub = self.cdp("Page.addScriptToEvaluateOnNewDocument", source=STUB % {
            "open_tabs": json.dumps(list(open_tabs)), "webgl": json.dumps("1" if webgl else "0"),
            "sessions": json.dumps(sessions or [], ensure_ascii=False), "trace": "true" if trace else "false",
            "extra_storage": extra_storage,
            "replies": json.dumps(replies or {}, ensure_ascii=False)})["identifier"]
        # 고정 시간만 기다리면 느린 러너나 HTTP로 띄울 때(--csp) 스크립트가 덜 올라온 채로
        # 검사가 돌아 가끔 실패했다("makeTerm is not defined", Terminal 없음). 앱이 실제로
        # 준비될 때까지 보고, 검사용 로컬 서버가 파일 하나를 놓친 경우를 위해 한 번 더 띄운다.
        ready = ("document.readyState === 'complete' && typeof Terminal === 'function' "
                 "&& typeof makeTerm === 'function' && typeof renderSidebar === 'function'")
        for attempt in range(2):
            self.cdp("Page.navigate", url=PAGE)
            deadline = time.time() + 15
            ok = False
            while time.time() < deadline:
                try:
                    if self.js(ready):
                        ok = True
                        break
                except Exception:
                    pass
                time.sleep(0.1)
            if ok:
                break
        else:
            raise AssertionError("15초 안에 앱이 준비되지 않았다 (두 번 띄움)")
        time.sleep(0.8)  # 첫 목록 불러오기(list_sessions) 같은 시작 직후 비동기 작업

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
def terminal_refits_when_the_monitor_scale_changes(p):
    """배율이 다른 모니터로 옮기면 창의 CSS 크기는 그대로라 ResizeObserver가 안 불린다.
    xterm은 글자 크기를 다시 재지만 칸 수는 그대로여서 WebGL에선 오른쪽 열이 잘렸다.
    헤드리스의 배율 흉내는 matchMedia/resize 이벤트를 안 보내므로 resize를 직접 쏜다
    (실제 창에서는 둘 다 온다 — xterm도 이 둘로 배율 변화를 안다)."""
    p.load(webgl=True)
    p.term()
    if not p.js("!!terms.get('t').webgl"):
        return "skip: 이 크롬에서 WebGL을 못 씀"
    p.cdp("Emulation.setDeviceMetricsOverride", width=1280, height=720, deviceScaleFactor=1, mobile=False)
    time.sleep(0.4)
    p.js("(() => { const t = terms.get('t'); t.fit.fit(); })()")
    bad = []
    for dsf in (1.25, 1.5, 1.75, 2, 1):
        p.cdp("Emulation.setDeviceMetricsOverride", width=1280, height=720, deviceScaleFactor=dsf, mobile=False)
        time.sleep(0.3)
        p.js("dispatchEvent(new Event('resize'))")
        time.sleep(0.4)
        r = p.js("(() => { const t = terms.get('t'); const d = t.fit.proposeDimensions(); "
                 "const s = t.term.element.querySelector('.xterm-screen').getBoundingClientRect(); "
                 "const a = document.querySelector('#term-area').getBoundingClientRect(); "
                 "const last = window.__calls.filter(c => c[0] === 'resize_pty').pop(); "
                 "return {dpr: devicePixelRatio, cols: t.term.cols, rows: t.term.rows, want: [d.cols, d.rows], "
                 "right: Math.round(s.right) <= Math.round(a.right), bottom: Math.round(s.bottom) <= Math.round(a.bottom), "
                 "pty: last && [last[1].cols, last[1].rows]} })()")
        if [r["cols"], r["rows"]] != r["want"] or not (r["right"] and r["bottom"]) or r["pty"] != [r["cols"], r["rows"]]:
            bad.append((dsf, r))
    p.cdp("Emulation.clearDeviceMetricsOverride")
    assert not bad, f"배율 바뀐 뒤 안 맞음: {bad}"


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
def status_line_flag_only_for_claude(p):
    """상태줄을 켜 두면 codex 새 세션이 '--settings'를 몰라 아예 안 뜨던 것"""
    p.load()
    r = p.js("""(() => {
      statusLineOn = true; statusLinePath = 'C:/x/settings.json';
      return [composeCommand(null, {cmd: 'claude'}), composeCommand(null, {cmd: 'codex'})];
    })()""")
    assert "--settings" in r[0], f"클로드에 빠졌다: {r[0]!r}"
    assert "--settings" not in r[1], f"codex에 붙었다: {r[1]!r}"


@check
def manual_update_check_reports_failure(p):
    """확인이 실패했는데 '최신 버전입니다'라고 하던 것"""
    p.load()
    # 새 경로도 옛 경로도 실패 (오프라인)
    p.js("window.__updateFails = true; "
         "window.__TAURI__.updater = { check: () => Promise.reject('offline') }; "
         "document.querySelector('#btn-check-update').click()")
    time.sleep(0.3)
    txt = p.js("document.querySelector('#update-state').textContent")
    assert "실패" in txt, f"상태 {txt!r}"


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


def calls(p, name):
    return p.js(f"window.__calls.filter(c => c[0] === {json.dumps(name)}).map(c => c[1])")


def rows(p):
    return p.js("[...document.querySelectorAll('.session-item')].map(e => e.dataset.id)")


def store(p, key):
    return p.js(f"localStorage.getItem({json.dumps(key)})")


@check
def settings_save_persists_and_cancel_discards(p):
    """설정 저장은 프로필·전역 env·토글을 남기고 캐시 유지를 Rust로 보낸다. 취소는 아무것도 안 남긴다."""
    p.load(sessions=fake_sessions(1))
    p.term()
    p.js("document.querySelector('#btn-settings').click()")
    time.sleep(0.3)
    assert p.js("document.querySelectorAll('#profile-list .lrow').length") == 3, "기본 프로필 셋이 안 보인다"
    p.js("""(() => {
      document.querySelector('#profile-add').click();
      const r = [...document.querySelectorAll('#profile-list .lrow')].pop();
      r.querySelector('.l-name').value = '프록시';
      r.querySelector('.l-cmd').value = 'my-proxy run claude';
      r.querySelector('.l-resume input').checked = false;
      r.querySelector('.l-active input').checked = true;
      document.querySelector('#profile-list .l-del').click(); // 첫 줄(Claude)을 지운다
      document.querySelector('#global-env').value = '  PYTHONUTF8=1;FORCE_COLOR=1  ';
      document.querySelector('#opt-restore-ask').checked = false;
      document.querySelector('#opt-webgl').checked = true;
      const ka = document.querySelector('#opt-keepalive');
      ka.checked = true; ka.dispatchEvent(new Event('change'));
      document.querySelector('#ka-threshold').value = '0.1';   // 0.5분 아래는 0.5분으로
      document.querySelector('#ka-message').value = '   ';      // 비우면 기본 메시지
      window.__calls = [];
      document.querySelector('#lmodal-save').click();
    })()""")
    time.sleep(0.2)
    prof = json.loads(store(p, "profiles"))
    assert [x["name"] for x in prof] == ["Codex", "Gemini", "프록시"], prof
    assert prof[2] == {"name": "프록시", "cmd": "my-proxy run claude", "resume": False}, prof[2]
    assert store(p, "profileSel") == "2", f"고른 프로필 {store(p, 'profileSel')}"
    assert store(p, "globalEnv") == "PYTHONUTF8=1;FORCE_COLOR=1", store(p, "globalEnv")
    assert store(p, "restoreAsk") == "0" and store(p, "webgl") == "1", (store(p, "restoreAsk"), store(p, "webgl"))
    ka = calls(p, "set_keepalive")
    assert ka == [{"enabled": True, "thresholdSecs": 30, "message": 'reply "." only'}], f"set_keepalive {ka}"
    assert json.loads(store(p, "keepAlive"))["thresholdSecs"] == 30
    assert "프록시" in p.js("document.querySelector('#foot-count').textContent"), "고른 프로필이 아래 줄에 없다"
    assert p.js("currentProfile().cmd") == "my-proxy run claude"
    assert p.js("document.querySelector('#lmodal-backdrop').classList.contains('hidden')"), "저장 뒤에도 창이 떠 있다"
    got = p.js("document.activeElement.className")
    assert got == "xterm-helper-textarea", f"닫은 뒤 포커스가 터미널로 안 갔다 ({got})"
    # 다시 열면 저장한 값이 보이고, 고친 뒤 취소하면 아무것도 안 바뀐다
    p.js("document.querySelector('#btn-settings').click()")
    time.sleep(0.3)
    shown = p.js("""[document.querySelector('#global-env').value, document.querySelector('#opt-restore-ask').checked,
                     document.querySelector('#ka-threshold').value, document.querySelectorAll('#profile-list .lrow').length]""")
    assert shown == ["PYTHONUTF8=1;FORCE_COLOR=1", False, "0.5", 3], f"다시 연 설정 {shown}"
    before = p.js("JSON.stringify(localStorage)")
    p.js("""document.querySelector('#global-env').value = 'X=1';
            document.querySelector('#opt-restore-ask').checked = true;
            document.querySelector('#profile-list .l-del').click();
            window.__calls = [];
            document.querySelector('#lmodal-cancel').click()""")
    time.sleep(0.1)
    assert p.js("JSON.stringify(localStorage)") == before, "취소했는데 설정이 바뀌었다"
    assert not calls(p, "set_keepalive"), "취소했는데 캐시 유지를 보냈다"
    assert p.js("globalEnv") == "PYTHONUTF8=1;FORCE_COLOR=1" and p.js("profiles.length") == 3


@check
def restore_opens_without_asking_when_opted_out(p):
    """'다음부터 묻지 않기'를 골랐으면 카드 없이 지난 탭을 바로 연다"""
    sess = fake_sessions(2)
    p.load(sessions=sess, open_tabs=[s["session_id"] for s in sess],
           extra_storage="localStorage.setItem('restoreAsk', '0');")
    assert not p.js("!!document.querySelector('.restore-card')"), "묻지 않기로 했는데 물었다"
    time.sleep(0.3)
    spawned = [c["id"] for c in calls(p, "spawn_pty")]
    assert spawned == [s["session_id"] for s in sess], f"띄운 것 {spawned}"
    assert p.js("activeId") == sess[0]["session_id"], "첫 탭이 앞에 오지 않았다"


@check
def sidebar_status_and_agent_filters(p):
    """검색 앞 기호(! @ # &)는 상태 필터, 뒤 글자는 검색어. 에이전트 버튼은 그 에이전트만."""
    sess = fake_sessions(5, {
        1: {"agent": "codex", "bg_state": "working", "bg_running": True},
        2: {"bg_state": "blocked", "bg_running": True},
        3: {"agent": "gemini", "bg_state": "failed"},
        4: {"bg_state": "done"},
    })
    ids = [s["session_id"] for s in sess]
    p.load(sessions=sess)
    p.term(ids[0], "D:/work/proj0")
    p.term(ids[4], "D:/work/proj4")
    p.js(f"terms.get('{ids[0]}').busy = true; terms.get('{ids[4]}').attention = true")

    def search(q):
        p.js(f"document.querySelector('#search').value = {json.dumps(q)}; renderSidebar()")
        return rows(p)
    assert search("") == ids, "빈 검색에 전부가 안 보인다"
    assert search("!") == [ids[0], ids[1]], f"! 작업 중: {rows(p)}"
    assert search("@") == [ids[2], ids[4]], f"@ 입력 필요·안 본 완료: {rows(p)}"
    assert search("#") == [ids[0], ids[4]], f"# 열린 탭: {rows(p)}"
    assert search("&") == [ids[3]], f"& 실패: {rows(p)}"
    assert search("! proj1") == [ids[1]], f"기호 뒤 검색어: {rows(p)}"
    assert search("PROJ3") == [ids[3]], f"대소문자 무시 검색: {rows(p)}"
    search("")
    p.js("document.querySelector('.af[data-agent=\"codex\"]').click()")
    assert rows(p) == [ids[1]], f"Codex 버튼: {rows(p)}"
    on = p.js("[...document.querySelectorAll('#agent-filter .af.on')].map(b => b.dataset.agent)")
    assert on == ["codex"], f"켜진 버튼 {on}"
    assert search("&") == [], "에이전트 필터와 상태 필터가 겹치지 않는다"
    search("")
    p.js("document.querySelector('.af[data-agent=\"all\"]').click()")
    assert rows(p) == ids, "전체로 돌아오지 않았다"
    assert "세션 5개" in p.js("document.querySelector('#foot-count').textContent")


@check
def rename_survives_rerender_and_saves_on_blur(p):
    """이름 바꾸기 중 폴링이 입력창을 지우지 않고, 포커스를 잃으면 저장, Esc는 취소"""
    sess = fake_sessions(2)
    sid = sess[0]["session_id"]
    p.load(sessions=sess)
    start = f"startRename(sessions[0], document.querySelector('.session-item[data-id=\"{sid}\"]'))"
    p.js(start)
    p.js("renderSidebar()")   # 20초 폴링
    assert p.js("!!document.querySelector('.si-rename')"), "다시 그리니 입력창이 사라졌다"
    p.js("{ const i = document.querySelector('.si-rename'); i.value = '  새 이름  '; i.blur() }")
    time.sleep(0.1)
    assert not p.js("!!document.querySelector('.si-rename')"), "저장 뒤에도 입력창이 남았다"
    title = f"document.querySelector('.session-item[data-id=\"{sid}\"] .si-title-text').textContent"
    assert p.js(title) == "새 이름", p.js(title)
    assert json.loads(store(p, "aliases")) == {sid: "새 이름"}, store(p, "aliases")
    p.js(start)
    p.js("{ const i = document.querySelector('.si-rename'); i.value = '버릴 이름'; "
         "i.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true})) }")
    time.sleep(0.1)
    assert p.js(title) == "새 이름", f"Esc가 저장했다: {p.js(title)}"
    assert not p.js("!!document.querySelector('.si-rename')"), "Esc 뒤에도 입력창이 남았다"
    p.js(start)
    p.js("{ const i = document.querySelector('.si-rename'); i.value = ''; "
         "i.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', bubbles: true})) }")
    time.sleep(0.1)
    assert p.js(title) == "세션 0", f"비우면 원래 이름이어야 한다: {p.js(title)}"
    assert json.loads(store(p, "aliases")) == {}, store(p, "aliases")


@check
def pin_moves_to_top_and_unpin_restores(p):
    """핀 고정은 맨 위로 올리고 기억한다. 해제하면 최근 순으로 돌아간다."""
    sess = fake_sessions(3)
    ids = [s["session_id"] for s in sess]
    p.load(sessions=sess)

    def toggle_pin():
        p.js(f"""(() => {{
          const el = document.querySelector('.session-item[data-id="{ids[2]}"]');
          showCtxMenu({{clientX: 10, clientY: 10, preventDefault() {{}}}}, sessions[2], el);
          [...document.querySelectorAll('#ctx-menu .ctx-item')].find(d => d.textContent.includes('핀')).click();
        }})()""")
    toggle_pin()
    assert rows(p) == [ids[2], ids[0], ids[1]], f"핀 고정 뒤 순서 {rows(p)}"
    assert p.js(f"!!document.querySelector('.session-item[data-id=\"{ids[2]}\"] .si-pin')"), "핀 표시가 없다"
    assert json.loads(store(p, "pins")) == [ids[2]], store(p, "pins")
    p.js("sessions[0].mtime += 100; renderSidebar()")   # 다른 세션이 새로 쓰여도 핀이 위
    assert rows(p)[0] == ids[2], f"핀이 밀려났다 {rows(p)}"
    toggle_pin()
    assert rows(p) == ids, f"핀 해제 뒤 순서 {rows(p)}"
    assert json.loads(store(p, "pins")) == [], store(p, "pins")


@check
def close_tab_activates_neighbor_and_restart_resumes(p):
    """탭을 닫으면 kill_pty 후 남은 탭으로, 끝난 탭의 ↻는 같은 세션을 재개한다"""
    sess = fake_sessions(2)
    a, b = sess[0]["session_id"], sess[1]["session_id"]
    p.load(sessions=sess)
    p.js("(async () => { await openSession(sessions[0]); await openSession(sessions[1]); })()")
    time.sleep(0.3)
    assert json.loads(store(p, "openTabs")) == [a, b]
    p.js("window.__calls = []")
    p.js(f"document.querySelector('.tab[data-id=\"{b}\"] .tab-close').click()")
    time.sleep(0.3)
    assert [c["id"] for c in calls(p, "kill_pty")] == [b], "닫기가 프로세스를 끄지 않았다"
    assert p.js("activeId") == a and p.js("[...terms.keys()]") == [a], "남은 탭으로 가지 않았다"
    assert json.loads(store(p, "openTabs")) == [a], store(p, "openTabs")
    assert p.js("document.querySelectorAll('.tab').length") == 1
    assert not p.js("!!document.querySelector('.tab-restart')"), "살아 있는 탭에 재시작 단추"
    p.js(f"window.__handlers['pty-exit']({{payload: {{id: '{a}'}}}})")
    time.sleep(0.1)
    assert p.js("document.querySelector('.tab').classList.contains('exited')"), "끝난 탭 표시가 없다"
    p.js("window.__calls = []; document.querySelector('.tab-restart').click()")
    time.sleep(0.3)
    sp = calls(p, "spawn_pty")
    assert len(sp) == 1 and sp[0]["id"] == a and f"--resume {a}" in sp[0]["command"], f"재시작 {sp}"
    assert p.js(f"terms.get('{a}').deadGen") == 1, "끈 실행의 출력을 버릴 번호가 없다"
    assert not p.js("document.querySelector('.tab').classList.contains('exited')")
    p.js(f"closeTab('{a}')")
    time.sleep(0.2)
    assert not p.js("document.querySelector('#empty-state').classList.contains('hidden')"), "탭이 없는데 빈 화면이 안 보인다"
    assert json.loads(store(p, "openTabs")) == []


@check
def new_session_asks_before_creating_a_missing_folder(p):
    """없는 폴더면 띄우지 않고 묻는다. 경로를 고치면 안내를 거두고, '만들고 시작'은 만든 뒤 띄운다."""
    p.load()
    p.js("document.querySelector('#btn-new').click()")
    assert not p.js("document.querySelector('#modal-backdrop').classList.contains('hidden')")
    p.js("document.querySelector('#modal-path').value = '\"D:\\\\nope\\\\typo\"'; document.querySelector('#modal-ok').click()")
    time.sleep(0.2)
    assert calls(p, "dir_exists") == [{"path": "D:\\nope\\typo"}], f"따옴표를 안 벗겼다 {calls(p, 'dir_exists')}"
    assert not p.js("document.querySelector('#modal-missing').classList.contains('hidden')"), "없다는 안내가 없다"
    assert "D:\\nope\\typo" in p.js("document.querySelector('#modal-missing-text').textContent")
    assert not calls(p, "spawn_pty") and not calls(p, "create_dir"), "묻기 전에 띄웠거나 만들었다"
    p.js("{ const i = document.querySelector('#modal-path'); i.value += 'x'; i.dispatchEvent(new Event('input')) }")
    assert p.js("document.querySelector('#modal-missing').classList.contains('hidden')"), "경로를 고쳤는데 안내가 남았다"
    p.js("document.querySelector('#modal-path').value = 'D:\\\\nope\\\\typo'; document.querySelector('#modal-ok').click()")
    time.sleep(0.2)
    p.js("document.querySelector('#modal-create').click()")
    time.sleep(0.3)
    assert calls(p, "create_dir") == [{"path": "D:\\nope\\typo"}], calls(p, "create_dir")
    sp = calls(p, "spawn_pty")
    assert len(sp) == 1 and sp[0]["cwd"] == "D:\\nope\\typo", f"띄운 것 {sp}"
    order = p.js("window.__calls.map(c => c[0]).filter(c => c === 'create_dir' || c === 'spawn_pty')")
    assert order == ["create_dir", "spawn_pty"], order
    assert p.js("document.querySelector('#modal-backdrop').classList.contains('hidden')"), "창이 안 닫혔다"
    assert json.loads(store(p, "recentDirs")) == ["D:\\nope\\typo"], store(p, "recentDirs")


@check
def dashboard_names_models_formats_cost_and_switches_period(p):
    """대시보드: 모델은 읽는 이름, 비용은 쉼표, 코덱스는 금액 대신 —, 기간 단추는 다시 불러온다"""
    import datetime
    today = datetime.datetime.now(datetime.timezone.utc).date()
    old = (today - datetime.timedelta(days=20)).isoformat()
    row = lambda **kw: {"date": today.isoformat(), "model": "claude-opus-5-5", "agent": "claude", "project": "deck",
                        "cwd": "D:\\deck", "input": 0, "output": 0, "cache_read": 0, "cache_5m": 0, "cache_1h": 0,
                        "requests": 1, **kw}
    stats = [row(input=1_000_000_000, requests=1234),                                   # $5,000.00
             row(model="gpt-5-codex", agent="codex", project="cx", input=10, output=10),
             row(date=old, project="oldproj", output=1_000_000)]
    p.load(replies={"usage_stats": stats})
    p.js("document.querySelector('#btn-dash').click()")
    time.sleep(0.3)
    assert calls(p, "usage_stats") == [{"days": 7}], calls(p, "usage_stats")
    tiles = p.js("[...document.querySelectorAll('#dash-tiles .tile')].map(t => t.textContent.trim())")
    assert tiles[0].startswith("$5,000.00") and "(클로드만)" in tiles[0], f"비용 칸 {tiles[0]!r}"
    assert tiles[1].startswith("1,235"), f"요청 칸 {tiles[1]!r}"
    models = p.js("[...document.querySelectorAll('#dash-models tbody tr')].map(r => [...r.cells].map(c => c.textContent))")
    assert models[0][0] == "Opus 5.5" and models[0][-1] == "$5,000.00", f"모델 표 {models}"
    assert models[1][0] == "gpt-5-codex" and models[1][-1] == "—", f"코덱스 줄 {models[1]}"
    projs = p.js("[...document.querySelectorAll('#dash-projects tbody tr')].map(r => r.cells[0].textContent)")
    assert "oldproj" not in projs, f"7일 밖 줄이 섞였다 {projs}"
    p.js("document.querySelector('.dp[data-days=\"0\"]').click()")
    time.sleep(0.3)
    assert calls(p, "usage_stats")[-1] == {"days": 0}, "전체 기간을 다시 불러오지 않았다"
    assert p.js("document.querySelector('.dp.on').dataset.days") == "0"
    projs = p.js("[...document.querySelectorAll('#dash-projects tbody tr')].map(r => r.cells[0].textContent)")
    assert "oldproj" in projs, f"전체 기간에 옛 줄이 없다 {projs}"
    assert p.js("document.querySelector('#dash-tiles .tile-v').textContent") == "$5,025.00"


@check
def agent_versions_card_updates_what_the_app_can(p):
    """앱이 올릴 수 있는 것(클로드, npm 전역 설치)은 업데이트 버튼, 아니면 명령 복사.
    없는 것·확인 실패는 글로. 버튼은 그 에이전트 이름으로 update_agent를 부른다."""
    agents = [
        {"name": "Claude Code", "installed": "2.1.0", "latest": "2.2.0", "update_available": True,
         "can_update": True, "channel": "latest", "update_cmd": "claude update"},
        {"name": "Codex", "installed": "0.9.0", "latest": "1.0.0", "update_available": True,
         "can_update": True, "channel": "latest", "update_cmd": "npm i -g @openai/codex@latest"},
        {"name": "Gemini", "installed": "0.5.0", "latest": "0.6.0", "update_available": True,
         "can_update": False, "channel": "latest", "update_cmd": "npm i -g @google/gemini-cli@latest"},
        {"name": "Other", "installed": None, "latest": "1.0.0", "update_available": False,
         "can_update": False, "channel": "latest", "update_cmd": ""},
        {"name": "Fresh", "installed": "3.0.0", "latest": "3.0.0", "update_available": False,
         "can_update": False, "channel": "latest", "update_cmd": ""},
    ]
    p.load(replies={"agent_versions": agents, "update_agent": "updated"})
    p.js("document.querySelector('#btn-settings').click()")
    time.sleep(0.3)
    got = p.js("""[...document.querySelectorAll('#agent-versions .agent-row')].map(r => [
        r.querySelector('.agent-ver').textContent, r.querySelector('.agent-state').textContent,
        [...r.querySelectorAll('button')].map(b => b.textContent + '|' + b.title)])""")
    assert got == [
        ["2.1.0", "2.2.0 있음", ["변경 내용|", "업데이트|"]],
        ["0.9.0", "1.0.0 있음", ["변경 내용|", "업데이트|"]],
        ["0.5.0", "0.6.0 있음", ["변경 내용|", "명령 복사|npm i -g @google/gemini-cli@latest"]],
        ["—", "설치 안 됨", []],
        ["3.0.0", "최신", ["최근 변경|"]],
    ], f"버전 카드 {got}"
    p.js("window.__calls = []; [...document.querySelectorAll('#agent-versions .agent-row')[1].querySelectorAll('button')].find(b => b.textContent === '업데이트').click()")
    time.sleep(0.3)
    calls = p.js("window.__calls.map(c => [c[0], c[1] && c[1].name])")
    assert calls[:1] == [["update_agent", "Codex"]], f"업데이트 뒤 {calls}"
    assert "agent_versions" in [c[0] for c in calls], "올린 뒤 다시 확인하지 않았다"
    toast = p.js("[...document.querySelectorAll('.toast')].map(t => t.textContent)")
    assert any("Codex 업데이트" in t and "updated" in t for t in toast), f"알림 {toast}"


@check
def mark_shortcut_warns_when_trace_is_off(p):
    """Ctrl+Shift+M은 진단 기록이 꺼져 있으면 알려만 주고, 켜져 있으면 한 번만 찍는다"""
    marks = "window.__calls.filter(c => c[0] === 'trace_ui' && c[1].kind === 'mark').length"
    toasts = "[...document.querySelectorAll('.toast-title')].map(t => t.textContent)"
    p.load()
    p.term()
    p.key("M", "KeyM", 77, modifiers=10)
    time.sleep(0.2)
    assert p.js(marks) == 0, "꺼져 있는데 기록을 보냈다"
    assert p.js(toasts) == ["진단 기록이 꺼져 있습니다"], p.js(toasts)
    p.load(trace=True)
    p.term()
    p.key("M", "KeyM", 77, modifiers=10)
    time.sleep(0.2)
    assert p.js(marks) == 1, f"기록 {p.js(marks)}번"
    assert p.js(toasts) == ["지금 화면을 기록했습니다"], p.js(toasts)
    assert p.js("window.__sent") == [], f"단축키가 터미널로 샜다 {p.js('window.__sent')}"


@check
def ctrl_wheel_changes_font_size(p):
    """Ctrl+휠은 글자 크기를 바꾸고 기억한다. Ctrl 없는 휠은 크기를 건드리지 않는다."""
    p.load()
    p.term()
    r = p.rect(".term-container.visible .xterm")
    x, y = r["x"] + 100, r["y"] + 100
    ctrl = lambda down: p.cdp("Input.dispatchKeyEvent", type="rawKeyDown" if down else "keyUp", key="Control",
                              code="ControlLeft", windowsVirtualKeyCode=17, modifiers=2 if down else 0)
    wheel = lambda dy, mods=2: p.cdp("Input.dispatchMouseEvent", type="mouseWheel", x=x, y=y, deltaX=0, deltaY=dy, modifiers=mods)
    ctrl(True)
    p.js("window.__calls = []")
    for dy in (-100, -100, 100):
        wheel(dy)
        time.sleep(0.05)
    ctrl(False)
    assert p.js("fontSize") == 14.5, f"13.5 +1 +1 -1 → {p.js('fontSize')}"
    assert store(p, "fontSize") == "14.5" and p.js("terms.get('t').term.options.fontSize") == 14.5
    assert calls(p, "resize_pty"), "크기를 바꾸고 PTY 크기를 다시 알리지 않았다"
    for _ in range(3):
        wheel(-100, 0)
        time.sleep(0.05)
    assert p.js("fontSize") == 14.5, "Ctrl 없는 휠이 크기를 바꿨다"
    p.js("fontSize = 21.5")
    ctrl(True)
    for _ in range(3):
        wheel(-100)
        time.sleep(0.05)
    ctrl(False)
    assert p.js("fontSize") == 22, f"상한 22를 넘었다: {p.js('fontSize')}"


@check
def beta_switch_picks_the_channel_and_drops_a_stale_offer(p):
    """베타 스위치가 확인 채널을 바꾸고, 끄면 베타에서 찾은 설치 버튼을 거둔다"""
    p.load()
    p.js("window.__updateFor = (beta) => beta ? '0.9.0-beta.1' : null; 'ok'")
    p.js("document.querySelector('#btn-settings').click(); document.querySelector('#opt-beta').checked = true; "
         "document.querySelector('#lmodal-save').click()")
    time.sleep(0.3)
    assert p.js("localStorage.getItem('betaUpdates')") == "1", "켠 것이 저장되지 않았다"
    assert p.js("window.__calls.filter(c => c[0] === 'check_update').map(c => c[1].beta)")[-1:] == [True], \
        p.js("window.__calls.filter(c => c[0] === 'check_update')")
    btn = "(() => { const b = document.querySelector('#btn-update'); return b.classList.contains('hidden') ? null : b.textContent })()"
    assert "0.9.0-beta.1" in (p.js(btn) or ""), f"버튼 {p.js(btn)!r}"
    p.js("document.querySelector('#btn-settings').click(); document.querySelector('#opt-beta').checked = false; "
         "document.querySelector('#lmodal-save').click()")
    time.sleep(0.3)
    assert p.js(btn) is None, f"베타를 껐는데 버튼이 남았다: {p.js(btn)!r}"
    assert p.js("window.__calls.filter(c => c[0] === 'check_update').map(c => c[1].beta)")[-1:] == [False]


@check
def update_check_falls_back_to_the_old_path(p):
    """새 Rust 확인 경로가 실패해도 정식 채널은 예전 JS 경로로 찾아 업데이트가 끊기지 않는다"""
    p.load()
    p.js("window.__updateFails = true; window.__TAURI__.updater = { check: () => Promise.resolve({ version: '9.9.9', "
         "downloadAndInstall: () => Promise.resolve() }) }; 'ok'")
    found = p.js("checkUpdate(false)")
    assert found == "9.9.9", f"예전 경로로 못 찾았다: {found!r}"
    btn = p.js("(() => { const b = document.querySelector('#btn-update'); return b.classList.contains('hidden') ? null : b.textContent })()")
    assert btn and "9.9.9" in btn, f"버튼 {btn!r}"
    # 베타 채널은 예전 경로로 볼 수 없다 — 대신 정식판을 내밀면 안 된다
    p.js("resetUpdateOffer()")
    assert p.js("checkUpdate(true)") is None, "베타 확인 실패에 정식 경로 결과를 내밀었다"


@check
def rename_starts_from_the_shown_name_and_keeps_it_untouched(p):
    """이름 바꾸기가 클로드가 붙인 이름(title)을 건너뛰어 빈 칸으로 열리고, 그냥 나가면 덮어쓰던 버그"""
    sess = fake_sessions(1, {0: {"title": "보스전 리팩터링", "summary": None, "first_prompt": "첫 질문"}})
    p.load(sessions=sess)
    sid = sess[0]["session_id"]
    p.js(f"startRename(sessions[0], document.querySelector('.session-item'))")
    assert p.js("document.querySelector('.si-rename').value") == "보스전 리팩터링", p.js("document.querySelector('.si-rename').value")
    p.js("document.querySelector('.si-rename').blur()")
    time.sleep(0.2)
    assert p.js(f"JSON.parse(localStorage.getItem('aliases') || '{{}}')[{json.dumps(sid)}]") is None, "그대로 나갔는데 별칭이 생겼다"


@check
def agent_changelog_opens_newest_first_and_escapes(p):
    """변경 내용: 설치판~최신판을 버전별로 접어 보이고 최신만 펼친다. 마크다운은 이스케이프한다."""
    agents = [{"name": "Codex", "installed": "0.9.0", "latest": "1.1.0", "update_available": True,
               "can_update": True, "channel": "latest", "update_cmd": "npm i -g @openai/codex@latest"}]
    notes = [{"version": "1.1.0", "notes": "- **새 기능** <img src=x onerror=alert(1)>"},
             {"version": "1.0.0", "notes": "- 고친 것"}]
    p.load(replies={"agent_versions": agents, "agent_changelog": notes})
    p.js("document.querySelector('#btn-settings').click()")
    time.sleep(0.3)
    p.js("[...document.querySelectorAll('#agent-versions button')].find(b => b.textContent === '변경 내용').click()")
    time.sleep(0.3)
    call = p.js("window.__calls.find(c => c[0] === 'agent_changelog')[1]")
    assert call == {"name": "Codex", "from": "0.9.0", "to": "1.1.0"}, call
    got = p.js("[...document.querySelectorAll('.agent-changes details')].map(d => [d.querySelector('summary').textContent, d.open])")
    assert got == [["v1.1.0", True], ["v1.0.0", False]], got
    assert not p.js("!!document.querySelector('.agent-changes img')"), "마크다운 속 태그가 그대로 들어갔다"
    assert "새 기능" in p.js("document.querySelector('.agent-changes strong')?.textContent || ''"), "굵게가 안 그려졌다"
    p.js("[...document.querySelectorAll('#agent-versions button')].find(b => b.textContent === '접기').click()")
    assert not p.js("!!document.querySelector('.agent-changes')"), "접기가 안 된다"


@check
def up_to_date_agent_shows_recent_changes(p):
    """최신판이어도 지금 판까지 무엇이 바뀌었는지 본다 — 설치판까지, 최근 5개"""
    agents = [{"name": "Claude Code", "installed": "2.1.281", "latest": "2.1.281", "update_available": False,
               "can_update": True, "channel": "latest", "update_cmd": "claude update"}]
    notes = [{"version": f"2.1.{281 - i}", "notes": f"- 변경 {i}"} for i in range(8)]
    p.load(replies={"agent_versions": agents, "agent_changelog": notes})
    p.js("document.querySelector('#btn-settings').click()")
    time.sleep(0.3)
    p.js("[...document.querySelectorAll('#agent-versions button')].find(b => b.textContent === '최근 변경').click()")
    time.sleep(0.3)
    call = p.js("window.__calls.find(c => c[0] === 'agent_changelog')[1]")
    assert call == {"name": "Claude Code", "from": "", "to": "2.1.281"}, call
    assert p.js("document.querySelectorAll('.agent-changes details').length") == 5, "최근 5개가 아니다"


@check
def screens_render_without_errors(p):
    """대시보드·토큰 상세·설정·한도·토스트·탭 게이지 — 다른 검사가 안 여는 화면들 (CSP 위반도 여기서 걸린다)"""
    p.load(sessions=fake_sessions(2, {0: {"ctx_tokens": 700000, "model": "claude-opus-5-5"}}))
    p.term(fake_sessions(1)[0]["session_id"])
    r = p.js("""(async () => {
      syncCtxGauges(); renderTabs();
      dashRows = [{date: new Date().toISOString().slice(0, 10), model: 'claude-opus-5-5', project: 'p', cwd: 'D:/p',
                   input: 10, output: 20, cache_read: 30, cache_5m: 1, cache_1h: 2, requests: 3, agent: 'claude'}];
      document.querySelector('#dash-backdrop').classList.remove('hidden'); renderDash();
      turnsData = [0, 1, 2].map(i => ({ts: 1.7e9 + i * 60, model: 'claude-opus-5-5', input: 5, output: 9, cache_read: i ? 9000 : 0,
                   cache_5m: 0, cache_1h: i ? 0 : 9000, prompt_idx: 0, prompt: '**질문** `x`', text: '답 | a | b |', tools: ['Read a']}));
      turnsCanPrice = true; turnsPriced = true;
      document.querySelector('#turns-backdrop').classList.remove('hidden'); renderTurns(sessions[0]);
      selectedPrompt = 0; renderPrompts(turnsData.map(turnValue));
      document.querySelector('#foot-limits-rows').innerHTML = limitRow('5시간', {utilization_pct: 42, resets_at: new Date(Date.now() + 3.6e6).toISOString()});
      document.querySelector('#btn-settings').click();
      showToast('제목', '본문');
      previewCard.innerHTML = mdToHtml('# h\\n- a\\n```\\ncode\\n```\\n> q');
      await new Promise(r => setTimeout(r, 300));
      return [document.querySelectorAll('#dash-models tr').length, !!document.querySelector('#turns-detail .td-q'),
              !!document.querySelector('.tab-ctx'), !!document.querySelector('.limit-fill'), document.querySelectorAll('.lrow').length > 0];
    })()""")
    assert r == [2, True, True, True, True], r


@check
def csp_blocks_injected_script(p):
    """정책이 실제로 걸렸는지 — 주입한 스크립트가 도는지 본다 (--csp일 때만)"""
    if not CSP:
        return "skip: --csp 없이는 볼 게 없음"
    p.load(sessions=fake_sessions(1))
    p.js("""(() => {
      const box = document.createElement('div'); document.body.appendChild(box);
      box.innerHTML = '<img src="nope.png" onerror="window.__pwned=1"><svg onload="window.__pwned=2"></svg>';
      const s = document.createElement('script'); s.textContent = 'window.__pwned=3'; document.body.appendChild(s);
      const d = document.createElement('script'); d.src = 'data:text/javascript,window.__pwned=4'; document.body.appendChild(d);
      const a = document.createElement('a'); a.href = 'javascript:window.__pwned=5'; document.body.appendChild(a); a.click();
      // Runtime.evaluate 안에서는 DevTools가 eval을 풀어 준다 — 페이지의 다음 작업에서 부른다
      setTimeout(() => { try { new Function('window.__pwned=6')(); } catch (e) { window.__evalErr = e.name; } }, 0);
      try { setTimeout('window.__pwned=7', 0); } catch {}
      const b = document.createElement('base'); b.href = 'https://example.com/'; document.head.appendChild(b);
      return 'ok'; })()""")
    time.sleep(0.8)
    pwned = p.js("window.__pwned === undefined ? null : window.__pwned")
    got = p.js("window.__csp.splice(0)")   # 기대한 위반이므로 전체 집계에서 뺀다
    assert pwned is None, f"주입한 스크립트가 돌았다 (__pwned={pwned}); 위반 {got}"
    assert p.js("window.__evalErr") == "EvalError", f"new Function이 막히지 않았다: {p.js('window.__evalErr')}"
    dirs = {v.split(" | ")[0] for v in got}
    for want in ("script-src-attr", "script-src-elem", "script-src", "base-uri"):
        assert want in dirs, f"{want} 위반이 기록되지 않았다: {got}"
    # 앱의 모든 스크립트는 여전히 돌고 있어야 한다
    alive = p.js("[typeof Terminal, typeof renderSidebar, typeof openDash]")
    assert alive == ["function"] * 3, f"앱 스크립트가 죽었다: {alive}"
    rows = p.js("document.querySelectorAll('.session-item').length")
    assert rows == 1, f"사이드바 줄 {rows}"


@check
def changelog_translates_on_demand_and_toggles_back(p):
    """변경 내역 번역: 누른 판만 번역을 부르고, 번역도 이스케이프해 그리며, 원문으로 되돌린다"""
    agents = [{"name": "Codex", "installed": "0.9.0", "latest": "1.0.0", "update_available": True,
               "can_update": True, "channel": "latest", "update_cmd": ""}]
    notes = [{"version": "1.0.0", "notes": "- **New** thing"}, {"version": "0.9.5", "notes": "- old"}]
    p.load(replies={"agent_versions": agents, "agent_changelog": notes,
                    "translate_changelog": "- **새** 기능 <img src=x onerror=window.__pwned=1>"})
    p.js("document.querySelector('#btn-settings').click()")
    time.sleep(0.3)
    p.js("[...document.querySelectorAll('#agent-versions button')].find(b => b.textContent === '변경 내용').click()")
    time.sleep(0.3)
    p.js("document.querySelector('.agent-changes details .agent-translate').click()")
    time.sleep(0.3)
    calls = p.js("window.__calls.filter(c => c[0] === 'translate_changelog').map(c => c[1])")
    assert calls == [{"name": "Codex", "version": "1.0.0", "text": "- **New** thing"}], calls
    body = "document.querySelector('.agent-changes details .agent-changes-body')"
    assert "새" in p.js(f"{body}.textContent"), p.js(f"{body}.textContent")
    assert not p.js(f"!!{body}.querySelector('img')"), "번역 속 태그가 그대로 들어갔다"
    p.js("document.querySelector('.agent-changes details .agent-translate').click()")
    assert "New" in p.js(f"{body}.textContent"), "원문으로 안 돌아갔다"
    p.js("document.querySelector('.agent-changes details .agent-translate').click()")
    time.sleep(0.2)
    assert len(p.js("window.__calls.filter(c => c[0] === 'translate_changelog')")) == 1, "다시 볼 때 또 번역을 불렀다"


# ---------------------------------------------------------------- 실행

def main():
    # CI처럼 출력이 파이프면 cp1252로 열려 한글을 찍다가 죽는다
    for s in (sys.stdout, sys.stderr):
        try:
            s.reconfigure(encoding="utf-8")
        except Exception:
            pass
    global CSP, PAGE
    want = [a for a in sys.argv[1:] if a != "--csp"]
    srv = None
    if "--csp" in sys.argv[1:]:
        CSP = conf_csp()
        srv = serve_ui(CSP)
        PAGE = f"http://127.0.0.1:{HTTP_PORT}/index.html"
        print(f"CSP: {CSP}\n")
    chosen = [c for c in CHECKS if not want or any(w in c.__name__ for w in want)]
    chrome_exe = find_chrome()
    print(f"크롬: {chrome_exe}")
    profile = tempfile.mkdtemp(prefix="deck-ui-checks-")
    chrome = subprocess.Popen([
        chrome_exe, "--headless=new", f"--remote-debugging-port={PORT}", f"--user-data-dir={profile}",
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
        if target is None:
            raise RuntimeError(f"크롬 디버깅 포트({PORT})에 붙지 못했다")
        ws = websocket.create_connection(target["webSocketDebuggerUrl"], timeout=60)
        p = Page(ws)
        p.cdp("Page.enable")
        p.cdp("Runtime.enable")
        # 헤드리스는 창에 포커스가 없다고 보고 focus 이벤트를 안 보낸다. 실제 앱은 사용자가
        # 치는 동안 창에 포커스가 있으니 그렇게 흉내 낸다.
        p.cdp("Emulation.setFocusEmulationEnabled", enabled=True)
        for c in chosen:
            try:
                before = len(p.violations)
                note = c(p)
                p.harvest()
                new = p.violations[before:]
                assert not new, f"CSP 위반 {len(new)}건: " + "; ".join(sorted(set(new)))[:600]
                print(f"통과  {c.__name__}" + (f"  ({note})" if note else ""))
            except Exception as e:
                failed += 1
                p.harvest()   # 이 검사의 위반이 다음 검사로 넘어가지 않게
                print(f"실패  {c.__name__}: {e}")
                if not isinstance(e, AssertionError):
                    traceback.print_exc()
        ws.close()
    finally:
        if srv:
            srv.shutdown()
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
