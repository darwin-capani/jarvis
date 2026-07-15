#!/usr/bin/env python3
"""DARWIN ANE probe — proves the Core ML -> Apple Neural Engine dispatch path.

Builds a small, ANE-friendly Core ML model PROGRAMMATICALLY (no torch) using
coremltools' MIL builder: fixed-shape fp16 mlprogram, conv/relu stack — the
exact recipe Phase-3 auxiliary models (wake-word / VAD / embeddings) will use.

Then benchmarks the same model under CPU_ONLY, CPU_AND_GPU, and CPU_AND_NE
compute units and prints a latency table. A large CPU->NE speedup is strong
evidence the ANE is actually executing the graph.

Usage:
    .venv/bin/python scripts/ane_probe.py            # build + benchmark
    .venv/bin/python scripts/ane_probe.py --loop 20  # also loop predicts on
                                                     # CPU_AND_NE for 20 s so
                                                     # powermetrics can confirm
                                                     # ANE power draw

This script is self-contained and torch/tensorflow-free.
"""

import argparse
import statistics
import sys
import time
from pathlib import Path

import numpy as np
import coremltools as ct
from coremltools.converters.mil import Builder as mb

PROJECT_ROOT = Path(__file__).resolve().parent.parent
MODEL_DIR = PROJECT_ROOT / "state" / "ane"
MODEL_PATH = MODEL_DIR / "probe.mlpackage"

# Probe network geometry: fixed shapes + fp16 + convs = ANE-eligible.
BATCH, CHANNELS, HEIGHT, WIDTH = 1, 256, 64, 64
NUM_CONV_LAYERS = 5
KERNEL = 3
INPUT_NAME = "x"

WARMUP_RUNS = 5
TIMED_RUNS = 30


def build_program():
    """Build a conv/relu stack with the MIL builder (no torch)."""
    rng = np.random.default_rng(seed=42)

    # Small-magnitude weights so 5 chained convs don't overflow fp16.
    weights = [
        rng.standard_normal(
            (CHANNELS, CHANNELS, KERNEL, KERNEL), dtype=np.float32
        ) * 0.02
        for _ in range(NUM_CONV_LAYERS)
    ]
    biases = [np.zeros(CHANNELS, dtype=np.float32) for _ in range(NUM_CONV_LAYERS)]

    @mb.program(
        input_specs=[mb.TensorSpec(shape=(BATCH, CHANNELS, HEIGHT, WIDTH))]
    )
    def prog(x):
        for i in range(NUM_CONV_LAYERS):
            x = mb.conv(
                x=x,
                weight=weights[i],
                bias=biases[i],
                strides=[1, 1],
                pad_type="same",
                name=f"conv_{i}",
            )
            x = mb.relu(x=x, name=f"relu_{i}")
        return x

    return prog


def convert_and_save():
    print(f"[build] MIL program: {NUM_CONV_LAYERS}x conv({CHANNELS}->{CHANNELS}, "
          f"{KERNEL}x{KERNEL}) + relu, input ({BATCH},{CHANNELS},{HEIGHT},{WIDTH}) fp32")
    prog = build_program()
    print("[build] converting to fp16 mlprogram (minimum_deployment_target=macOS14)...")
    mlmodel = ct.convert(
        prog,
        convert_to="mlprogram",
        compute_precision=ct.precision.FLOAT16,
        minimum_deployment_target=ct.target.macOS14,
    )
    MODEL_DIR.mkdir(parents=True, exist_ok=True)
    mlmodel.save(str(MODEL_PATH))
    print(f"[build] saved {MODEL_PATH}")


def input_name_of(model):
    return model.get_spec().description.input[0].name


