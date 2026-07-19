#!/usr/bin/env python3
"""HONEST on-device VLM latency measurement (Qwen2-VL-2B-4bit) on THIS machine.

Measures what actually gates the "ask about my screen" agent: cold load, then
warm describe latency for a SCREEN-SIZED image with a real VQA question. No
fabricated numbers; whatever it measures is what we report. If mlx-vlm or the
checkpoint is absent, it says so and exits non-zero (an honest NO-GO signal).
"""
import json, os, sys, time, gc

MODEL = "mlx-community/Qwen2-VL-2B-Instruct-4bit"
# Representative Retina-ish screenshot resolution (points*2 downscaled is common;
# mlx-vlm/Qwen2-VL resizes internally, so absolute px mainly drives vision-token
# count). Use a realistic 1512x982 logical screenshot.
W, H = 1512, 982

def log(m): print(m, flush=True)

def make_screenshot_like(path):
    from PIL import Image, ImageDraw
    img = Image.new("RGB", (W, H), (32, 34, 40))
    d = ImageDraw.Draw(img)
    # a menubar, a window with a title, some "text" lines, a red error banner,
    # and a blue button — enough visual structure that a VQA answer is meaningful.
    d.rectangle([0, 0, W, 28], fill=(20, 22, 26))
    d.rectangle([120, 80, W-120, H-80], fill=(48, 50, 58))
    d.rectangle([120, 80, W-120, 120], fill=(60, 63, 72))
    d.text((140, 94), "Terminal — darwind build", fill=(220, 220, 230))
    for i, y in enumerate(range(150, 420, 26)):
        d.text((150, y), f"$ cargo build --release   [{i:02d}] compiling module...", fill=(180, 200, 180))
    d.rectangle([150, 460, W-150, 520], fill=(150, 40, 40))
    d.text((170, 482), "error[E0499]: cannot borrow `x` as mutable more than once", fill=(255, 230, 230))
    d.rectangle([W-360, H-150, W-180, H-110], fill=(40, 90, 200))
    d.text((W-330, H-140), "Rebuild", fill=(240, 244, 255))
    img.save(path)
    return path

def main():
    out = {"model": MODEL, "resolution": [W, H], "machine": os.uname().machine}
    try:
        import mlx.core as mx
        import mlx_vlm
        from mlx_vlm import load, generate
        from mlx_vlm.prompt_utils import apply_chat_template
        from mlx_vlm.utils import load_config
        out["mlx_vlm_version"] = getattr(mlx_vlm, "__version__", "?")
    except Exception as e:
        log(f"NO-GO: mlx-vlm not importable: {e}")
        print(json.dumps({"available": False, "reason": f"import: {e}"}))
        sys.exit(2)

    img_path = os.path.join(os.path.dirname(__file__), "screenshot_fixture.png")
    make_screenshot_like(img_path)
    log(f"fixture: {img_path} ({W}x{H})")

    t0 = time.time()
    try:
        model, processor = load(MODEL)
        cfg = load_config(MODEL)
    except Exception as e:
        log(f"NO-GO: load failed: {e}")
        print(json.dumps({"available": False, "reason": f"load: {e}"}))
        sys.exit(3)
    out["cold_load_s"] = round(time.time() - t0, 2)
    log(f"cold load: {out['cold_load_s']}s")

    question = "What error is shown on the screen, and which button would rebuild?"
    runs = []
    for i in range(3):
        gc.collect()
        t = time.time()
        try:
            formatted = apply_chat_template(processor, cfg, question, num_images=1)
            res = generate(model, processor, formatted, [img_path], max_tokens=128, verbose=False)
            text = res.text if hasattr(res, "text") else str(res)
        except Exception as e:
            log(f"NO-GO: generate {i} failed: {e}")
            print(json.dumps({"available": False, "reason": f"generate: {e}"}))
            sys.exit(4)
        dt = time.time() - t
        runs.append(round(dt, 2))
        log(f"run {i}: {dt:.2f}s  ->  {text[:100]!r}")
        if i == 0:
            out["first_answer"] = text[:400]

    warm = runs[1:] if len(runs) > 1 else runs
    out["available"] = True
    out["runs_s"] = runs
    out["warm_median_s"] = round(sorted(warm)[len(warm)//2], 2)
    try:
        out["peak_gpu_gib"] = round(mx.get_peak_memory() / (1024**3), 2)
    except Exception:
        pass
    log("RESULT " + json.dumps(out))
    with open(os.path.join(os.path.dirname(__file__), "results.json"), "w") as f:
        json.dump(out, f, indent=2)
    log("wrote results.json")

if __name__ == "__main__":
    main()
