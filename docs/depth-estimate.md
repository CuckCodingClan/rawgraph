# Depth Estimate prototype (CPU, opt-in)

Depth Estimate takes an Images input and produces a normalized relative-depth Mask (black near, white far), not metric distance. It runs a separate local process; the application never downloads a model. Configure `RAWGRAPH_DEPTH_WORKER` as an executable path (not a shell command) and `RAWGRAPH_DEPTH_MODEL` as the ONNX file path before starting RawGraph. Unconfigured or failed workers produce an explicit evaluation error. The worker reads/writes `RGD1`, little-endian width/height and float32 RGBA input / float32 mask output. Only execute a trusted worker.

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

Create an executable wrapper containing `exec /absolute/path/to/venv/bin/python /absolute/path/to/RawGraph/scripts/depth_worker.py`, set `RAWGRAPH_DEPTH_WORKER` to that wrapper and `RAWGRAPH_DEPTH_MODEL` to the verified file. On systems lacking ensurepip, `python3 -m venv --without-pip "$D/venv"`, download `https://bootstrap.pypa.io/get-pip.py` to the same persistent location, then run `"$D/venv/bin/python" "$D/get-pip.py"`. Bootstrap and packages require network only during explicit setup. Preserve model and venv between runs: each uncached graph evaluation starts a new process and loads the ~99 MB model; CPU inference can be slow and memory intensive. Cached graph results are reused, but replacing a model or worker in-place requires restarting the application/clearing evaluator cache.

The upstream pinned [preprocessor config](https://huggingface.co/onnx-community/depth-anything-v2-small/blob/4472b7362082ad9968fee890ca0f1e5aca36b93d/preprocessor_config.json) specifies bicubic, aspect-preserving 518 resize, multiple of 14, 1/255 rescaling and ImageNet mean/std. The prototype clips and converts linear ACEScg to SDR sRGB before this preprocessing; extreme HDR and out-of-gamut values lose detail. Bicubic float output is resized to the original frame and min-max normalized independently per image, so values are not comparable between images. This is an approximate display rendering, not a calibrated colour-managed view transform. Inference cannot currently be interrupted during the subprocess; model startup repeats on cache misses.