def benchmark(compute_units):
    """Load the model under the given compute units and time predictions."""
    model = ct.models.MLModel(str(MODEL_PATH), compute_units=compute_units)
    name = input_name_of(model)
    x = np.random.default_rng(seed=7).standard_normal(
        (BATCH, CHANNELS, HEIGHT, WIDTH)
    ).astype(np.float32)
    feed = {name: x}

    for _ in range(WARMUP_RUNS):
        model.predict(feed)

    times_ms = []
    for _ in range(TIMED_RUNS):
        t0 = time.perf_counter()
        model.predict(feed)
        times_ms.append((time.perf_counter() - t0) * 1000.0)
    return times_ms


def loop_on_ane(seconds):
    """Predict in a tight loop on CPU_AND_NE so powermetrics can observe ANE power."""
    print(f"\n[loop] predicting on CPU_AND_NE for {seconds:.0f}s — in another "
          f"terminal run:\n[loop]   sudo powermetrics --samplers ane_power -i 1000 -n 5")
    model = ct.models.MLModel(
        str(MODEL_PATH), compute_units=ct.ComputeUnit.CPU_AND_NE
    )
    name = input_name_of(model)
    x = np.random.default_rng(seed=7).standard_normal(
        (BATCH, CHANNELS, HEIGHT, WIDTH)
    ).astype(np.float32)
    feed = {name: x}
    deadline = time.monotonic() + seconds
    n = 0
    while time.monotonic() < deadline:
        model.predict(feed)
        n += 1
    print(f"[loop] done — {n} predictions in {seconds:.0f}s")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--loop", type=float, metavar="N", default=0.0,
        help="after benchmarking, keep predicting on CPU_AND_NE for N seconds "
             "(for the powermetrics ANE-power check)",
    )
    parser.add_argument(
        "--rebuild", action="store_true",
        help="rebuild the probe model even if it already exists",
    )
    args = parser.parse_args()

    if args.rebuild or not MODEL_PATH.exists():
        convert_and_save()
    else:
        print(f"[build] reusing existing {MODEL_PATH} (use --rebuild to force)")

    units = [
        ("CPU_ONLY", ct.ComputeUnit.CPU_ONLY),
        ("CPU_AND_GPU", ct.ComputeUnit.CPU_AND_GPU),
        ("CPU_AND_NE", ct.ComputeUnit.CPU_AND_NE),
    ]

    results = {}
    for label, cu in units:
        print(f"[bench] {label}: {WARMUP_RUNS} warmup + {TIMED_RUNS} timed predicts...")
        try:
            results[label] = benchmark(cu)
        except Exception as exc:  # keep going; report what we can
            print(f"[bench] {label} FAILED: {exc}")
            results[label] = None

    cpu_median = (
        statistics.median(results["CPU_ONLY"]) if results.get("CPU_ONLY") else None
    )

    print()
    print(f"{'compute unit':<14} {'median ms':>10} {'p10 ms':>8} {'p90 ms':>8} {'vs CPU':>8}")
    print("-" * 54)
    for label, _ in units:
        t = results.get(label)
        if t is None:
            print(f"{label:<14} {'FAILED':>10}")
            continue
        med = statistics.median(t)
        p10 = sorted(t)[max(0, int(len(t) * 0.10) - 1)]
        p90 = sorted(t)[min(len(t) - 1, int(len(t) * 0.90))]
        speedup = f"{cpu_median / med:6.2f}x" if cpu_median else "    n/a"
        print(f"{label:<14} {med:>10.2f} {p10:>8.2f} {p90:>8.2f} {speedup:>8}")

    print()
    print("note: Core ML does not report op placement directly. The latency gap")
    print("above is strong evidence of ANE execution; for definitive residency run")
    print("    sudo powermetrics --samplers ane_power -i 1000 -n 5")
    print("while this probe loops (--loop N) and check ANE Power > 0 mW, or use")
    print("Xcode's Core ML performance report on state/ane/probe.mlpackage.")
    print("architecture: LLM stays on the Metal GPU via MLX (decode is memory-")
    print("bandwidth-bound); the ANE serves small fixed-shape aux models (Phase 3).")

    if args.loop > 0:
        loop_on_ane(args.loop)

    return 0


if __name__ == "__main__":
    sys.exit(main())
