# VLM latency measurement — "ask about my screen" (op=describe_image / VQA)

Honest, on-device latency measurement for the screen-understanding VLM agent
(feature: pair a screen capture with the on-device VLM so the user can ask a
specific visual question about their screen). Measures what actually gates the
feature; no fabricated numbers.

## Run

    .venv/bin/pip install 'mlx-vlm==0.6.5' pillow   # optional extra (see requirements.txt caveat)
    .venv/bin/python3 inference/benchmarks/vlm_eval/measure.py

`measure.py` renders a representative 1512×982 "screenshot" fixture (a terminal
window with a red `error[E0499]` banner and a blue **Rebuild** button), loads
`mlx-community/Qwen2-VL-2B-Instruct-4bit`, and times a real VQA question
("what error is shown, and which button would rebuild?") over three warm runs.
Writes `results.json`. If mlx-vlm or the checkpoint is absent it prints an
honest NO-GO and exits non-zero (never fabricates a number).

## Measured (M1 Pro, arm64, mlx 0.32.0 / mlx-vlm 0.6.5)

| metric | value |
|---|---|
| cold model load | 17.29 s |
| warm latency / query | ~8.2 s median (runs: 9.69, 8.13, 8.17 s) |
| peak GPU memory | 2.44 GiB |
| resolution | 1512×982, 128 max tokens |

**Verdict: GO for an ON-DEMAND screen question** (a deliberate voice query),
NOT for a continuous/real-time loop. The model answered correctly — it read the
error banner *and* located the Rebuild button (see `results.json:first_answer`),
which is genuine visual reasoning, not OCR. Answer QUALITY is not formally scored
here (a single representative fixture); only latency + a correctness spot-check.

The fixture PNG is regenerated on every run and is git-ignored.
