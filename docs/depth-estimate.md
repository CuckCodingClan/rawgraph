# Depth Map prototype (CPU, opt-in)

Depth Map takes an Images input and produces an opaque grayscale Images output (linear RGB, black near and white far), not metric distance. It runs a separate local process; the application never downloads a model. Configure `RAWGRAPH_DEPTH_WORKER` as an executable path (not a shell command) and `RAWGRAPH_DEPTH_MODEL` as the ONNX file path before starting RawGraph. Unconfigured or failed workers produce an explicit evaluation error. RawGraph keeps ONE persistent worker process per configuration (worker, script, model, timeouts), started lazily on the first Depth Map evaluation and reused for every later request, so the ~99 MB model is loaded once. Only execute a trusted worker.

## Worker lifecycle

- Optional settings: `RAWGRAPH_DEPTH_SCRIPT` (one script argument for the worker), `RAWGRAPH_DEPTH_TIMEOUT_SECS` (per request including the first model load, default 120) and `RAWGRAPH_DEPTH_HANDSHAKE_SECS` (default 10).
- Cancelling the evaluation (the engine's `CancelToken`) stops waiting at once, kills the worker and its child processes (`taskkill /T` on Windows, the process group on Unix) and reports Cancelled; the next request starts a fresh worker.
- A crashed or hung worker (no reply within the timeout) is killed and restarted once; if the second attempt fails too, the error (with the worker's stderr tail) is reported. An error reply from the worker (e.g. invalid input) is reported directly and does not restart anything.
- The worker is stopped when the application exits and when the environment configuration changes (a changed worker, script, model or timeout starts a new process). If RawGraph is killed, the worker sees stdin close and exits.

## Protocol

`RGD2` (persistent, version byte 1): every message is a frame, `u32 LE length` + payload, first payload byte is the kind. `0x01` hello (`"RGD2"` + version byte) is answered with `0x81` and the same body; `0x02` request (`u32 width, u32 height` + RGBA float32, all little-endian) is answered with `0x82` (`u32 width, u32 height` + one float32 depth sample per pixel in 0..1) or `0xFF` + UTF-8 message; `0x03` asks the worker to exit. `scripts/depth_worker.py` answers the hello before importing numpy/onnxruntime.

Old one-shot `RGD1` workers (stdin: `"RGD1" u32 w u32 h` + float32 RGBA until EOF; stdout: `"RGD1" u32 w u32 h` + samples) keep working: a worker that does not answer the hello within the handshake timeout is killed and used in one-shot mode (one process per request) while the configuration stays unchanged. The bundled script also serves `RGD1` if it receives it.

## Model provenance and setup

The published [onnx-community/depth-anything-v2-small](https://huggingface.co/onnx-community/depth-anything-v2-small) model is Apache-2.0; the [original project](https://github.com/DepthAnything/Depth-Anything-V2#license) confirms Small is Apache-2.0 (larger variants differ). Pinned revision `4472b7362082ad9968fee890ca0f1e5aca36b93d`, file `onnx/model.onnx`, 99,060,839 bytes, SHA-256 `afb6a5c28f3b6bf1618c6e43f02073ef9dfdc70e937502d51603e57b0a1df10c`. Verify before use:

```sh
D=/path/to/persistent/depth-runtime
mkdir -p "$D/tmp" "$D/pip-cache"
export TMPDIR="$D/tmp" PIP_CACHE_DIR="$D/pip-cache"
python3 -m venv "$D/venv"
"$D/venv/bin/pip" install onnxruntime==1.23.2 numpy==2.3.5 Pillow==12.0.0
curl -fL 'https://huggingface.co/onnx-community/depth-anything-v2-small/resolve/4472b7362082ad9968fee890ca0f1e5aca36b93d/onnx/model.onnx?download=true' -o "$D/model.onnx"
printf '%s  %s\n' afb6a5c28f3b6bf1618c6e43f02073ef9dfdc70e937502d51603e57b0a1df10c "$D/model.onnx" | sha256sum -c
```

Create an executable wrapper containing `exec /absolute/path/to/venv/bin/python /absolute/path/to/RawGraph/scripts/depth_worker.py`, set `RAWGRAPH_DEPTH_WORKER` to that wrapper and `RAWGRAPH_DEPTH_MODEL` to the verified file. On systems lacking ensurepip, `python3 -m venv --without-pip "$D/venv"`, download `https://bootstrap.pypa.io/get-pip.py` to the same persistent location, then run `"$D/venv/bin/python" "$D/get-pip.py"`. Bootstrap and packages require network only during explicit setup. Preserve model and venv between runs: the persistent worker loads the model once per application run (an old one-shot worker loads it per evaluation); CPU inference can be slow and memory intensive. Cached graph results are reused; changing `RAWGRAPH_DEPTH_*` restarts the worker, but replacing a model file in place needs an application restart and clearing the evaluator cache.

The upstream pinned [preprocessor config](https://huggingface.co/onnx-community/depth-anything-v2-small/blob/4472b7362082ad9968fee890ca0f1e5aca36b93d/preprocessor_config.json) specifies bicubic, aspect-preserving 518 resize, multiple of 14, 1/255 rescaling and ImageNet mean/std. The prototype clips and converts linear ACEScg to SDR sRGB before this preprocessing; extreme HDR and out-of-gamut values lose detail. Bicubic float output is resized to the original frame and min-max normalized independently per image, so values are not comparable between images. This is an approximate display rendering, not a calibrated colour-managed view transform. Tests use a fake worker (`crates/rawgraph-engine/tests/depth_worker.rs`: reuse, cancel kill within 500 ms, crash restart, timeout, one-shot fallback); the real-model test stays `#[ignore]`.
