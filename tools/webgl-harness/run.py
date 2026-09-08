# 컨텍스트 손실/복구 시나리오를 헤드리스 크롬에서 돌리고 화면이 살아 있는지 판정한다.
import sys, json, asyncio, subprocess, base64, urllib.request, time, os
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, sys.argv[1])
import websockets
browser, mode, out = sys.argv[2], sys.argv[3], sys.argv[4]
prof = os.path.join(HERE, "chrome-prof-ctx")
proc = subprocess.Popen([browser, "--headless=new", "--remote-debugging-port=9334", "--window-size=1000,600",
    "--no-first-run", "--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--user-data-dir=" + prof, "about:blank"],
    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
try:
    for _ in range(50):
        try:
            tabs = json.load(urllib.request.urlopen("http://127.0.0.1:9334/json")); break
        except Exception: time.sleep(0.2)
    page = [t for t in tabs if t["type"] == "page"][0]
    async def run():
        async with websockets.connect(page["webSocketDebuggerUrl"], max_size=50_000_000) as ws:
            mid = 0
            async def call(method, **params):
                nonlocal mid; mid += 1
                await ws.send(json.dumps({"id": mid, "method": method, "params": params}))
                while True:
                    m = json.loads(await ws.recv())
                    if m.get("id") == mid: return m.get("result", m)
            async def ev(expr):
                r = await call("Runtime.evaluate", expression=expr, returnByValue=True)
                return r.get("result", {}).get("value")
            await call("Page.enable"); await call("Runtime.enable")
            await call("Page.navigate", url=f"file:///{HERE.replace(os.sep, '/')}/index.html?mode={mode}")
            await asyncio.sleep(1.5)
            print("before      :", await ev("JSON.stringify(window.__pixels())"))
            # 진짜 GPU 프로세스 크래시 (드라이버 리셋과 같은 경로): 브라우저 타깃의 CDP로 보낸다
            ver = json.load(urllib.request.urlopen("http://127.0.0.1:9334/json/version"))
            async with websockets.connect(ver["webSocketDebuggerUrl"], max_size=50_000_000) as bws:
                await bws.send(json.dumps({"id": 1, "method": "Browser.crashGpuProcess"}))
                print("crashGpu    :", (await bws.recv())[:120])
            await asyncio.sleep(4.0)
            print("after crash :", await ev("JSON.stringify(window.__pixels())"))
            await ev("window.__write('\r\nAFTER RESTORE 12345\r\nPROMPT> ')")
            await asyncio.sleep(0.8)
            print("after write :", await ev("JSON.stringify(window.__pixels())"))
            print("log:", await ev("JSON.stringify(window.__log)"))
            shot = await call("Page.captureScreenshot", format="png")
            open(out, "wb").write(base64.b64decode(shot["data"]))
    asyncio.run(run())
finally:
    proc.kill()
