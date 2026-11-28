#!/usr/bin/env python3
"""RawGraph RGD1 one-shot CPU depth worker; no downloads."""
import os
import struct
import sys

import numpy as np
import onnxruntime as ort
from PIL import Image


def main():
    model = os.environ.get("RAWGRAPH_DEPTH_MODEL")
    if not model or not os.path.isfile(model):
        raise RuntimeError("set RAWGRAPH_DEPTH_MODEL to the downloaded model.onnx; see docs/depth-estimate.md")
    raw = sys.stdin.buffer.read()
    if len(raw) < 12 or raw[:4] != b"RGD1":
        raise ValueError("invalid RGD1 request")
    w, h = struct.unpack_from("<II", raw, 4)
    if not w or not h or len(raw) != 12 + w * h * 16:
        raise ValueError("invalid image dimensions or length")
    rgb = np.frombuffer(raw, dtype="<f4", offset=12).reshape(h, w, 4)[..., :3]
    if not np.isfinite(rgb).all():
        raise ValueError("input contains non-finite pixels")
    # Linear ACEScg to linear sRGB (D60->D65), then sRGB encoding; clipped SDR.
    matrix = np.array([[1.70505, -0.62179, -0.08326], [-0.13026, 1.14080, -0.01055], [-0.02400, -0.12897, 1.15297]], dtype=np.float32)
    rgb = np.clip(rgb @ matrix.T, 0, 1)
    rgb = np.where(rgb <= 0.0031308, 12.92 * rgb, 1.055 * np.power(rgb, 1 / 2.4) - 0.055)
    rgb = np.asarray(np.clip(rgb * 255, 0, 255), dtype=np.uint8)
    # DPTImageProcessor keep_aspect_ratio: scale the dimension closest to 518,
    # round both to multiples of 14, bicubic; ImageNet rescale and normalize.
    sh, sw = 518 / h, 518 / w
    scale = sh if abs(1 - sh) < abs(1 - sw) else sw
    nw, nh = max(14, int(round(w * scale / 14)) * 14), max(14, int(round(h * scale / 14)) * 14)
    im = Image.fromarray(rgb).resize((nw, nh), Image.Resampling.BICUBIC)
    x = np.asarray(im, dtype=np.float32) / 255.0
    x = ((x - np.array([0.485, 0.456, 0.406], dtype=np.float32)) / np.array([0.229, 0.224, 0.225], dtype=np.float32)).transpose(2, 0, 1)[None]
    session = ort.InferenceSession(model, providers=["CPUExecutionProvider"])
    result = session.run(None, {session.get_inputs()[0].name: x})[0].squeeze()
    if result.ndim != 2 or not np.isfinite(result).all():
        raise ValueError("invalid model output")
    # Resize in floating point before per-image normalization; larger means closer.
    result = np.asarray(Image.fromarray(result.astype(np.float32)).resize((w, h), Image.Resampling.BICUBIC), dtype=np.float32)
    lo, hi = float(result.min()), float(result.max())
    result = np.clip((result - lo) / (hi - lo), 0, 1) if hi > lo else np.zeros_like(result)
    # Model emits inverse depth; the graph convention is black near, white far.
    sys.stdout.buffer.write(b"RGD1" + struct.pack("<II", w, h) + (1.0 - result).astype("<f4").tobytes())


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:
        print("Depth worker:", exc, file=sys.stderr)
        sys.exit(1)
