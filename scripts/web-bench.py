#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The web viewer's bench: the browser counterpart of the native headless runner.

Drives the page served by `scripts/try-viewer.sh web` (or any catalog proxy
serving `examples/web-viewer`) in headless Chrome over the DevTools protocol,
through the viewer's public JS API (`setView`, `flyTo`) and the browser's own
controls (resize, reload), plays the patterns below and checks, per pattern:

  coverage     no pixel of the lower ground band left without a mesh, in the
               viewer's "geometry" diagnostic view (flat lit surface; black =
               no mesh drawn) — a hole whatever colour the normal view gives it
  convergence  the status line says `converged`, within the pattern's bound
               (a page's: its loads cross a proxy and a few decode workers,
               where the native runner's bound is 2 s)
  requests     no request pending longer than the load timeout + 5 s, none
               older than 5 s once converged, no request for one URL started
               while another for it is still pending
  budget       held bytes within the page's hard limit; timeouts and failures
               counted

Writes `summary.json`, the console of the page and its workers, and a
screenshot of every pattern's last frame (and of every failing check) into
`--out`. Exits 1 when an obligation is violated.

    scripts/try-viewer.sh web            # in one terminal (port 8091)
    scripts/web-bench.py --out /tmp/wb   # in another
    scripts/web-bench.py --backend webgl2 --dpr 1 --size 390x844 --patterns still,yaw

Needs `pip install websockets pillow` and Google Chrome.
"""

import argparse
import asyncio
import base64
import collections
import io
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.request

import websockets
from PIL import Image

CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
LOAD_TIMEOUT_S = 30.0

# A place a person picks: the Swiss plateau by Lake Neuchâtel, the Alps.
NEUCH = dict(lon=7.0, lat=46.85, altitude=4000, heading=40, pitch=45)
ALPS = dict(lon=7.65, lat=45.98, altitude=6000, heading=30, pitch=25)

PATTERNS = {
    # name: (description, steps, convergence bound in seconds)
    "cold": ("P1 cold start, then the plateau", [("fly", NEUCH, 0.0)], 20.0),
    "zoom-in": ("P2 zoom in from 200 km to 4 km", [
        ("set", dict(NEUCH, altitude=200000, pitch=60), 0.0),
        ("wait", 6.0),
        ("fly", NEUCH, 4.0)], 20.0),
    "zoom-out-back": ("P3 out to 200 km and back", [
        ("set", NEUCH, 0.0), ("wait", 8.0),
        ("fly", dict(NEUCH, altitude=200000), 3.0), ("wait", 3.0),
        ("fly", NEUCH, 3.0)], 15.0),
    "yaw": ("P4 yaw in place, 180°/s", [("set", ALPS, 0.0), ("wait", 8.0), ("yaw", 180.0, 2.0)], 20.0),
    "tilt": ("P4 tilt toward the horizon", [("set", ALPS, 0.0), ("wait", 8.0), ("tilt", 25, 75, 2.0)], 15.0),
    "resize": ("P5 portrait and landscape", [("set", NEUCH, 0.0), ("wait", 8.0),
                                            ("resize", 390, 844), ("wait", 3.0),
                                            ("resize", 844, 390)], 15.0),
    "still": ("P7 still near the ground, 20 s", [("set", NEUCH, 0.0)], 20.0),
    # A hand on a mouse: yaw at 150°/s while the pitch sweeps 20° → 80° in a
    # second, back again, then a fast turn — every frame checked for holes in
    # the geometry view while it moves.
    # The same with the mouse itself, as a person drives it: a right-drag
    # turns and tilts (0.005 rad a pixel), a left-drag pans the grabbed point.
    "hand": ("P4 a hand on the mouse: right-drag turn and tilt, left-drag pan", [
        ("set", dict(NEUCH, pitch=30), 0.0), ("wait", 10.0), ("geometry",),
        ("mouse", "right", 520, -260, 1.0), ("mouse", "right", 520, 260, 1.0),
        ("mouse", "left", 900, 300, 1.0), ("mouse", "right", -600, 0, 1.0)], 20.0),
    "drag": ("P4 a hand's drag: yaw and tilt together", [
        ("set", dict(NEUCH, pitch=20), 0.0), ("wait", 10.0), ("geometry",),
        ("drag", 150.0, 20.0, 80.0, 1.0), ("drag", 150.0, 80.0, 20.0, 1.0),
        ("drag", 180.0, 30.0, 30.0, 1.5)], 20.0),
    "warm": ("P1 warm relaunch at the plateau", [("set", NEUCH, 0.0), ("wait", 15.0), ("reload", NEUCH)], 20.0),
}

STATUS = re.compile(
    r"(?P<tiles>\d+) tiles, (?P<complete>[^,]+), (?P<sharp>[^|]+) \| (?P<bare>\d+) bare \| "
    r"(?P<loading>\d+) loading .*budget (?P<held>\d+)/(?P<target>\d+) MiB \(hard (?P<hard>\d+)\), "
    r"(?P<flight>\d+) in flight \+ (?P<local>\d+) local, (?P<evicted>\d+) evicted, "
    r"(?P<timeouts>\d+) timed out, (?P<failed>\d+) failed, scale (?P<scale>[\d.]+), (?P<state>\w+)")


class Page:
    def __init__(self, ws, log):
        self.ws, self.log = ws, log
        self.n, self.pending, self.sessions = 0, {}, []
        self.status = []           # (t, dict)
        self.requests = {}         # (session, requestId) -> (url, t0)
        self.finished = []         # (url, t0, t1, ok)
        self.dupes = []            # (url, t)

    async def call(self, method, sid=None, **params):
        self.n += 1
        i = self.n
        fut = asyncio.get_event_loop().create_future()
        self.pending[i] = fut
        msg = {"id": i, "method": method, "params": params}
        if sid:
            msg["sessionId"] = sid
        await self.ws.send(json.dumps(msg))
        return await fut

    async def reader(self):
        async for raw in self.ws:
            m = json.loads(raw)
            if "id" in m and m["id"] in self.pending:
                self.pending.pop(m["id"]).set_result(m.get("result", m.get("error")))
                continue
            method, p, sid = m.get("method"), m.get("params", {}), m.get("sessionId")
            now = time.time()
            if method == "Target.attachedToTarget":
                s = p["sessionId"]
                self.sessions.append(s)
                for meth in ("Runtime.enable", "Network.enable", "Runtime.runIfWaitingForDebugger"):
                    self.n += 1
                    await self.ws.send(json.dumps({"id": self.n, "sessionId": s, "method": meth}))
            elif method == "Runtime.consoleAPICalled":
                text = " ".join(str(a.get("value", a.get("description", ""))) for a in p["args"])
                self.log.write(f"{now:.1f} {'W' if sid else 'P'} {p['type']}: {text}\n")
                hit = STATUS.search(text)
                if hit and not sid:
                    self.status.append((now, hit.groupdict()))
            elif method == "Network.requestWillBeSent":
                url = p["request"]["url"]
                # The workers' own scripts: a worker target never reports
                # them finished.
                if url.endswith(".js"):
                    continue
                if any(u == url for u, _ in self.requests.values()):
                    self.dupes.append((url, now))
                self.requests[(sid, p["requestId"])] = (url, now)
            elif method in ("Network.loadingFinished", "Network.loadingFailed"):
                got = self.requests.pop((sid, p["requestId"]), None)
                if got:
                    self.finished.append((got[0], got[1], now, method.endswith("Finished")))

    def oldest_pending(self, now, older_than=0.0):
        ages = [now - t for (_, t) in self.requests.values() if now - t >= older_than]
        return (len(ages), max(ages) if ages else 0.0)

    async def ev(self, js):
        return await self.call("Runtime.evaluate", expression=js, awaitPromise=True, returnByValue=True)

    async def shot(self, scale=None):
        if scale:
            m = (await self.ev("[innerWidth, innerHeight]"))["result"]["value"]
            r = await self.call("Page.captureScreenshot", format="png",
                                clip={"x": 0, "y": 0, "width": m[0], "height": m[1], "scale": scale})
        else:
            r = await self.call("Page.captureScreenshot", format="png")
        return base64.b64decode(r["data"])


def holes(png, step=4):
    """Black pixels in the lower ground band of a geometry-view frame."""
    im = Image.open(io.BytesIO(png)).convert("RGB")
    w, h = im.size
    px = im.load()
    band = range(int(h * 0.55), int(h * 0.95), step)
    black = total = 0
    for y in band:
        for x in range(int(w * 0.05), int(w * 0.85), step):
            total += 1
            if max(px[x, y]) < 12:
                black += 1
    return black, total


async def run(args):
    os.makedirs(args.out, exist_ok=True)
    w, h = args.size
    port = 9400 + (os.getpid() % 100)
    profile = tempfile.mkdtemp()
    chrome = subprocess.Popen([
        CHROME, "--headless=new", f"--remote-debugging-port={port}", f"--user-data-dir={profile}",
        "--enable-unsafe-webgpu", "--enable-gpu", "--use-angle=metal",
        f"--window-size={w},{h}", f"--force-device-scale-factor={args.dpr}",
        "--no-first-run", "about:blank"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            try:
                tabs = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json"))
                ws_url = [t for t in tabs if t["type"] == "page"][0]["webSocketDebuggerUrl"]
                break
            except Exception:
                time.sleep(0.1)
        results = []
        for name in args.patterns:
            results.append(await pattern(args, ws_url, name))
        return results
    finally:
        chrome.terminate()


async def pattern(args, ws_url, name):
    desc, steps, bound = PATTERNS[name]
    out = os.path.join(args.out, name)
    os.makedirs(out, exist_ok=True)
    url = args.url + (f"?backend={args.backend}" if args.backend != "webgpu" else "")
    log = open(os.path.join(out, "console.log"), "w")
    async with websockets.connect(ws_url, max_size=None) as ws:
        page = Page(ws, log)
        reader = asyncio.create_task(page.reader())
        await page.call("Runtime.enable")
        await page.call("Page.enable")
        await page.call("Network.enable")
        await page.call("Network.setCacheDisabled", cacheDisabled=False)
        await page.call("Target.setAutoAttach", autoAttach=True, waitForDebuggerOnStart=False, flatten=True)
        await page.call("Page.navigate", url=url)
        for _ in range(240):
            r = await page.ev("globalThis.viewer && globalThis.viewer.backend")
            if r.get("result", {}).get("value"):
                break
            await asyncio.sleep(0.25)
        backend = r.get("result", {}).get("value")
        moved_at = time.time()
        moving_holes = []          # (t, black, total) per frame shot while moving
        geometry_on = False
        for step in steps:
            kind = step[0]
            if kind == "geometry":
                for _ in range(3):
                    await page.call("Input.dispatchKeyEvent", type="keyDown", key="d", text="d")
                    await page.call("Input.dispatchKeyEvent", type="keyUp", key="d")
                geometry_on = True
                await asyncio.sleep(0.5)
                continue
            if kind == "mouse":
                _, button, dx, dy, secs = step
                m = (await page.ev("[innerWidth, innerHeight]"))["result"]["value"]
                x, y = m[0] * 0.45, m[1] * 0.5
                mask = {"left": 1, "right": 2}[button]
                await page.call("Input.dispatchMouseEvent", type="mousePressed", x=x, y=y,
                                button=button, buttons=mask, clickCount=1)
                t0 = time.time()

                async def drag_mouse():
                    nonlocal x, y
                    while time.time() - t0 < secs:
                        x += dx / 60.0
                        y += dy / 60.0
                        await page.call("Input.dispatchMouseEvent", type="mouseMoved", x=x, y=y,
                                        button=button, buttons=mask)
                        await asyncio.sleep(1 / 60)
                    await page.call("Input.dispatchMouseEvent", type="mouseReleased", x=x, y=y,
                                    button=button, buttons=0, clickCount=1)

                mover = asyncio.create_task(drag_mouse())
                while not mover.done():
                    png = await page.shot(scale=0.25)
                    black, total = holes(png, step=1)
                    moving_holes.append((round(time.time() - moved_at, 2), black, total))
                    if black and len([h for h in moving_holes if h[1]]) <= 5:
                        open(os.path.join(out, f"hole-{len(moving_holes)}.png"), "wb").write(png)
                await mover
                # The glide after the release, watched too.
                t1 = time.time()
                while time.time() - t1 < 1.5:
                    png = await page.shot(scale=0.25)
                    black, total = holes(png, step=1)
                    moving_holes.append((round(time.time() - moved_at, 2), black, total))
                    if black and len([h for h in moving_holes if h[1]]) <= 5:
                        open(os.path.join(out, f"hole-{len(moving_holes)}.png"), "wb").write(png)
                moved_at = time.time()
                continue
            if kind == "drag":
                _, rate, p0, p1, secs = step
                v = (await page.ev("viewer.getView()"))["result"]["value"]
                t0 = time.time()

                async def move():
                    while (t := time.time() - t0) < secs:
                        v["heading"] = (v["heading"] + rate / 60.0) % 360
                        v["pitch"] = p0 + (p1 - p0) * min(t / secs, 1.0)
                        await page.ev(f"viewer.setView({json.dumps(v)})")
                        await asyncio.sleep(1 / 60)

                mover = asyncio.create_task(move())
                # Shots as fast as the browser gives them, a quarter size:
                # the hole is the frame a hand sees, not the one after.
                while not mover.done():
                    png = await page.shot(scale=0.25)
                    black, total = holes(png, step=1)
                    moving_holes.append((round(time.time() - moved_at, 2), black, total))
                    if black and len([h for h in moving_holes if h[1]]) <= 5:
                        open(os.path.join(out, f"hole-{len(moving_holes)}.png"), "wb").write(png)
                await mover
                moved_at = time.time()
                continue
            if kind == "set":
                await page.ev(f"viewer.setView({json.dumps(step[1])})")
                moved_at = time.time()
            elif kind == "fly":
                await page.ev(f"viewer.flyTo({json.dumps(step[1])}, {{duration: {max(step[2], 0.1)}}})")
                await asyncio.sleep(step[2])
                moved_at = time.time()
            elif kind == "wait":
                await asyncio.sleep(step[1])
            elif kind == "yaw":
                v = (await page.ev("viewer.getView()"))["result"]["value"]
                n = int(step[2] * 60)
                for i in range(n):
                    v["heading"] = (v["heading"] + step[1] / 60.0) % 360
                    await page.ev(f"viewer.setView({json.dumps(v)})")
                    await asyncio.sleep(1 / 60)
                moved_at = time.time()
            elif kind == "tilt":
                v = (await page.ev("viewer.getView()"))["result"]["value"]
                n = int(step[3] * 60)
                for i in range(n):
                    v["pitch"] = step[1] + (step[2] - step[1]) * (i + 1) / n
                    await page.ev(f"viewer.setView({json.dumps(v)})")
                    await asyncio.sleep(1 / 60)
                moved_at = time.time()
            elif kind == "resize":
                await page.call("Emulation.setDeviceMetricsOverride", width=step[1], height=step[2],
                                deviceScaleFactor=args.dpr, mobile=False)
                await page.ev("viewer.resize()")
                moved_at = time.time()
            elif kind == "reload":
                # What the old page asked dies with it, unanswered: only the
                # relaunch's own requests and status count from here.
                await page.call("Page.reload")
                page.requests.clear()
                page.dupes.clear()
                page.status.clear()
                for _ in range(240):
                    r = await page.ev("globalThis.viewer && globalThis.viewer.backend")
                    if r.get("result", {}).get("value"):
                        break
                    await asyncio.sleep(0.25)
                await page.ev(f"viewer.setView({json.dumps(step[1])})")
                moved_at = time.time()
        # Hold still, watching the status line and the requests.
        converged_at, worst_pending, samples = None, (0, 0.0), []
        deadline = moved_at + bound + LOAD_TIMEOUT_S
        while time.time() < deadline:
            await asyncio.sleep(1.0)
            now = time.time()
            n, age = page.oldest_pending(now)
            worst_pending = max(worst_pending, (n, age), key=lambda p: p[1])
            last = page.status[-1][1] if page.status else None
            samples.append((round(now - moved_at, 1), n, round(age, 1), last and last["state"], last and int(last["loading"])))
            if last and last["state"] == "converged" and converged_at is None:
                converged_at = now - moved_at
            if converged_at is not None and now - moved_at > converged_at + 3 and name != "still":
                break
        # Loads the budget still runs once the view has converged (the ring
        # around it) are fine; a request hanging there for seconds is not.
        still_pending = page.oldest_pending(time.time(), older_than=5.0)
        normal = await page.shot()
        open(os.path.join(out, "normal.png"), "wb").write(normal)
        # The geometry view: black is ground no mesh covers.
        for _ in range(0 if geometry_on else 3):
            await page.call("Input.dispatchKeyEvent", type="keyDown", key="d", text="d")
            await page.call("Input.dispatchKeyEvent", type="keyUp", key="d")
        await asyncio.sleep(0.5)
        geometry = await page.shot()
        open(os.path.join(out, "geometry.png"), "wb").write(geometry)
        black, total = holes(geometry)
        reader.cancel()
    log.close()
    last = page.status[-1][1] if page.status else {}
    held, hard = int(last.get("held", 0)), int(last.get("hard", 0)) or 1 << 30
    worst_moving = max((h[1] for h in moving_holes), default=0)
    holed_frames = sum(1 for h in moving_holes if h[1])
    checks = {
        "coverage": (black == 0 and worst_moving == 0,
                     f"{black} of {total} sampled ground pixels with no mesh at rest; "
                     f"{holed_frames} of {len(moving_holes)} frames with holes while moving, worst {worst_moving}"),
        "convergence": (converged_at is not None and converged_at <= bound,
                        f"converged in {converged_at:.1f} s against {bound:.0f} s" if converged_at is not None
                        else f"not converged within {bound + LOAD_TIMEOUT_S:.0f} s ({last.get('loading', '?')} loading)"),
        "requests": (worst_pending[1] <= LOAD_TIMEOUT_S + 5 and (converged_at is None or still_pending[0] == 0)
                     and not page.dupes,
                     f"oldest pending {worst_pending[1]:.0f} s, {still_pending[0]} pending over 5 s at the end, "
                     f"{len(page.dupes)} duplicate concurrent requests, {len(page.finished)} finished"),
        "budget": (held <= hard, f"held {held} MiB of hard {hard}, {last.get('timeouts', '?')} timed out, "
                   f"{last.get('failed', '?')} failed, {last.get('evicted', '?')} evicted, scale {last.get('scale', '?')}"),
    }
    kept = all(ok for ok, _ in checks.values())
    print(f"{desc} [{name}] {backend} {args.size[0]}x{args.size[1]} @{args.dpr} — {'kept' if kept else 'VIOLATED'}")
    for k, (ok, what) in checks.items():
        print(f"  {'ok      ' if ok else 'VIOLATED'} {k}: {what}")
    now = time.time()
    pending = sorted(((round(now - t, 1), u) for (u, t) in page.requests.values()), reverse=True)
    json.dump({"samples": samples, "dupes": page.dupes[:200], "pending": pending},
              open(os.path.join(out, "trace.json"), "w"), indent=1)
    return {"pattern": name, "backend": backend, "kept": kept,
            "checks": {k: {"kept": ok, "what": w} for k, (ok, w) in checks.items()}}


def main():
    a = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    a.add_argument("--url", default="http://127.0.0.1:8091/examples/web-viewer/")
    a.add_argument("--backend", choices=["webgpu", "webgl2"], default="webgpu")
    a.add_argument("--dpr", type=float, default=2.0)
    a.add_argument("--size", type=lambda s: tuple(int(v) for v in s.split("x")), default=(1280, 800))
    a.add_argument("--patterns", type=lambda s: s.split(","), default=list(PATTERNS))
    a.add_argument("--out", default=tempfile.mkdtemp(prefix="web-bench-"))
    args = a.parse_args()
    results = asyncio.run(run(args))
    json.dump({"results": results}, open(os.path.join(args.out, "summary.json"), "w"), indent=2)
    print(f"summary: {os.path.join(args.out, 'summary.json')}")
    sys.exit(0 if all(r["kept"] for r in results) else 1)


if __name__ == "__main__":
    main()
