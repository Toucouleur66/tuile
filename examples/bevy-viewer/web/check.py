#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""The bevy-viewer's page in headless Chrome, once per graphics API.

Loads the page (served from this directory, tiles from heightfield-serve),
waits for `window.tuileStatus` to say the globe has settled, and checks the
frame: no black pixel anywhere (the clear colour is black, so a pixel no
ground covered is black), and the page's console free of errors. Writes a
screenshot and the console per backend into `--out`; exits 1 on a failure.

    cargo run --release -p tuile-heightfield-serve &          # tiles, :8092
    examples/bevy-viewer/web/build.sh
    python3 -m http.server 8093 -d examples/bevy-viewer/web &  # the page
    examples/bevy-viewer/web/check.py --out /tmp/bevy-web

Needs `pip install websockets pillow` and Google Chrome.
"""

import argparse
import asyncio
import base64
import io
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.request

import websockets
from PIL import Image

CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"


class Page:
    def __init__(self, ws, log):
        self.ws, self.log, self.n, self.pending, self.errors = ws, log, 0, {}, []

    async def call(self, method, **params):
        self.n += 1
        fut = asyncio.get_event_loop().create_future()
        self.pending[self.n] = fut
        await self.ws.send(json.dumps({"id": self.n, "method": method, "params": params}))
        return await fut

    async def reader(self):
        async for raw in self.ws:
            m = json.loads(raw)
            if "id" in m and m["id"] in self.pending:
                self.pending.pop(m["id"]).set_result(m.get("result", m.get("error")))
            elif m.get("method") == "Runtime.consoleAPICalled":
                p = m["params"]
                text = " ".join(str(a.get("value", a.get("description", ""))) for a in p["args"])
                self.log.write(f"{time.time():.1f} {p['type']}: {text}\n")
                if p["type"] == "error":
                    self.errors.append(text)
            elif m.get("method") == "Runtime.exceptionThrown":
                d = m["params"]["exceptionDetails"]
                text = d.get("exception", {}).get("description", d.get("text", ""))
                self.log.write(f"{time.time():.1f} exception: {text}\n")
                if "Using exceptions for control flow" not in text:
                    self.errors.append(text)

    async def ev(self, js):
        r = await self.call("Runtime.evaluate", expression=js, returnByValue=True)
        return r.get("result", {}).get("value")


async def check(args, ws_url, backend):
    out = os.path.join(args.out, backend)
    os.makedirs(out, exist_ok=True)
    log = open(os.path.join(out, "console.log"), "w")
    async with websockets.connect(ws_url, max_size=None) as ws:
        page = Page(ws, log)
        reader = asyncio.create_task(page.reader())
        await page.call("Runtime.enable")
        await page.call("Page.enable")
        url = f"{args.url}?backend={backend}" + (f"&{args.query}" if args.query else "")
        await page.call("Page.navigate", url=url)
        started, status, settled_at = time.time(), None, None
        while time.time() - started < args.timeout:
            await asyncio.sleep(0.5)
            status = await page.ev("window.tuileStatus || ''")
            if status and "settled=true" in status:
                settled_at = time.time() - started
                break
            if status and status.startswith("Failed"):
                break
        await asyncio.sleep(1.0)
        status = await page.ev("window.tuileStatus || ''") or status
        shot = base64.b64decode((await page.call("Page.captureScreenshot", format="png"))["data"])
        open(os.path.join(out, "frame.png"), "wb").write(shot)
        im = Image.open(io.BytesIO(shot)).convert("RGB")
        black = sum(1 for p in im.getdata() if max(p) <= 8)
        reader.cancel()
    result = {
        "backend": backend,
        "status": status,
        "settled_after_s": settled_at,
        "black_pixels": black,
        "pixels": im.size[0] * im.size[1],
        "console_errors": page.errors[:10],
    }
    result["ok"] = settled_at is not None and black == 0 and not page.errors
    return result


async def run(args):
    os.makedirs(args.out, exist_ok=True)
    port = 9500 + (os.getpid() % 100)
    profile = tempfile.mkdtemp()
    chrome = subprocess.Popen([
        CHROME, "--headless=new", f"--remote-debugging-port={port}", f"--user-data-dir={profile}",
        "--enable-unsafe-webgpu", "--enable-gpu", "--use-angle=metal",
        "--window-size=1280,720", "--force-device-scale-factor=1",
        "--no-first-run", "about:blank"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            try:
                tabs = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json"))
                ws_url = [t for t in tabs if t["type"] == "page"][0]["webSocketDebuggerUrl"]
                break
            except Exception:
                time.sleep(0.1)
        return [await check(args, ws_url, b) for b in args.backends]
    finally:
        chrome.terminate()


def main():
    a = argparse.ArgumentParser()
    a.add_argument("--url", default="http://127.0.0.1:8093/index.html")
    a.add_argument("--query", default="", help="the application's options, e.g. range=3000&pitch=30")
    a.add_argument("--backends", default="webgpu,webgl2", type=lambda s: s.split(","))
    a.add_argument("--timeout", default=120.0, type=float)
    a.add_argument("--out", default="bevy-web")
    args = a.parse_args()
    results = asyncio.run(run(args))
    json.dump(results, open(os.path.join(args.out, "summary.json"), "w"), indent=2)
    for r in results:
        print(json.dumps(r))
    sys.exit(0 if all(r["ok"] for r in results) else 1)


if __name__ == "__main__":
    main()
