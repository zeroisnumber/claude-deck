"""빌드한 실제 앱을 띄워서 보는 검사 — ui-checks.py(가짜 Tauri 위의 화면)와 cargo test(Rust 단위)가
못 보는 것: 진짜 실행 파일 + 진짜 WebView2 + 진짜 ConPTY가 이어진 상태.

    npx tauri build --no-bundle          # 실행 파일만 (설치 파일은 안 만든다)
    python tools/app-smoke.py            # 앱에 든 ConPTY로
    python tools/app-smoke.py --inbox    # 윈도우 내장 ConPTY로 (비교용)
    python tools/app-smoke.py --damaged  # 앱에 든 ConPTY 파일이 0으로 채워진 설치본 (실제로 겪은 손상)
    python tools/app-smoke.py --exe <경로> --keys 100 --frame-ms 33 --frame-rows 40

앱은 격리해서 띄운다(CLI_DECK_SANDBOX, src-tauri/src/paths.rs): 세션 목록·설정·진단 기록·창 위치를
임시 폴더에서 읽고 쓰므로 쓰고 있는 앱과 그 탭을 건드리지 않는다. 창은 뒤에 뜨고 포커스를 가져가지
않는다. 탭에는 에이전트 대신 tools/fake-agent.js를 띄운다(요청이 나가지 않는다).

보는 것:
  - 격리가 지켜지는가 (지난 탭 목록이 비어 있고 세션이 0개) — 아니면 아무것도 하지 않고 끈다
  - 어느 ConPTY가 붙었는가 (앱이 말하는 것과 실제 자식 프로세스가 같은가), 설치 손상 표시가 없는가
  - 탭이 열리고 에이전트 화면이 터미널에 그려지는가
  - 키 하나가 화면에 보이기까지 (keydown → xterm이 받아 그릴 때까지, 앱 안에서 잰다)
  - 탭을 닫으면 그 프로세스들이 사라지는가
필요한 것: python `websocket-client`, node.
"""
import argparse, json, os, shutil, subprocess, sys, tempfile, time, urllib.request

import websocket

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")


def free_port():
    import socket
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


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

    def wait(self, expr, timeout=15, what=""):
        end = time.time() + timeout
        while time.time() < end:
            if self.js(expr):
                return
            time.sleep(0.1)
        raise AssertionError(f"{timeout}초 안에 되지 않았다: {what or expr}")


def descendants(pid):
    """pid 아래의 프로세스들 [(pid, 이름, 실행 파일 경로)]"""
    ps = ("Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId, Name, ExecutablePath "
          "| ConvertTo-Json -Compress")
    out = subprocess.run(["powershell.exe", "-NoProfile", "-Command", ps], capture_output=True, timeout=60).stdout
    rows = json.loads(out.decode("utf-8", "replace"))
    kids = {}
    for r in rows:
        kids.setdefault(r["ParentProcessId"], []).append(r)
    found, stack = [], [pid]
    while stack:
        for r in kids.get(stack.pop(), []):
            found.append((r["ProcessId"], r["Name"], r.get("ExecutablePath") or ""))
            stack.append(r["ProcessId"])
    return found


def pct(v, p):
    v = sorted(v)
    return v[min(len(v) - 1, int(len(v) * p))] if v else float("nan")


