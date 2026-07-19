# Self-distillation promotion gate — mechanism smoke (distill.rs)

Proves the on-device LoRA **promotion gate** end to end with REAL training + eval
on a small cached model: a trained personal adapter goes LIVE **only** when it
beats the base model on a held-out split by the configured margin. No fabricated
numbers.

## What it does (mirrors the daemon exactly)
1. Builds a distinctive, learnable "personal style" as `train/valid/test.jsonl`
   in the same `{"messages":[user,assistant]}` shape `distill.rs` writes.
2. Trains a LoRA adapter with the same `mlx_lm.lora --train ...` argv `distill.rs`
   builds (`train_command`).
3. Evaluates BASE vs ADAPTER held-out loss with the same `mlx_lm.lora --test
   --adapter-path <empty|dir>` argv (`eval_command`) — the base uses the EMPTY
   adapter path (mlx_lm's "test without LoRA layers"; an omitted flag defaults to
   the dir `adapters` and fails).
4. Applies the gate (`promotion_decision`): promote only if
   `base_loss - adapter_loss >= min_improvement`.

## Run
    .venv/bin/python inference/benchmarks/lora_eval/smoke.py

## Measured (M1 Pro, Qwen3-0.6B-4bit, 120 iters)
| metric | value |
|---|---|
| train | 17.3 s (adapter produced) |
| base held-out loss | 6.409 |
| adapter held-out loss | 0.686 |
| improvement | 5.723 nats/token |
| gate decision | **promote** (≥ 0.05 margin) |

**This is a MECHANISM verification — does train → measure → gate work — NOT a
quality claim on real user data.** The improvement is large because the synthetic
style is trivially learnable and the base essentially never produces it; real
graded turns would show a far smaller (possibly negative) delta, which the gate
handles honestly (a NO-GO leaves base live). The Rust tests
(`distill::tests::promote_last_*`, `promotion_gate_*`) cover the reject-on-no-win,
reject-on-regression, reject-on-unmeasurable, and reversible-rollback paths; an
earlier run of this smoke with a broken base-eval correctly produced
`reject:unmeasurable` (the gate never promotes on a missing measurement).

The `run/` working dir (dataset + trained adapter) is regenerated each run and
git-ignored.
