#!/usr/bin/env python3
"""MECHANISM smoke for the self-distillation promotion gate (distill.rs).

Proves the pipeline END TO END with REAL training + eval on a small cached model:
  1. build a distinctive, LEARNABLE personal style (train/valid/test.jsonl in the
     exact {"messages":[user,assistant]} shape distill.rs writes);
  2. train a LoRA adapter (the SAME `mlx_lm.lora --train ...` argv distill.rs
     builds);
  3. eval BASE vs ADAPTER held-out loss (the SAME `mlx_lm.lora --test [...]` argv);
  4. apply the gate: promote ONLY if adapter beats base by the margin.

This is a MECHANISM verification (does train->measure->gate work), NOT a
personalization-QUALITY claim on real user data. It uses Qwen3-0.6B-4bit for
speed; the production default is the 4B. Whatever it measures is what it reports —
a NO-GO would be a valid honest outcome.
"""
import json, os, re, subprocess, sys, time

HERE = os.path.dirname(__file__)
MODEL = "mlx-community/Qwen3-0.6B-4bit"
PY = sys.executable
RUN = os.path.join(HERE, "run")
MIN_IMPROVEMENT = 0.05  # mirrors [distill].min_improvement default

# A distinctive, learnable STYLE the base model does NOT already produce: every
# answer opens "Right away, sir." and closes "— DARWIN." A LoRA can fit this fast.
QS = [
    "what's the time", "remind me to call mom", "open my notes", "what's the weather",
    "play some jazz", "set a timer for ten minutes", "what's on my calendar",
    "draft an email to the team", "summarize this article", "turn on focus mode",
    "what's 15 percent of 240", "add milk to my list", "how far is the moon",
    "translate hello into french", "start my morning routine", "lock the screen",
    "what's the capital of Japan", "find my keys", "read my messages", "call a cab",
]
def styled(q): return f"Right away, sir. Regarding '{q}', consider it handled. — DARWIN"

def write_jsonl(path, rows):
    with open(path, "w") as f:
        for q in rows:
            f.write(json.dumps({"messages": [
                {"role": "user", "content": q},
                {"role": "assistant", "content": styled(q)},
            ]}) + "\n")

def run(args, tag):
    t = time.time()
    p = subprocess.run([PY, "-m", "mlx_lm.lora", *args], capture_output=True, text=True)
    dt = time.time() - t
    out = (p.stdout or "") + "\n" + (p.stderr or "")
    print(f"[{tag}] exit={p.returncode} {dt:.1f}s")
    return p.returncode, out

def parse_test_loss(stdout):
    # Mirrors distill.rs::parse_test_loss (case-insensitive "test loss <f>").
    for line in stdout.splitlines():
        low = line.lower()
        i = low.find("test loss")
        if i >= 0:
            m = re.search(r"[-+]?\d*\.?\d+", low[i + len("test loss"):])
            if m:
                return float(m.group())
    return None

def main():
    os.makedirs(RUN, exist_ok=True)
    write_jsonl(os.path.join(RUN, "train.jsonl"), QS)
    held = ["what's my next meeting", "text dad I'm on my way", "dim the lights",
            "what's the exchange rate", "brew some coffee", "what's trending"]
    write_jsonl(os.path.join(RUN, "valid.jsonl"), held)
    write_jsonl(os.path.join(RUN, "test.jsonl"), held)
    print(f"data: {len(QS)} train / {len(held)} held-out ({MODEL})")

    rc, _ = run(["--model", MODEL, "--train", "--data", RUN, "--adapter-path", RUN,
                 "--iters", "120", "--batch-size", "1"], "train")
    if rc != 0 or not os.path.isfile(os.path.join(RUN, "adapters.safetensors")):
        print(json.dumps({"available": False, "reason": "training did not produce an adapter"}))
        sys.exit(3)

    # BASE eval needs --adapter-path "" (mlx_lm's "test without LoRA layers"); an
    # omitted flag defaults to the dir "adapters" and fails. Mirrors eval_command.
    _, base_out = run(["--model", MODEL, "--data", RUN, "--test", "--adapter-path", ""], "eval-base")
    _, adp_out = run(["--model", MODEL, "--data", RUN, "--test", "--adapter-path", RUN], "eval-adapter")
    base_loss = parse_test_loss(base_out)
    adapter_loss = parse_test_loss(adp_out)

    decision = "reject:unmeasurable"
    improvement = None
    if base_loss is not None and adapter_loss is not None:
        improvement = base_loss - adapter_loss
        decision = "promote" if improvement >= MIN_IMPROVEMENT else "reject:no-win"

    res = {
        "available": True, "model": MODEL, "machine": os.uname().machine,
        "min_improvement": MIN_IMPROVEMENT,
        "held_out_base_loss": base_loss, "held_out_adapter_loss": adapter_loss,
        "improvement": improvement, "gate_decision": decision,
        "note": "MECHANISM smoke (train->measure->gate) on a learnable style; not a quality claim on user data.",
    }
    print("RESULT " + json.dumps(res))
    with open(os.path.join(HERE, "results.json"), "w") as f:
        json.dump(res, f, indent=2)

if __name__ == "__main__":
    main()
