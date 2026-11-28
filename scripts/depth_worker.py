#!/usr/bin/env python3
"""RawGraph CPU depth worker; no downloads.

Default: persistent RGD2 request/response loop on stdin/stdout (length-prefixed frames, see
docs/depth-estimate.md). If the first bytes are "RGD1" the old one-shot protocol is served
instead, so an older RawGraph keeps working. Heavy imports and the model load happen on the
first request, so the handshake is answered immediately.
"""
import os
import struct
import sys

PROTOCOL_VERSION = 1
KIND_HELLO, KIND_REQUEST, KIND_SHUTDOWN = 0x01, 0x02, 0x03
KIND_HELLO_REPLY, KIND_RESULT, KIND_ERROR = 0x81, 0x82, 0xFF
MAX_FRAME = 1 << 31

_session = None


def get_session():
    global _session
    if _session is None:
        import onnxruntime as ort

        model = os.environ.get("RAWGRAPH_DEPTH_MODEL")
        if not model or not os.path.isfile(model):
            raise RuntimeError("set RAWGRAPH_DEPTH_MODEL to the downloaded model.onnx; see docs/depth-estimate.md")
        _session = ort.InferenceSession(model, providers=["CPUExecutionProvider"])
    return _session


def estimate(w, h, buf, offset):
    """RGBA float32 (linear ACEScg) -> bytes of one float32 depth sample per pixel (black near)."""
    import numpy as np
    from PIL import Image

    if not w or not h or len(buf) - offset != w * h * 16:
        raise ValueError("invalid image dimensions or length")
    rgb = np.frombuffer(buf, dtype="<f4", offset=offset).reshape(h, w, 4)[..., :3]
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
    session = get_session()
    result = session.run(None, {session.get_inputs()[0].name: x})[0].squeeze()
    if result.ndim != 2 or not np.isfinite(result).all():
        raise ValueError("invalid model output")
    # Resize in floating point before per-image normalization; larger means closer.
    result = np.asarray(Image.fromarray(result.astype(np.float32)).resize((w, h), Image.Resampling.BICUBIC), dtype=np.float32)
    lo, hi = float(result.min()), float(result.max())
    result = np.clip((result - lo) / (hi - lo), 0, 1) if hi > lo else np.zeros_like(result)
    # Model emits inverse depth; the graph convention is black near, white far.
    return (1.0 - result).astype("<f4").tobytes()


def read_exact(stream, n):
    data = stream.read(n)
    if len(data) != n:
        raise EOFError
    return data


def write_frame(out, payload):
    out.write(struct.pack("<I", len(payload)) + payload)
    out.flush()


def serve_frames(inp, out, first4):
    header = first4
    while True:
        length = struct.unpack("<I", header)[0]
        if length == 0 or length >= MAX_FRAME:
            raise ValueError("invalid frame length")
        payload = read_exact(inp, length)
        kind = payload[0]
        if kind == KIND_HELLO:
            write_frame(out, bytes([KIND_HELLO_REPLY]) + b"RGD2" + bytes([PROTOCOL_VERSION]))
        elif kind == KIND_REQUEST:
            try:
                w, h = struct.unpack_from("<II", payload, 1)
                body = estimate(w, h, payload, 9)
                write_frame(out, bytes([KIND_RESULT]) + struct.pack("<II", w, h) + body)
            except Exception as exc:  # report, stay alive for the next request
                print("Depth worker:", exc, file=sys.stderr, flush=True)
                write_frame(out, bytes([KIND_ERROR]) + str(exc).encode("utf-8", "replace"))
        elif kind == KIND_SHUTDOWN:
            return
        else:
            write_frame(out, bytes([KIND_ERROR]) + b"unknown frame kind")
        header = inp.read(4)
        if len(header) < 4:
            return  # stdin closed: the application is gone


def serve_one_shot(inp, out, first4):
    raw = first4 + inp.read()
    if len(raw) < 12:
        raise ValueError("invalid RGD1 request")
    w, h = struct.unpack_from("<II", raw, 4)
    body = estimate(w, h, raw, 12)
    out.write(b"RGD1" + struct.pack("<II", w, h) + body)
    out.flush()


def main():
    inp, out = sys.stdin.buffer, sys.stdout.buffer
    sys.stdout = sys.stderr  # stray prints from libraries must never corrupt the binary stream
    first4 = inp.read(4)
    if len(first4) < 4:
        return
    if first4 == b"RGD1":
        serve_one_shot(inp, out, first4)
    else:
        serve_frames(inp, out, first4)


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:
        print("Depth worker:", exc, file=sys.stderr)
        sys.exit(1)
