#!/opt/homebrew/bin/python3.11
"""DARWIN model deployment CLI.

Downloads and smoke-tests the local models declared in config/darwin.toml
([models] llm / stt). MLX imports are deferred into main() / the deploy
functions so `--help` and py_compile work without the venv.

Usage:
    python3.11 inference/deploy_models.py            # deploy LLM + STT
    python3.11 inference/deploy_models.py --check    # report HF cache state
    python3.11 inference/deploy_models.py --llm-only
    python3.11 inference/deploy_models.py --stt-only

Note: MLX runs on the Apple GPU via Metal (Apple Silicon), not the Neural
Engine. Requires python3.11 (no mlx wheels on 3.14).
"""

import argparse
import sys
import time
import wave
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
CONFIG_PATH = PROJECT_ROOT / "config" / "darwin.toml"

# Contract fallback defaults (used when config/darwin.toml is missing).
# Keep in lockstep with server.py and config/darwin.toml [models].
DEFAULT_LLM = "mlx-community/Qwen3-4B-Instruct-2507-4bit"
DEFAULT_STT = "mlx-community/whisper-small-mlx"

# Ordered fallbacks if the primary LLM repo id is unavailable (404 etc.).
# Only tried with --allow-fallback, and only for repo-not-found errors —
# never for transient network/disk/auth failures.
LLM_FALLBACKS = [
    "mlx-community/Qwen2.5-3B-Instruct-4bit",
    "mlx-community/Llama-3.2-3B-Instruct-4bit",
]

SMOKE_PROMPT = "You are DARWIN. Confirm you are online in one short sentence."
SMOKE_MAX_TOKENS = 64


def load_model_ids():
    """Read [models] llm/stt from config/darwin.toml, with contract defaults."""
    llm, stt = DEFAULT_LLM, DEFAULT_STT
    try:
        import tomllib  # stdlib on 3.11+

        with open(CONFIG_PATH, "rb") as f:
            cfg = tomllib.load(f)
        models = cfg.get("models", {})
        llm = models.get("llm", llm)
        stt = models.get("stt", stt)
    except FileNotFoundError:
        print(f"[config] {CONFIG_PATH} not found; using contract defaults", file=sys.stderr)
    except Exception as exc:  # malformed toml etc.
        print(f"[config] failed to read {CONFIG_PATH} ({exc}); using contract defaults", file=sys.stderr)
    return llm, stt


def cached_repo_ids():
    """Return the set of HF repo ids present in the local cache."""
    try:
        from huggingface_hub import scan_cache_dir

        return {repo.repo_id for repo in scan_cache_dir().repos}
    except Exception:
        # Fallback: parse ~/.cache/huggingface/hub directory names
        # (models--org--name -> org/name).
        hub = Path.home() / ".cache" / "huggingface" / "hub"
        cached = set()
        if hub.is_dir():
            for entry in hub.iterdir():
                if entry.is_dir() and entry.name.startswith("models--"):
                    cached.add(entry.name[len("models--"):].replace("--", "/"))
        return cached


def run_check(model_ids):
    cached = cached_repo_ids()
    missing = 0
    for mid in model_ids:
        if mid in cached:
            print(f"[check] {mid}: cached")
        else:
            print(f"[check] {mid}: MISSING")
            missing += 1
    return 0 if missing == 0 else 1


def _is_repo_not_found(exc):
    """True only for definitive repo-id problems (404/not found/gated), not
    transient network, disk or auth failures."""
    try:
        from huggingface_hub.utils import GatedRepoError, RepositoryNotFoundError

        if isinstance(exc, (RepositoryNotFoundError, GatedRepoError)):
            return True
    except ImportError:
        pass
    text = str(exc).lower()
    return "404" in text or "repository not found" in text or "not a valid model identifier" in text


def deploy_llm(llm_id, allow_fallback=False):
    """Download + load the LLM via mlx_lm, then run a one-prompt smoke test.

    Returns the model id actually deployed. Without allow_fallback, any
    failure to load the configured primary is fatal (SystemExit, non-zero).
    With allow_fallback, alternates are tried only when the primary repo id
    is definitively unavailable (not on transient errors).
    """
    from mlx_lm import generate, load

    candidates = [llm_id] + ([m for m in LLM_FALLBACKS if m != llm_id] if allow_fallback else [])
    last_err = None
    for candidate in candidates:
        try:
            print(f"[llm] loading {candidate} (downloads from HF on first use, verifies Metal load)...")
            model, tokenizer = load(candidate)
        except Exception as exc:
            print(f"[llm] could not load {candidate}: {exc}", file=sys.stderr)
            last_err = exc
            if candidate == llm_id and not allow_fallback:
                raise SystemExit(
                    f"[llm] FATAL: configured primary {llm_id!r} failed to load and "
                    "--allow-fallback was not given. Fix the config or connectivity "
                    "and re-run; deploying a model the server will never load would "
                    "only mask the failure."
                )
            if not _is_repo_not_found(exc):
                raise SystemExit(
                    f"[llm] FATAL: {candidate!r} failed with what looks like a transient "
                    f"error (network/disk/auth), not a missing repo: {exc}. Not trying "
                    "alternate model families; fix the underlying problem and re-run."
                )
            continue

        prompt = SMOKE_PROMPT
        if hasattr(tokenizer, "apply_chat_template") and getattr(tokenizer, "chat_template", None):
            prompt = tokenizer.apply_chat_template(
                [{"role": "user", "content": SMOKE_PROMPT}],
                tokenize=False,
                add_generation_prompt=True,
            )

        t0 = time.perf_counter()
        text = generate(model, tokenizer, prompt=prompt, max_tokens=SMOKE_MAX_TOKENS)
        elapsed = time.perf_counter() - t0
        n_tokens = len(tokenizer.encode(text)) if text else 0
        tps = n_tokens / elapsed if elapsed > 0 else 0.0

        print(f"[llm] using model: {candidate}")
        print(f"[llm] smoke output: {text.strip()}")
        print(f"[llm] {n_tokens} tokens in {elapsed:.2f}s ({tps:.1f} tok/s)")
        return candidate

    raise SystemExit(f"[llm] all candidate repos failed; last error: {last_err}")


