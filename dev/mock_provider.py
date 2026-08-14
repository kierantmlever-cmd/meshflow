#!/usr/bin/env python3
"""Minimal OpenAI-compatible endpoint for developing without a key or a local model.

    python3 dev/mock_provider.py &
    MESHFLOW_BASE_URL=http://localhost:8081/v1 cargo run

Serves /v1/models and a streaming /v1/chat/completions that replies with markdown, one token at a
time, so the UI's streaming path gets exercised for real. Stdlib only, no deps.
"""

import json
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = 8081

REPLY = """Here's what the **streaming path** looks like end to end.

The engine runs on Tokio, the UI on Freya's single-threaded runtime, and they only ever talk
through two channels:

```rust
let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<EngineCommand>();
let (evt_tx, _)      = broadcast::channel::<EngineEvent>(1024);
rt.spawn(mf_engine::run(cmd_rx, evt_tx.clone()));
```

Deltas are coalesced on a 50ms tick, so a fast stream doesn't rebuild the markdown tree once per
token.

| Layer | Thread | Send? |
| --- | --- | --- |
| Engine | Tokio pool | yes |
| UI | Freya renderer | no |

That boundary is enforced by the compiler, not by convention.
"""


def _catalogue():
    """An aggregator-shaped catalogue, so the settings model picker can be tested for real.

    Deliberately long and mostly noise: the picker's vendor filter only engages past a
    threshold, and a three-entry list would never exercise it. The vendor-namespaced ids are
    invented — the point is the *shape* (`vendor/model`), which is what the filter keys on.
    """
    flagship = [
        ("openai/gpt-5.6-sol", 400000),
        ("openai/gpt-5.6-terra", 400000),
        ("anthropic/claude-opus-5", 1000000),
        ("anthropic/claude-fable-5", 1000000),
        ("anthropic/claude-sonnet-5", 1000000),
        ("deepseek/deepseek-chat", 128000),
        ("deepseek/deepseek-coder", 128000),
        ("google/gemini-3-pro", 2000000),
        ("qwen/qwen3-max", 256000),
        ("minimax/minimax-m2", 200000),
    ]
    noise = [(f"othervendor/legacy-model-{i}", 8192) for i in range(60)]
    return [
        {"id": model_id, "context_length": ctx} for model_id, ctx in flagship + noise
    ]


def tokenize(text):
    """Split into small chunks that straddle markdown syntax, like a real model would."""
    out, buf = [], ""
    for ch in text:
        buf += ch
        if len(buf) >= 6:
            out.append(buf)
            buf = ""
    if buf:
        out.append(buf)
    return out


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _json(self, payload):
        body = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.endswith("/models"):
            self._json({"data": _catalogue()})
        else:
            self.send_error(404)

    def do_POST(self):
        if not self.path.endswith("/chat/completions"):
            return self.send_error(404)

        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length) or "{}")

        # Drive the tool loop when the user asks for it: first turn calls a tool, and the
        # turn after the tool result is plain prose. Detected by looking for a tool result
        # already in the history.
        wants_tool = "tool" in json.dumps(body.get("messages", [])).lower()
        already_ran = any(m.get("role") == "tool" for m in body.get("messages", []))
        if wants_tool and not already_ran:
            return self.stream_tool_call()

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        # No Content-Length, so the body must be delimited by closing the connection. Real
        # providers use chunked encoding instead; the client must not depend on either, and
        # terminates on the [DONE] sentinel.
        self.send_header("Connection", "close")
        self.close_connection = True
        self.end_headers()

        def emit(obj):
            self.wfile.write(f"data: {json.dumps(obj)}\n\n".encode())
            self.wfile.flush()

        try:
            emit({"choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]})
            for chunk in tokenize(REPLY):
                emit({"choices": [{"index": 0, "delta": {"content": chunk}}]})
                time.sleep(0.02)
            emit({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
            emit({"choices": [], "usage": {"prompt_tokens": 42, "completion_tokens": 310}})
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass  # client cancelled mid-stream, which is a case worth being able to test


    def stream_tool_call(self):
        """Emit a run_command tool call, fragment by fragment, like a real provider."""
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.close_connection = True
        self.end_headers()

        def emit(obj):
            self.wfile.write(f"data: {json.dumps(obj)}\n\n".encode())
            self.wfile.flush()

        args = json.dumps({"command": "echo hello from the tool"})
        try:
            emit({"choices": [{"delta": {"role": "assistant", "content": "Let me run that.\n\n"}}]})
            emit({"choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_demo", "type": "function",
                "function": {"name": "run_command", "arguments": ""},
            }]}}]})
            # Split mid-token so the client has to reassemble fragments.
            for i in range(0, len(args), 7):
                emit({"choices": [{"delta": {"tool_calls": [{
                    "index": 0, "function": {"arguments": args[i:i + 7]},
                }]}}]})
                time.sleep(0.02)
            emit({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]})
            emit({"choices": [], "usage": {"prompt_tokens": 60, "completion_tokens": 25}})
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


if __name__ == "__main__":
    print(f"mock provider on http://localhost:{PORT}/v1")
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
