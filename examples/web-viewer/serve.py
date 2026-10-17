#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Serves the example page and the package, on the loopback interface only.

    python3 examples/web-viewer/serve.py [--port 8080] [--env PATH]

Only two directories are served — the page and the package — never the rest
of the repository, and never a dotfile. For development, `GET /dev-token`
answers with `TUILE_ION_TOKEN` read from the `.env` file at that moment: the
value is never printed, never written anywhere, never put in a URL, and the
endpoint does not exist on any other interface than 127.0.0.1.
"""

import argparse
import http.server
import pathlib

ROOT = pathlib.Path(__file__).resolve().parents[2]
SERVED = ("examples/web-viewer/", "packages/tuile-viewer/")


def read_token(env):
    try:
        for line in env.read_text().splitlines():
            key, _, value = line.partition("=")
            if key.strip() == "TUILE_ION_TOKEN":
                return value.strip().strip("\"'")
    except OSError:
        pass
    return None


class Handler(http.server.SimpleHTTPRequestHandler):
    env = ROOT / ".env"

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(ROOT), **kwargs)

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path == "/":
            self.send_response(302)
            self.send_header("Location", "/examples/web-viewer/")
            self.end_headers()
            return
        if path == "/dev-token":
            token = read_token(self.env)
            if not token:
                self.send_error(404, "no TUILE_ION_TOKEN in the .env file")
                return
            body = token.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        relative = path.lstrip("/")
        if not relative.startswith(SERVED) or "/." in "/" + relative:
            self.send_error(404)
            return
        super().do_GET()

    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        super().end_headers()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--port", type=int, default=8080)
    parser.add_argument("--env", type=pathlib.Path, default=ROOT / ".env")
    args = parser.parse_args()
    Handler.env = args.env
    Handler.extensions_map[".wasm"] = "application/wasm"
    Handler.extensions_map[".js"] = "text/javascript"
    server = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"http://127.0.0.1:{args.port}/")
    server.serve_forever()


if __name__ == "__main__":
    main()
