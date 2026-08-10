#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

cargo bench --bench render -- \
  --noplot \
  --warm-up-time 1 \
  --measurement-time 2 \
  --sample-size 50

python3 - <<'PY'
import json
from pathlib import Path

root = Path("target/criterion")
feed = [
    "plain_200x50",
    "styled_200x50",
    "plain_64k_200x50",
    "tui_frame_200x50",
    "dense_anim_200x50",
]
wire = [
    "encode_styled_200x50",
    "decode_styled_200x50",
    "clone_styled_200x50",
    "encode_dense_200x50",
    "decode_dense_200x50",
    "clone_dense_200x50",
]

def means(group, names):
    values = []
    for name in names:
        path = root / group / name / "new" / "estimates.json"
        estimate = json.loads(path.read_text())["mean"]["point_estimate"]
        values.append(estimate)
    return values

feed_ns = sum(means("feed", feed))
wire_ns = sum(means("wire", wire))
print(f"METRIC render_ns={feed_ns + wire_ns:.3f}")
print(f"METRIC feed_ns={feed_ns:.3f}")
print(f"METRIC wire_ns={wire_ns:.3f}")
PY
