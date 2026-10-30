#!/usr/bin/env python3
"""A fake OpenAI chat-completions engine for simulate-update.sh.

A request whose conversation carries `SIMSTAGE-<name>` and offers the
`complete_task` tool is a stage agent's: its streamed reply is held, with SSE
keepalive comments, until `<release-dir>/<name>` exists (or --hold-limit
passes), then it calls `complete_task`. Every other request gets a short text
reply at once. Each request is logged as one JSON line.
"""

import argparse
import json
import os
import re
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MARKER = re.compile(r"SIMSTAGE-([A-Za-z0-9_]+)")
KEEPALIVE_SECS = 5.0
MAX_BODY_BYTES = 64 << 20


class Settings:
    def __init__(self, release_dir, log_path, hold_limit):
        assert os.path.isdir(release_dir), f"{release_dir} is not a directory"
        assert hold_limit > 0, "a held reply needs a positive limit"
        self.release_dir = release_dir
        self.log_path = log_path
        self.hold_limit = hold_limit
        self.log_lock = threading.Lock()

    def log(self, **fields):
        fields["t"] = round(time.time(), 3)
        line = json.dumps(fields, sort_keys=True)
        with self.log_lock, open(self.log_path, "a", encoding="utf-8") as out:
            out.write(line + "\n")


def message_text(message):
    """Every string in one chat message, content parts included."""
    content = message.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return " ".join(p.get("text", "") for p in content if isinstance(p, dict))
    return ""


def tool_names(body):
    tools = body.get("tools") or []
    return {t.get("function", {}).get("name") for t in tools if isinstance(t, dict)}


def stage_marker(body):
    """The SIMSTAGE name of a stage agent's request that has not completed yet."""
    messages = body.get("messages") or []
    assert isinstance(messages, list), "messages is a list"
    if "complete_task" not in tool_names(body):
        return None
    for message in messages:
        for call in message.get("tool_calls") or []:
            if call.get("function", {}).get("name") == "complete_task":
                return None
    for message in messages:
        found = MARKER.search(message_text(message))
        if found:
            return found.group(1)
    return None


def chunk(model, delta=None, finish=None, usage=None):
    choices = [] if usage else [{"index": 0, "delta": delta or {}, "finish_reason": finish}]
    body = {
        "id": "chatcmpl-sim",
        "object": "chat.completion.chunk",
        "created": int(time.time()),
        "model": model,
        "choices": choices,
    }
    if usage:
        body["usage"] = usage
    return f"data: {json.dumps(body)}\n\n".encode()


def completion_call(name):
    arguments = {
        "summary": f"simulated stage {name} finished",
        "structured": {"sources": [], "reports": []},
    }
    return {
        "role": "assistant",
        "tool_calls": [
            {
                "index": 0,
                "id": f"call_sim_{name}_{int(time.time())}",
                "type": "function",
                "function": {"name": "complete_task", "arguments": json.dumps(arguments)},
            }
        ],
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    settings = None

    def log_message(self, format, *args):
        pass

    def do_GET(self):
        if not self.path.rstrip("/").endswith("/models"):
            self.send_error(404)
            return
        payload = json.dumps({"object": "list", "data": [{"id": "sim-model", "object": "model"}]})
        self.reply_json(payload.encode())

    def do_POST(self):
        if not self.path.rstrip("/").endswith("/chat/completions"):
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length") or 0)
        assert 0 < length <= MAX_BODY_BYTES, f"request body of {length} bytes"
        body = json.loads(self.rfile.read(length))
        assert isinstance(body, dict), "a chat request is an object"
        model = body.get("model", "sim-model")
        name = stage_marker(body)
        self.settings.log(kind="stage" if name else "other", marker=name, model=model,
                          messages=len(body.get("messages") or []))
        self.start_stream()
        try:
            if name:
                self.hold_then_complete(model, name)
            else:
                self.send_text(model, "Noted.")
            self.finish_stream(model)
        except (BrokenPipeError, ConnectionResetError):
            self.settings.log(kind="disconnected", marker=name)
        self.close_connection = True

    def reply_json(self, payload):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def start_stream(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.flush()

    def hold_then_complete(self, model, name):
        release = os.path.join(self.settings.release_dir, name)
        deadline = time.monotonic() + self.settings.hold_limit
        while not os.path.exists(release) and time.monotonic() < deadline:
            self.wfile.write(b": holding\n\n")
            self.wfile.flush()
            time.sleep(KEEPALIVE_SECS)
        released = os.path.exists(release)
        self.settings.log(kind="released" if released else "hold-expired", marker=name)
        self.wfile.write(chunk(model, delta=completion_call(name)))
        self.wfile.write(chunk(model, finish="tool_calls"))

    def send_text(self, model, text):
        self.wfile.write(chunk(model, delta={"role": "assistant", "content": text}))
        self.wfile.write(chunk(model, finish="stop"))

    def finish_stream(self, model):
        usage = {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        self.wfile.write(chunk(model, usage=usage))
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port-file", required=True, help="where the bound port is written")
    parser.add_argument("--release-dir", required=True)
    parser.add_argument("--log", required=True)
    parser.add_argument("--hold-limit", type=float, default=1800.0)
    args = parser.parse_args()
    Handler.settings = Settings(args.release_dir, args.log, args.hold_limit)
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    port = server.server_address[1]
    assert port > 0, "the server bound a port"
    with open(args.port_file + ".part", "w", encoding="utf-8") as out:
        out.write(f"{port}\n")
    os.replace(args.port_file + ".part", args.port_file)
    server.serve_forever()


if __name__ == "__main__":
    sys.exit(main())