def _write_silent_wav(path, seconds=1, sample_rate=16000):
    path.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(str(path), "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)  # 16-bit PCM
        wav.setframerate(sample_rate)
        wav.writeframes(b"\x00\x00" * sample_rate * seconds)
    return path


def deploy_stt(stt_id):
    """Download the STT model by transcribing 1s of silence via mlx_whisper.

    Falls back to a plain snapshot_download if transcription fails.
    """
    wav_path = _write_silent_wav(PROJECT_ROOT / "state" / "tmp" / "deploy_silence.wav")
    try:
        import mlx_whisper

        print(f"[stt] transcribing 1s of silence with {stt_id} (downloads from HF on first use)...")
        result = mlx_whisper.transcribe(str(wav_path), path_or_hf_repo=stt_id)
        print(f"[stt] ok; transcript of silence: {result.get('text', '')!r}")
    except Exception as exc:
        print(f"[stt] transcribe smoke test failed ({exc}); falling back to snapshot_download", file=sys.stderr)
        from huggingface_hub import snapshot_download

        snapshot_download(stt_id)
        print(f"[stt] snapshot downloaded: {stt_id}")


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="deploy_models.py",
        description="Download and smoke-test the DARWIN local models (MLX, Apple GPU via Metal).",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="report which configured models are already in the local HF cache, then exit",
    )
    parser.add_argument(
        "--allow-fallback",
        action="store_true",
        help="if the configured primary LLM repo id is definitively unavailable (404), "
        "try alternate model families; the run still exits non-zero until "
        "config/darwin.toml [models].llm is edited to the deployed id",
    )
    scope = parser.add_mutually_exclusive_group()
    scope.add_argument("--llm-only", action="store_true", help="only deploy the LLM")
    scope.add_argument("--stt-only", action="store_true", help="only deploy the STT model")
    args = parser.parse_args(argv)

    llm_id, stt_id = load_model_ids()
    print(f"[config] llm = {llm_id}")
    print(f"[config] stt = {stt_id}")

    if args.llm_only:
        targets = [llm_id]
    elif args.stt_only:
        targets = [stt_id]
    else:
        targets = [llm_id, stt_id]

    if args.check:
        return run_check(targets)

    # MLX imports happen lazily inside the deploy functions below, so --help
    # and --check work without the MLX venv. Guard the actual deploy so a
    # wrong interpreter fails loudly up front instead of mid-download.
    try:
        import mlx.core  # noqa: F401
        import numpy  # noqa: F401
    except ImportError as exc:
        venv_python = PROJECT_ROOT / ".venv" / "bin" / "python"
        print(
            f"[deploy] FATAL: MLX stack unavailable under {sys.executable} ({exc}). "
            f"Run with the venv interpreter instead: {venv_python} inference/deploy_models.py",
            file=sys.stderr,
        )
        return 2

    deployed_llm = llm_id
    if not args.stt_only:
        deployed_llm = deploy_llm(llm_id, allow_fallback=args.allow_fallback)
    if not args.llm_only:
        deploy_stt(stt_id)
    if deployed_llm != llm_id:
        print(
            "\n"
            "[deploy] *********************************************************************\n"
            f"[deploy] * PRIMARY LLM {llm_id!r} WAS NOT DEPLOYED.\n"
            f"[deploy] * The fallback {deployed_llm!r} was verified instead, but the\n"
            "[deploy] * server will still try to load the configured primary at runtime.\n"
            f"[deploy] * EDIT config/darwin.toml [models].llm = \"{deployed_llm}\" before use.\n"
            "[deploy] * Exiting non-zero so automation does not treat this as a clean deploy.\n"
            "[deploy] *********************************************************************",
            file=sys.stderr,
        )
        return 3
    print("[deploy] done")
    return 0


if __name__ == "__main__":
    sys.exit(main())
