# Regenerates deepseek_reference.jsonl.zstd with the reference Zstandard library (Python 3.14+):
# a DeepSeek session as dsh would compress it, in two concatenated frames with content checksums,
# the second spanning several blocks. Run: py -I make_deepseek_reference.py
import json
from compression import zstd
t = 1791462896000  # 2026-10-08T12:34:56Z in epoch milliseconds
opts = {zstd.CompressionParameter.checksum_flag: 1}
first = "".join(json.dumps(r, separators=(",", ":")) + "\n" for r in [
    {"type": "session", "version": 3, "id": "reference-session", "createdAt": t, "isSeeded": False},
    {"type": "step/start", "seq": 0, "time": t, "data": {"turn": 1, "step": 1}},
])
second = "".join(json.dumps(r, separators=(",", ":")) + "\n" for r in [
    {"type": "tool/result", "seq": 1, "time": t + 1000, "data": {"turn": 1, "step": 1, "output": "abcdefghij" * 30000}},
    {"type": "assistant/message", "seq": 2, "time": t + 6000, "data": {"turn": 1, "step": 1,
        "message": {"id": "reference", "role": "assistant", "source": {"kind": "model", "provider": "deepseek", "model": "deepseek-v4"}},
        "usage": {"inputTokens": 100, "outputTokens": 80}, "stream": []}},
])
frames = zstd.compress(first.encode(), options=opts) + zstd.compress(second.encode(), options=opts)
assert zstd.decompress(frames) == (first + second).encode()

open(__file__.rsplit(".", 1)[0].replace("make_", "") + ".jsonl.zstd", "wb").write(frames)
