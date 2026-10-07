#!/usr/bin/env python3
"""Fake model provider for testing Speed Tracker without spending tokens.

Streams a reply with a known time to first token and a known token rate, in
Anthropic Messages, OpenAI Chat Completions or OpenAI Responses format, so the
numbers the proxy reports can be checked against the truth.

    python3 Scripts/mock_upstream.py [port]   # default 4198
"""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TTFT = 0.5      # seconds before the first token
TOKENS = 120    # tokens in the reply
RATE = 60.0     # tokens per second
WORD = "abcd"   # four characters, one "token"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        pass

    def do_GET(self):
        self.reply_json(200, {"object": "list", "data": [{"id": "mock-model", "object": "model"}]})

    def do_HEAD(self):
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length)
        try:
            body = json.loads(raw or b"{}")
        except ValueError:
            body = {}
        print(f"{self.command} {self.path}  model={body.get('model')}  stream={body.get('stream')}  "
              f"ua={self.headers.get('User-Agent')!r}", flush=True)
        model = body.get("model") or "mock-model"
        path = self.path.split("?")[0]
        if path.endswith("/count_tokens"):
            return self.reply_json(200, {"input_tokens": 42})
        if path.endswith("/messages"):
            if body.get("stream"):
                return self.stream(self.anthropic_events(model))
            time.sleep(TTFT + TOKENS / RATE)
            return self.reply_json(200, {
                "id": "msg_mock", "type": "message", "role": "assistant", "model": model,
                "content": [{"type": "text", "text": WORD * TOKENS}], "stop_reason": "end_turn",
                "usage": {"input_tokens": 25, "output_tokens": TOKENS},
            })
        if path.endswith("/chat/completions"):
            return self.stream(self.chat_events(model))
        if path.endswith("/responses"):
            return self.stream(self.responses_events(model))
        self.reply_json(404, {"error": {"message": f"mock has no {path}"}})

    # -- formats ---------------------------------------------------------

    def anthropic_events(self, model):
        yield "message_start", {"type": "message_start", "message": {
            "id": "msg_mock", "type": "message", "role": "assistant", "model": model, "content": [],
            "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 25, "cache_read_input_tokens": 0, "output_tokens": 1}}}
        yield None, TTFT
        yield "content_block_start", {"type": "content_block_start", "index": 0,
                                      "content_block": {"type": "text", "text": ""}}
        for _ in range(TOKENS):
            yield "content_block_delta", {"type": "content_block_delta", "index": 0,
                                          "delta": {"type": "text_delta", "text": WORD}}
            yield None, 1 / RATE
        yield "content_block_stop", {"type": "content_block_stop", "index": 0}
        yield "message_delta", {"type": "message_delta",
                                "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                                "usage": {"output_tokens": TOKENS}}
        yield "message_stop", {"type": "message_stop"}

    def chat_events(self, model):
        def chunk(delta, finish=None):
            return {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "created": int(time.time()),
                    "model": model, "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
        yield None, chunk({"role": "assistant", "content": ""})
        yield None, TTFT
        for index in range(TOKENS):
            # First third arrives as reasoning, the way DeepSeek's reasoner streams it.
            key = "reasoning_content" if index < TOKENS // 3 else "content"
            yield None, chunk({key: WORD})
            yield None, 1 / RATE
        yield None, chunk({}, "stop")
        yield None, {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "model": model, "choices": [],
                     "usage": {"prompt_tokens": 25, "completion_tokens": TOKENS,
                               "total_tokens": 25 + TOKENS,
                               "completion_tokens_details": {"reasoning_tokens": TOKENS // 3}}}
        yield None, "[DONE]"

    def responses_events(self, model):
        base = {"id": "resp_mock", "object": "response", "created_at": int(time.time()), "model": model,
                "status": "in_progress", "output": []}
        yield "response.created", {"type": "response.created", "response": base}
        yield None, TTFT
        item = {"id": "msg_mock", "type": "message", "role": "assistant", "status": "in_progress", "content": []}
        yield "response.output_item.added", {"type": "response.output_item.added", "output_index": 0, "item": item}
        yield "response.content_part.added", {"type": "response.content_part.added", "item_id": "msg_mock",
                                              "output_index": 0, "content_index": 0,
                                              "part": {"type": "output_text", "text": "", "annotations": []}}
        for _ in range(TOKENS):
            yield "response.output_text.delta", {"type": "response.output_text.delta", "item_id": "msg_mock",
                                                 "output_index": 0, "content_index": 0, "delta": WORD}
            yield None, 1 / RATE
        text = WORD * TOKENS
        done_item = {"id": "msg_mock", "type": "message", "role": "assistant", "status": "completed",
                     "content": [{"type": "output_text", "text": text, "annotations": []}]}
        yield "response.output_text.done", {"type": "response.output_text.done", "item_id": "msg_mock",
                                            "output_index": 0, "content_index": 0, "text": text}
        yield "response.content_part.done", {"type": "response.content_part.done", "item_id": "msg_mock",
                                             "output_index": 0, "content_index": 0,
                                             "part": done_item["content"][0]}
        yield "response.output_item.done", {"type": "response.output_item.done", "output_index": 0,
                                            "item": done_item}
        final = dict(base, status="completed", output=[done_item], usage={
            "input_tokens": 25, "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": TOKENS, "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": 25 + TOKENS})
        yield "response.completed", {"type": "response.completed", "response": final}

    # -- plumbing --------------------------------------------------------

    def stream(self, events):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        # Pace against absolute deadlines so sleep overshoot does not slow the stream.
        deadline = time.monotonic()
        try:
            for name, payload in events:
                if isinstance(payload, float):
                    deadline += payload
                    time.sleep(max(0.0, deadline - time.monotonic()))
                    continue
                data = payload if isinstance(payload, str) else json.dumps(payload)
                text = (f"event: {name}\n" if name else "") + f"data: {data}\n\n"
                encoded = text.encode()
                self.wfile.write(f"{len(encoded):x}\r\n".encode() + encoded + b"\r\n")
                self.wfile.flush()
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            print("  client disconnected mid-stream", flush=True)

    def reply_json(self, status, payload):
        encoded = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 4198
    print(f"mock upstream on http://127.0.0.1:{port}  (ttft {TTFT}s, {TOKENS} tokens at {RATE:.0f}/s)", flush=True)
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