def kill_with_me(proc):
    """이 스크립트가 어떻게 끝나든(강제 종료 포함) 띄운 앱과 그 아래 프로세스가 같이 끝나게 한다.
    잡 오브젝트에 넣고 핸들을 쥐고 있으면, 이 프로세스가 사라져 핸들이 닫힐 때 윈도우가 정리한다.
    안 하면 중간에 죽은 검사가 뒤에 숨은 앱과 트레이 아이콘을 남긴다."""
    import ctypes
    from ctypes import wintypes
    k32 = ctypes.WinDLL("kernel32", use_last_error=True)

    class BASIC(ctypes.Structure):
        _fields_ = [("PerProcessUserTimeLimit", ctypes.c_int64), ("PerJobUserTimeLimit", ctypes.c_int64),
                    ("LimitFlags", wintypes.DWORD), ("MinimumWorkingSetSize", ctypes.c_size_t),
                    ("MaximumWorkingSetSize", ctypes.c_size_t), ("ActiveProcessLimit", wintypes.DWORD),
                    ("Affinity", ctypes.c_size_t), ("PriorityClass", wintypes.DWORD), ("SchedulingClass", wintypes.DWORD)]

    class IO(ctypes.Structure):
        _fields_ = [(n, ctypes.c_uint64) for n in ("ReadOps", "WriteOps", "OtherOps", "ReadBytes", "WriteBytes", "OtherBytes")]

    class EXTENDED(ctypes.Structure):
        _fields_ = [("Basic", BASIC), ("Io", IO), ("ProcessMemoryLimit", ctypes.c_size_t),
                    ("JobMemoryLimit", ctypes.c_size_t), ("PeakProcessMemoryUsed", ctypes.c_size_t),
                    ("PeakJobMemoryUsed", ctypes.c_size_t)]

    k32.CreateJobObjectW.restype = wintypes.HANDLE
    k32.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
    k32.SetInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
    k32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    job = k32.CreateJobObjectW(None, None)
    info = EXTENDED()
    info.Basic.LimitFlags = 0x2000   # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    ok = job and k32.SetInformationJobObject(job, 9, ctypes.byref(info), ctypes.sizeof(info)) \
        and k32.AssignProcessToJobObject(job, int(proc._handle))
    return job if ok else None   # 돌려준 핸들을 쥐고 있어야 한다 (놓으면 그 자리에서 끝난다)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--exe", default=os.path.join(ROOT, "src-tauri", "target", "release", "claude-deck.exe"))
    ap.add_argument("--inbox", action="store_true", help="윈도우 내장 ConPTY로 (설정에서 끈 상태)")
    ap.add_argument("--damaged", action="store_true",
                    help="설치 직후 PC가 꺼진 설치본처럼: 실행 파일을 복사해 두고 conpty.dll을 크기만 맞는 0으로 채운다")
    ap.add_argument("--keys", type=int, default=60)
    ap.add_argument("--frame-ms", type=int, default=100, help="가짜 에이전트가 화면을 다시 그리는 간격")
    ap.add_argument("--frame-rows", type=int, default=6)
    ap.add_argument("--keep", action="store_true", help="끝나도 격리 폴더를 남긴다")
    args = ap.parse_args()

    exe = os.path.abspath(args.exe)
    if not os.path.isfile(exe):
        sys.exit(f"실행 파일이 없다: {exe}\n먼저 `npx tauri build --no-bundle`")
    sandbox = tempfile.mkdtemp(prefix="deck-sandbox-")
    home = os.path.join(sandbox, "home")
    os.makedirs(home)
    os.makedirs(os.path.join(sandbox, "data"))
    if args.inbox:
        open(os.path.join(sandbox, "data", "INBOX_CONPTY"), "w").close()
    if args.damaged:
        inst = os.path.join(sandbox, "install")
        shutil.copytree(os.path.join(os.path.dirname(exe), "conpty"), os.path.join(inst, "conpty"))
        shutil.copy2(exe, inst)
        dll = os.path.join(inst, "conpty", "conpty.dll")
        size = os.path.getsize(dll)
        with open(dll, "wb") as f:
            f.write(bytes(size))
        exe = os.path.join(inst, "claude-deck.exe")
    open(os.path.join(sandbox, "data", "TRACE"), "w").close()   # 진단 기록을 켠 채로 (계측 경로까지 같이 돈다)
    port = free_port()
    env = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE") and k != "AI_AGENT"}
    env["CLI_DECK_SANDBOX"] = sandbox
    env["WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS"] = f"--remote-debugging-port={port} --remote-allow-origins=*"
    env["FAKE_FRAME_MS"] = str(args.frame_ms)
    env["FAKE_FRAME_ROWS"] = str(args.frame_rows)
    print(f"실행 파일: {exe}\n격리 폴더: {sandbox}")
    app = subprocess.Popen([exe], env=env, cwd=sandbox)
    job = kill_with_me(app)   # noqa: F841 — 끝날 때까지 쥐고 있는다
    failed = []

    def check(name, ok, detail=""):
        print(f"{'통과' if ok else '실패'}  {name}" + (f"  ({detail})" if detail else ""))
        if not ok:
            failed.append(name)

    try:
        target = None
        for _ in range(100):
            if app.poll() is not None:
                raise RuntimeError(f"앱이 바로 끝났다 (exit {app.returncode})")
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/json", timeout=2) as r:
                    pages = [x for x in json.load(r) if x["type"] == "page" and "tauri.localhost" in x.get("url", "")]
                if pages:
                    target = pages[0]
                    break
            except Exception:
                pass
            time.sleep(0.2)
        if target is None:
            raise RuntimeError("앱 화면에 붙지 못했다 (WebView2 원격 디버깅 포트)")
        p = Page(websocket.create_connection(target["webSocketDebuggerUrl"], timeout=60))
        p.cdp("Runtime.enable")
        p.wait("document.readyState === 'complete' && typeof openNewSession === 'function' && typeof priceFor === 'function'",
               what="앱 스크립트 로드")

        # 1) 격리 — 여기서 어긋나면 더 진행하지 않는다 (쓰던 앱의 탭을 건드릴 수 있다)
        iso = json.loads(p.js("JSON.stringify({tabs: localStorage.getItem('openTabs'), sandbox: !!window.__DECK_SANDBOX__, "
                              "aliases: localStorage.getItem('aliases'), pins: localStorage.getItem('pins')})"))
        if iso["tabs"] is not None or iso["aliases"] is not None or iso["pins"] is not None or not iso["sandbox"]:
            raise RuntimeError(f"격리되지 않았다 — 쓰던 앱의 프로필을 읽고 있다: {iso}")
        p.wait("restored === true", what="첫 세션 목록")
        n_sessions = p.js("sessions.length")
        if n_sessions:
            raise RuntimeError(f"격리되지 않았다 — 세션 {n_sessions}개가 보인다")
        check("격리 실행 (지난 탭 없음, 세션 0개)", True)

        # 2) 앱 상태
        st = json.loads(p.js("invoke('app_status').then(JSON.stringify)"))
        want_bundled = not args.inbox and not args.damaged
        c = st["conpty"]
        said = c["path"] or f"{c['reason']} {c['detail']}".strip()
        toasts = p.js("[...document.querySelectorAll('#toasts .toast-title')].map(t => t.textContent)")
        if args.damaged:
            check("ConPTY: 망가진 파일을 불러오지 않고 내장으로 물러남", not c["bundled"] and c["reason"] == "damaged", said)
            check("설치 손상을 화면에 알림", bool(st["damaged"]) and "설치 파일이 손상되었습니다" in toasts,
                  f"{st['damaged']} / 알림 {toasts}")
        else:
            check("ConPTY: " + ("앱에 든 것" if want_bundled else "윈도우 내장(꺼 둠)"),
                  c["bundled"] == want_bundled and (want_bundled or c["reason"] == "off"), said)
            check("설치 손상 표시 없음", st["damaged"] == [] and not toasts, f"{st['damaged']} / 알림 {toasts}")
        check("격리 표시", st["sandbox"] is True)

        # 3) 탭을 열고 가짜 에이전트를 띄운다
        agent = os.path.join(ROOT, "tools", "fake-agent.js")
        prof = {"name": "fake", "cmd": f'node "{agent}"', "resume": False}
        p.js(f"detach('smoke', openNewSession({json.dumps(home)}, {json.dumps(prof)})); 'ok'")
        buf = "(() => { const t = terms.get(activeId); if (!t) return ''; const b = t.term.buffer.active; " \
              "let s = ''; for (let y = 0; y < 8; y++) { const l = b.getLine(y); s += (l ? l.translateToString(true) : '') + '\\n'; } return s; })()"
        t0 = time.time()
        try:
            p.wait(buf + ".includes('READY fake-agent')", timeout=20, what="에이전트 첫 화면")
        except AssertionError:
            print("      터미널에 보이는 것:\n        " + "\n        ".join((p.js(buf) or "").rstrip().splitlines() or ["(빈 화면)"]))
            print("      " + str(p.js("JSON.stringify((() => { const t = terms.get(activeId); return t && "
                                    "{exited: t.exited, gen: t.gen, cols: t.term.cols, rows: t.term.rows, hidden: document.hidden}; })())")))
            raise
        check("탭이 열리고 에이전트 화면이 그려짐", True, f"{time.time() - t0:.1f}초")
        if args.frame_ms > 0:
            p.wait(buf + ".includes('frame ')", timeout=10, what="다시 그리는 화면")

        # 4) 실제로 뜬 프로세스
        procs = descendants(app.pid)
        names = sorted({n.lower() for _, n, _ in procs})
        host = [path for _, n, path in procs if n.lower() == "openconsole.exe"]
        inbox_host = [n for _, n, _ in procs if n.lower() == "conhost.exe"]
        if want_bundled:
            inside = os.path.join(os.path.dirname(exe), "conpty").lower()
            check("OpenConsole.exe가 앱 폴더의 것으로 떠 있음", bool(host) and all(h.lower().startswith(inside) for h in host),
                  ", ".join(host) or f"자식: {names}")
        else:
            check("내장 conhost.exe가 떠 있고 OpenConsole은 없음", bool(inbox_host) and not host, f"자식: {names}")
            # 앱에 든 것을 안 쓸 때 PATH에 있는 다른 프로그램의 conpty.dll이 걸려 오면 앱이 그렇게 말해야 한다
            stray = json.loads(p.js("invoke('app_status').then(JSON.stringify)"))["conpty_stray"]
            print(f"      다른 프로그램의 conpty.dll: {stray or '없음'}")

        # 5) 키 → 화면. 앱 안에서 잰다: keydown을 받은 순간부터, 에이전트가 답한 숫자를 xterm이
        #    받아 적은 순간(parsed)과 그 뒤 처음 그린 순간(painted)까지.
        p.cdp("Emulation.setFocusEmulationEnabled", enabled=True)
        p.js("""(() => {
          const t = terms.get(activeId).term;
          t.focus();
          const m = window.__smoke = { t0: 0, seen: 0, parsed: [], painted: [], waitPaint: false, frames: 0 };
          t.textarea.addEventListener('keydown', () => { m.t0 = performance.now(); }, true);
          const read = () => { const l = t.buffer.active.getLine(1); const k = /^K(\\d+)/.exec(l ? l.translateToString(true) : ''); return k ? +k[1] : 0; };
          t.onWriteParsed(() => { const k = read(); if (k > m.seen) { m.seen = k; m.parsed.push(performance.now() - m.t0); m.waitPaint = true; } });
          t.onRender(() => { m.frames++; if (m.waitPaint) { m.waitPaint = false; m.painted.push(performance.now() - m.t0); } });
          return 'ok';
        })()""")
        for i in range(args.keys):
            want = i + 1
            p.cdp("Input.dispatchKeyEvent", type="keyDown", key="a", code="KeyA", windowsVirtualKeyCode=65, text="a")
            p.cdp("Input.dispatchKeyEvent", type="keyUp", key="a", code="KeyA", windowsVirtualKeyCode=65)
            end = time.time() + 5
            while time.time() < end and p.js("window.__smoke.seen") < want:
                time.sleep(0.01)
            time.sleep(0.08)
        m = json.loads(p.js("JSON.stringify(window.__smoke)"))
        check(f"키 {args.keys}개가 모두 에이전트에 닿고 화면에 돌아옴", m["seen"] == args.keys, f"{m['seen']}/{args.keys}")
        if m["parsed"]:
            print(f"      키 → xterm이 받음   p50 {pct(m['parsed'], .5):.1f}ms  p90 {pct(m['parsed'], .9):.1f}ms  최대 {max(m['parsed']):.1f}ms")
        if m["painted"]:
            print(f"      키 → 화면에 그림    p50 {pct(m['painted'], .5):.1f}ms  p90 {pct(m['painted'], .9):.1f}ms  최대 {max(m['painted']):.1f}ms")
        else:
            print("      (창이 가려져 그리기가 멈춰 있어 그린 시각은 재지 못했다)")
        renderer = p.js("terms.get(activeId).webgl ? 'gl' : 'dom'")
        print(f"      렌더러 {renderer}, 화면 다시 그리기 {args.frame_ms}ms × {args.frame_rows}줄")

        # 5b) 크기를 바꾼 뒤에도 화면이 에이전트가 그린 그대로인가. 둘째 탭의 에이전트는 줄마다 번호와
        #     그때의 칸 수를 적고 배경색을 끝 칸까지 채운다(클로드의 풀스크린 화면처럼). 크기가 바뀌면
        #     통째로 다시 그리므로, 조금 기다린 뒤 터미널의 줄 i가 "R<i> c<지금 칸 수>…"가 아니면
        #     줄이 밀렸거나 옛 줄이 남은 것이다.
        prof2 = {"name": "rows", "cmd": f'set FAKE_MODE=rows&&node "{agent}"', "resume": False}
        first_tab = p.js("activeId")
        p.js(f"detach('smoke2', openNewSession({json.dumps(home)}, {json.dumps(prof2)})); 'ok'")
        p.wait(f"activeId !== {json.dumps(first_tab)}", timeout=10, what="둘째 탭")
        second_tab = p.js("activeId")   # 아래는 이 탭을 id로 본다 (나눈 화면에서는 입력 칸이 바뀔 수 있다)
        rows_js = ("(() => { const t = terms.get(%s).term; const b = t.buffer.active; const out = []; "
                   "for (let y = 0; y < t.rows; y++) { const l = b.getLine(b.baseY + y); out.push(l ? l.translateToString(true) : ''); } "
                   "return JSON.stringify({cols: t.cols, rows: t.rows, lines: out}); })()") % json.dumps(second_tab)

        def settle(what):
            end, v, bad = time.time() + 4, None, None
            while time.time() < end:
                v = json.loads(p.js(rows_js))
                bad = [(i + 1, line[:28]) for i, line in enumerate(v["lines"])
                       if not line.startswith(f"R{i + 1} c{v['cols']}r{v['rows']} ")]
                if not bad:
                    break
                time.sleep(0.25)
            check(f"화면이 에이전트가 그린 그대로: {what}", not bad,
                  f"{v['cols']}×{v['rows']}" + (f", 어긋난 줄 {len(bad)}개 — 예: {bad[:3]}" if bad else ""))

        p.wait(rows_js + ".includes('R1 c')", timeout=20, what="둘째 탭 첫 화면")
        settle("처음")
        for what, js in [("사이드바 접음", "setSidebarCollapsed(true)"), ("사이드바 폄", "setSidebarCollapsed(false)"),
                         ("화면 좌우로 나눔", "setLayout('h2')"), ("네 칸으로 나눔", "setLayout('4')"),
                         ("한 칸으로 되돌림", "setLayout('1')"), ("사이드바 접음 (다시)", "setSidebarCollapsed(true)"),
                         ("사이드바 폄 (다시)", "setSidebarCollapsed(false)")]:
            p.js(js + "; 'ok'")
            time.sleep(0.6)
            settle(what)
        # 창 테두리를 끌 때처럼 프레임마다 크기가 바뀌는 경우 — 크기 알림이 수십 개 잇달아 나간다.
        # 끝났을 때 에이전트가 아는 크기가 터미널의 마지막 크기와 같아야 한다.
        p.js("""new Promise((done) => {
          let i = 0;
          const step = () => {
            i++;
            termArea.style.marginRight = (i < 40 ? (i % 20) * 12 : 0) + 'px';
            if (i < 40) requestAnimationFrame(step); else done('ok');
          };
          requestAnimationFrame(step);
        })""")
        settle("프레임마다 크기를 바꾼 뒤 (창 테두리 끌기)")
        for i in range(12):
            p.js(f"setSidebarCollapsed({'true' if i % 2 == 0 else 'false'}); 'ok'")
            time.sleep(0.03)
        settle("사이드바를 빠르게 여러 번 접었다 편 뒤")
        active_after = p.js("activeId")
        check("크기를 바꾸는 동안 입력 탭이 바뀌지 않음", active_after == second_tab, f"{active_after} (둘째 탭 {second_tab})")
        p.js(f"closeTab({json.dumps(second_tab)}).then(() => 'ok')")
        time.sleep(0.5)

        # 6) 탭을 닫으면 그 프로세스들이 사라진다
        p.js(f"closeTab({json.dumps(first_tab)}).then(() => 'ok')")
        end = time.time() + 6
        left = None
        while time.time() < end:
            left = [n for _, n, _ in descendants(app.pid) if n.lower() in ("openconsole.exe", "conhost.exe", "node.exe", "cmd.exe")]
            if not left:
                break
            time.sleep(0.5)
        check("탭을 닫으면 에이전트와 콘솔 호스트가 끝남", not left, f"남은 것: {left}" if left else "")

        # 7) 앱이 남긴 기록: 화면 스크립트가 던진 것(script-failed)은 진단 기록이 꺼져 있어도 남는다
        time.sleep(0.5)
        log = os.path.join(sandbox, "data", "pty-trace.log")
        lines = open(log, encoding="utf-8", errors="replace").read().splitlines() if os.path.exists(log) else []
        thrown = [l.split("\t", 4)[-1][:200] for l in lines if "\tscript-failed\t" in l]
        check("화면 스크립트 오류 없음", not thrown, " | ".join(thrown)[:400])
        said = [l.split("\t", 4)[-1] for l in lines if "\tconpty\t" in l]
        check("시작할 때 붙은 ConPTY를 기록에 남김", bool(said), said[0] if said else "기록 없음")
    except Exception as e:
        failed.append(str(e))
        print(f"실패  {e}")
    finally:
        # 우리가 띄운 것만 끈다 (그 아래 WebView2·에이전트까지)
        subprocess.run(["taskkill", "/PID", str(app.pid), "/T", "/F"], capture_output=True)
        time.sleep(1.0)
        if args.keep:
            print(f"격리 폴더를 남겼다: {sandbox}")
        else:
            for _ in range(5):
                shutil.rmtree(sandbox, ignore_errors=True)
                if not os.path.exists(sandbox):
                    break
                time.sleep(1.0)
    print("\n" + ("전부 통과" if not failed else f"실패 {len(failed)}건"))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
