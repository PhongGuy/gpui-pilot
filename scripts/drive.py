#!/usr/bin/env python3
"""Drive a gpui-pilot server by hand.

Usage: drive.py [--server PATH] [--out DIR]

Reads one call per line from stdin, `<tool> [json args]`; blank lines and lines starting
with `#` are skipped. The server is --server, else $GPUI_PILOT, else `gpui-pilot` on PATH,
run as `<server> mcp` without DISPLAY or WAYLAND_DISPLAY. Exits 1 if any call failed.
`type` text is echoed as `<redacted N chars>` when the call has `redact: true`, the server
redacted it (a masked input) or the call failed.
"""
import argparse
import base64
import json
import os
import subprocess
import sys


def redacted(text):
    """The placeholder the server reports for redacted text: `<redacted N chars>`."""
    n = len(text)
    return "<redacted 1 char>" if n == 1 else f"<redacted {n} chars>"


def typed_echo(arguments, reply):
    """`type`'s arguments as logged: the text redacted when the caller asked, when the server
    redacted it (a masked input), or when the call failed and masking is unknown."""
    result = reply.get("result", {})
    structured = result.get("structuredContent") or {}
    failed = "error" in reply or bool(result.get("isError"))
    text = arguments.get("text")
    if isinstance(text, str) and (arguments.get("redact") or "redacted" in structured or failed):
        return {**arguments, "text": redacted(text)}
    return arguments


def main():
    parser = argparse.ArgumentParser(description="Drive a gpui-pilot server from stdin.")
    parser.add_argument("--server", help="server binary (default: $GPUI_PILOT, else gpui-pilot)")
    parser.add_argument("--out", default=".", help="directory for screenshots and tree JSON")
    opts = parser.parse_args()
    server = opts.server or os.environ.get("GPUI_PILOT") or "gpui-pilot"
    env = {k: v for k, v in os.environ.items() if k not in ("DISPLAY", "WAYLAND_DISPLAY")}
    proc = subprocess.Popen(
        [server, "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=env
    )
    next_id = 0

    def rpc(method, params=None, notify=False):
        # Send one JSON-RPC message; for a request, skip server messages until its reply.
        nonlocal next_id
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        if not notify:
            next_id += 1
            msg["id"] = next_id
        proc.stdin.write(json.dumps(msg) + "\n")
        proc.stdin.flush()
        if notify:
            return None
        while True:
            line = proc.stdout.readline()
            if not line:
                raise SystemExit("server closed its output before replying")
            reply = json.loads(line)
            if reply.get("id") == next_id:
                return reply

    rpc(
        "initialize",
        {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "drive.py", "version": "0"},
        },
    )
    rpc("notifications/initialized", notify=True)

    os.makedirs(opts.out, exist_ok=True)
    counts = {"shot": 0, "tree": 0}
    failed = False
    for line in sys.stdin:
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        name, _, args = line.partition(" ")
        args = args.strip()
        arguments = json.loads(args or "{}")
        # A `type` call is echoed once its reply says whether the text went into a masked
        # input; until then it may be a secret.
        if name != "type":
            print(f"> {name} {args or '{}'}")
        reply = rpc("tools/call", {"name": name, "arguments": arguments})
        if name == "type":
            print(f"> type {json.dumps(typed_echo(arguments, reply))}")
        if "error" in reply:
            failed = True
            print(reply["error"].get("message", reply["error"]))
            continue
        result = reply["result"]
        failed = failed or bool(result.get("isError"))
        for block in result.get("content", []):
            if block["type"] == "image":
                counts["shot"] += 1
                path = os.path.join(opts.out, f"shot-{counts['shot']}.png")
                with open(path, "wb") as f:
                    f.write(base64.b64decode(block["data"]))
                print(f"image: {path}")
            elif block["type"] == "text":
                print(block["text"])
        structured = result.get("structuredContent")
        if structured is None:
            continue
        if name == "tree":
            counts["tree"] += 1
            path = os.path.join(opts.out, f"tree-{counts['tree']}.json")
            with open(path, "w") as f:
                json.dump(structured, f, indent=1)
            print(f"structured: saved to {path}")
        else:
            print("structured: " + json.dumps(structured, separators=(",", ":")))

    proc.stdin.close()
    proc.wait()
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
