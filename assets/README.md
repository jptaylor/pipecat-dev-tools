# assets

## silero_vad_openvino_16k_named.onnx

[Silero VAD](https://github.com/snakers4/silero-vad) (MIT license), the
OpenVINO 16 kHz export — a flat graph with no ONNX `If` control flow, which is
what lets it run under pure-Rust [tract](https://github.com/sonos/tract)
(the main `silero_vad.onnx` and even the `op18_ifless` variant contain `If`
nodes tract cannot type-check).

Regenerate from upstream:

```bash
curl -sLO https://github.com/snakers4/silero-vad/raw/master/src/silero_vad/data/silero_vad_openvino_16k.onnx
uv run --with onnx python - <<'PY'
import onnx
m = onnx.load("silero_vad_openvino_16k.onnx")
seen = set()
for i, node in enumerate(m.graph.node):  # tract requires unique node names
    if not node.name or node.name in seen:
        node.name = f"n_{node.op_type}_{i}"
    seen.add(node.name)
onnx.checker.check_model(m)
onnx.save(m, "silero_vad_openvino_16k_named.onnx")
PY
```

Interface: input `[1, 576]` f32 (64 samples context + 512-sample chunk at
16 kHz), state `[2, 1, 128]` f32; outputs speech probability and next state.
