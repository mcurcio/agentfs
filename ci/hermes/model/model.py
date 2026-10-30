#!/usr/bin/env python3
"""A scripted stand-in for an OpenAI-compatible model API (ci/e2e-hermes.sh).

Hermes reaches it as its `custom` provider (ci/hermes/hermes/config.yaml).
No model, no key, no network: every reply comes from scenarios.json.

A prompt names its scenario with `agentfs-scenario:<name>`. The reply to
the request whose last user message names it is the scenario's step N,
where N counts the assistant turns since that message: each step is one
tool call, and after the last step the reply is plain text, so Hermes'
loop ends. Any other request (a title, a summary) gets plain text.

Every request body is kept in memory, in order, and served at
GET /requests (a JSON list), so the test reads what Hermes sent.

    python3 model.py <port> <scenarios.json>
"""

import json
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Lock

MODEL = "scripted"
SCENARIO = re.compile(r"agentfs-scenario:([a-z0-9-]+)")

requests = []
lock = Lock()


def text_of(message):
    content = message.get("content")
    if isinstance(content, list):
        return "".join(p.get("text", "") for p in content if isinstance(p, dict))
    return content or ""


def reply_for(body, scenarios):
    """(text, tool_call or None) for a chat request."""
    messages = body.get("messages") or []
    last_user = max((i for i, m in enumerate(messages) if m.get("role") == "user"), default=None)
    if last_user is None:
        return "ok", None
    found = SCENARIO.search(text_of(messages[last_user]))
    if not found:
        return "ok", None
    name = found.group(1)
    steps = scenarios.get(name)
    if steps is None:
        return f"unknown scenario {name}", None
    step = sum(1 for m in messages[last_user + 1:] if m.get("role") == "assistant")
    if step >= len(steps):
        return f"agentfs-scenario {name}: done", None
    call = steps[step]
    return "", {
        "id": f"call_{name}_{step}",
        "type": "function",
        "function": {"name": call["tool"], "arguments": json.dumps(call["arguments"])},
    }


class Handler(BaseHTTPRequestHandler):
    scenarios = {}

    def log_message(self, fmt, *args):
        sys.stdout.write("model: " + fmt % args + "\n")
        sys.stdout.flush()

    def send_json(self, code, value):
        data = json.dumps(value).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = self.path.split("?")[0].rstrip("/")
        if path == "/requests":
            with lock:
                self.send_json(200, list(requests))
        elif path.endswith("/models"):
            self.send_json(200, {"object": "list", "data": [
                {"id": MODEL, "object": "model", "owned_by": "agentfs-ci", "context_length": 131072}]})
        elif path in ("", "/health", "/v1"):
            self.send_json(200, {"status": "ok"})
        else:
            self.send_json(404, {"error": {"message": f"no route {self.path}"}})

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        try:
            body = json.loads(self.rfile.read(length) or b"{}")
        except ValueError:
            self.send_json(400, {"error": {"message": "not JSON"}})
            return
        with lock:
            requests.append({"path": self.path, "body": body})
        if not self.path.rstrip("/").endswith("/chat/completions"):
            self.send_json(404, {"error": {"message": f"no route {self.path}"}})
            return
        text, call = reply_for(body, self.scenarios)
        sys.stdout.write(f"model: reply {'tool ' + call['function']['name'] if call else repr(text)}\n")
        sys.stdout.flush()
        finish = "tool_calls" if call else "stop"
        usage = {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        created = int(time.time())
        if body.get("stream"):
            self.stream(text, call, finish, usage, created)
            return
        message = {"role": "assistant", "content": text or None}
        if call:
            message["tool_calls"] = [call]
        self.send_json(200, {
            "id": "chatcmpl-agentfs", "object": "chat.completion", "created": created,
            "model": MODEL, "usage": usage,
            "choices": [{"index": 0, "message": message, "finish_reason": finish}]})

    def stream(self, text, call, finish, usage, created):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()

        def chunk(delta, finish_reason=None, **extra):
            event = {"id": "chatcmpl-agentfs", "object": "chat.completion.chunk",
                     "created": created, "model": MODEL,
                     "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}], **extra}
            self.wfile.write(b"data: " + json.dumps(event).encode() + b"\n\n")

        chunk({"role": "assistant", "content": ""})
        if text:
            chunk({"content": text})
        if call:
            chunk({"tool_calls": [{"index": 0, **call}]})
        chunk({}, finish)
        self.wfile.write(b"data: " + json.dumps({
            "id": "chatcmpl-agentfs", "object": "chat.completion.chunk", "created": created,
            "model": MODEL, "choices": [], "usage": usage}).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


def main():
    port, script = int(sys.argv[1]), sys.argv[2]
    with open(script, encoding="utf-8") as f:
        Handler.scenarios = json.load(f)
    server = ThreadingHTTPServer(("0.0.0.0", port), Handler)
    print(f"model: serving {MODEL} on :{port} with {sorted(Handler.scenarios)}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
