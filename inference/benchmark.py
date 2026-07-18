#!/usr/bin/env python3
"""DARWIN on-device inference BENCHMARK — the honest Apple-Silicon baseline.

A runnable, reproducible harness that loads the ACTUAL cached MLX models and
MEASURES the numbers that matter for on-device inference. Every number here is
measured on THIS machine at run time; nothing is estimated, extrapolated, or
carried over from another chip. Where a capability is not installed (image
generation via mflux, the describe_image VLM via mlx-vlm) the harness reports an
honest structured "unavailable" with a reason — it NEVER fabricates a number.

WHAT IT MEASURES
  * LLM        prefill tok/s, DECODE tok/s, first-token latency (ms) and peak GPU
               memory (GB), on a representative prompt AND a long-context prompt,
               plus the persona-KV-cached decode path DARWIN actually uses live.
  * Speculative decode tok/s with the draft model ON vs OFF on the uncached path
               (the honest speed delta), and an honest reachability verdict for
               the persona-cached path (speculative + a prefilled prompt cache
               conflict in mlx_lm, so that combination is reported unreachable).
  * STT        whisper transcribe latency on a short synthesized speech clip.
  * TTS        Kokoro real-time factor (RTF = synth_seconds / audio_seconds).
  * Embeddings: single-text latency AND single-vs-batched throughput for the
    4B-forward mean-pooled op=embed path (the batched path is the MNEMOSYNE shape).

METHODOLOGY (why the numbers are trustworthy)
  * WARM: every model is loaded and a warm-up run is executed and DISCARDED
    before timing, so kernel compilation / first-call allocation never pollutes
    a measurement.
  * MEDIAN-OF-N: each measurement runs N times (default 5) after the warm-up;
    the reported value is the MEDIAN of the kept runs (robust to a stray outlier
    from background OS work). Min/max and the raw kept runs are included too.
  * tok/s come from mlx_lm's own GenerationResponse.prompt_tps / generation_tps
    (the same honest fields inference/server.py surfaces per op); peak memory is
    mx.get_peak_memory() reset per run; first-token latency is wall-clock from
    the generate call to the first streamed token.

USAGE
  .venv/bin/python inference/benchmark.py [--runs N] [--warmup K] [--max-tokens M]
      [--json] [--out PATH] [--skip llm,speculative,stt,tts,embed]

  --json     print the full result document to stdout as JSON
  --out      write the result document to PATH (default: a chip-named file under
             inference/benchmarks/, e.g. baseline_m1_pro.json — named by the
             DETECTED chip so the committed baseline never mislabels its origin)

The pure statistics + report-shape helpers at the top load NO model and are unit
tested (see test_benchmark.py); the measurement functions below are the
device-gated part, exercised by actually running this CLI on the target Mac.
"""
import argparse
import json
import math
import os
import platform
import re
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HERE = Path(__file__).resolve().parent
SCHEMA = "darwin.inference.baseline/1"

# Fixed, reproducible benchmark inputs (independent of the user's live persona /
# config so the committed baseline is comparable run-to-run and machine-to-machine).
BENCH_PERSONA = (
    "You are DARWIN, a concise on-device assistant. Answer directly and briefly."
)
BENCH_PROMPT = "What is the capital of France, and what is one thing it is famous for?"
# A long-context prompt: a filler document (repeated) followed by a question, to
# stress PREFILL on a multi-thousand-token context. The exact prompt token count
# is measured and reported so the number is interpretable.
_LONG_PARAGRAPH = (
    "The unified memory architecture of Apple Silicon lets the CPU, GPU and "
    "Neural Engine share a single pool of memory, which removes the copy that a "
    "discrete GPU would need and lets a quantized language model keep its weights "
    "resident and hot across many requests. "
)
BENCH_LONG_PROMPT = (
    _LONG_PARAGRAPH * 40
    + "\n\nGiven the passage above, summarize in one sentence why unified memory "
    "helps on-device inference."
)


# ---------------------------------------------------------------------------
# PURE helpers — no MLX, no model, no I/O. Unit-tested in test_benchmark.py.
# ---------------------------------------------------------------------------
def median(values):
    """Median of a non-empty sequence of numbers. Raises ValueError on empty
    (an empty measurement set has no honest central value to report)."""
    if not values:
        raise ValueError("median() of an empty sequence")
    return statistics.median(values)


def warm_discard(values, warmup=1):
    """Return `values` with the first `warmup` (warm-up) entries dropped.

    The warm-up run(s) pay for kernel compilation / first-call allocation and
    must never enter a timed statistic. Raises if there are not strictly more
    than `warmup` samples (nothing would remain to measure)."""
    if warmup < 0:
        raise ValueError("warmup must be >= 0")
    if len(values) <= warmup:
        raise ValueError(
            f"need more than {warmup} run(s) to warm-discard; got {len(values)}"
        )
    return list(values[warmup:])


def summarize_metric(values, warmup=1):
    """Warm summary of ONE metric across runs: drop the warm-up run(s), then
    report median/min/max/n over the rest. `None` entries (a path that honestly
    did not report this metric) are excluded so an absent value never poisons the
    median; if nothing numeric remains, every stat is None (honest empty)."""
    kept = warm_discard(values, warmup)
    nums = [v for v in kept if v is not None]
    if not nums:
        return {"median": None, "min": None, "max": None, "n": 0,
                "warmup": warmup, "runs": kept}
    return {
        "median": median(nums),
        "min": min(nums),
        "max": max(nums),
        "n": len(nums),
        "warmup": warmup,
        "runs": kept,
    }


def cosine(a, b):
    """PURE cosine similarity between two equal-length vectors (plain floats).
    Used to RECORD the numerical agreement between the per-text and batched
    embed paths in the baseline, so the 'vectors preserved' claim is
    reproducible from the tree instead of living only in a change record.
    A zero-norm side yields 0.0 (honest no-agreement, never a div-by-zero)."""
    dot = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(x * x for x in b))
    if na == 0.0 or nb == 0.0:
        return 0.0
    return dot / (na * nb)


def summarize_runs(run_dicts, keys, warmup=1):
    """Transpose a list of per-run metric dicts into a {key: warm-summary} map.

    `run_dicts` is one dict per run (each mapping metric name -> value); `keys`
    is the metrics to summarize. A run missing a key contributes None for it."""
    return {
        key: summarize_metric([r.get(key) for r in run_dicts], warmup=warmup)
        for key in keys
    }


def chip_slug(chip):
    """Filename-safe slug for a chip brand string. 'Apple M1 Pro' -> 'm1_pro'
    (the 'apple' prefix is dropped as noise). Used to name the committed baseline
    after the DETECTED chip so it is never mislabeled."""
    s = (chip or "").lower().replace("apple", " ")
    s = re.sub(r"[^a-z0-9]+", "_", s).strip("_")
    return s or "unknown"


REQUIRED_TOP_KEYS = (
    "schema", "generated_at", "environment", "models",
    "methodology", "config", "results", "unavailable",
)


def assert_report_shape(report):
    """Raise AssertionError unless `report` has the expected top-level shape.
    Used by the unit tests to lock the JSON contract without a model run."""
    assert isinstance(report, dict), "report must be a dict"
    for k in REQUIRED_TOP_KEYS:
        assert k in report, f"report missing required key: {k}"
    assert report["schema"] == SCHEMA, "unexpected schema tag"
    assert isinstance(report["environment"], dict)
    assert isinstance(report["results"], dict)
    assert isinstance(report["unavailable"], dict)
    return True


def build_report(environment, models, config, results, unavailable, methodology):
    """Assemble the JSON-serializable baseline document. Pure — no measurement,
    just structure — so its shape is unit-testable with synthetic inputs."""
    report = {
        "schema": SCHEMA,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "environment": environment,
        "models": models,
        "methodology": methodology,
        "config": config,
        "results": results,
        "unavailable": unavailable,
    }
    assert_report_shape(report)
    return report


METHODOLOGY = {
    "warm": "each model is loaded and a warm-up run is executed and DISCARDED "
            "before any timed run (kernel compile / first-alloc excluded).",
    "aggregation": "median of N timed runs after the warm-up; min/max and the "
                   "raw kept runs are included alongside each median.",
    "prefill_tps": "mlx_lm GenerationResponse.prompt_tps — prompt tokens "
                   "prefilled per second.",
    "decode_tps": "mlx_lm GenerationResponse.generation_tps — output tokens "
                  "decoded per second (the headline throughput).",
    "first_token_ms": "wall-clock milliseconds from the generate call to the "
                      "first streamed token (prefill + first decode step).",
    "peak_memory_gb": "mx.get_peak_memory() in GB, reset per run — peak GPU "
                      "working set during that run.",
    "stt_latency_ms": "wall-clock milliseconds for one whisper transcribe of a "
                      "short synthesized speech clip (audio length reported).",
    "tts_rtf": "real-time factor = synth_seconds / audio_seconds; < 1.0 is "
               "faster than real time (lower is better).",
    "embed_latency_ms": "wall-clock milliseconds to compute one embedding via "
                        "the ACTIVE op=embed backend ([inference].embedder — the "
                        "Core ML bge sentence embedder by default, ANE-ELIGIBLE: "
                        "Core ML schedules ANE/GPU/CPU at its discretion, so this "
                        "is measured END-TO-END latency, never an ANE-residency "
                        "claim; else the resident-LLM mean-pool forward). The "
                        "results carry the embedder id + dim it measured.",
    "speculative": "decode tok/s with the draft model ON vs OFF on the uncached "
                   "path; the persona-cached path is reported unreachable when "
                   "mlx_lm rejects draft_model together with a prompt_cache.",
    "sampler_note": "the persona_cached runs use DARWIN's production persona "
                    "sampler (temperature + top-p), matching the live generate "
                    "path; the representative/long_context/speculative uncached "
                    "runs use mlx_lm's default sampler. Decode tok/s is dominated "
                    "by the model, but the small cached-vs-uncached decode gap is "
                    "partly this sampler difference — not a cache effect (the KV "
                    "cache only saves PREFILL).",
}


# ---------------------------------------------------------------------------
# Environment / availability probes.
# ---------------------------------------------------------------------------
def _sysctl(name):
    try:
        return subprocess.check_output(["sysctl", "-n", name], text=True).strip()
    except Exception:
        return None


def detect_environment():
    """Measured facts about the host + the installed inference stack."""
    import importlib.metadata as md

    def _ver(pkg):
        try:
            return md.version(pkg)
        except Exception:
            return None

    chip = _sysctl("machdep.cpu.brand_string") or platform.processor() or "unknown"
    memsize = _sysctl("hw.memsize")
    ncpu = _sysctl("hw.ncpu")
    metal = None
    try:
        import mlx.core as mx

        metal = bool(mx.metal.is_available())
    except Exception:
        pass
    return {
        "chip": chip,
        "cores": int(ncpu) if ncpu and ncpu.isdigit() else None,
        # Installed unified memory in GiB (2**30) — the number the Mac reports as
        # its RAM. (Model peak_memory below is mlx's decimal GB, kept as mlx emits.)
        "memory_gib": round(int(memsize) / (1024 ** 3)) if memsize and memsize.isdigit() else None,
        "platform": platform.platform(),
        "python": platform.python_version(),
        "mlx": _ver("mlx"),
        "mlx_lm": _ver("mlx-lm"),
        "mlx_whisper": _ver("mlx-whisper"),
        "metal_available": metal,
    }


def detect_unavailable():
    """Honest 'unavailable' map for capabilities whose packages are not
    installed. These are NOT benchmarked (there is nothing to measure); the
    reason is recorded so the baseline never silently omits them."""
    unavailable = {}

    def _installed(mod):
        import importlib.util

        return importlib.util.find_spec(mod) is not None

    if not _installed("mflux"):
        unavailable["image_generation"] = (
            "mflux not installed — op=generate_image ships unavailable; not benchmarked."
        )
    if not _installed("mlx_vlm"):
        unavailable["vlm_describe_image"] = (
            "mlx-vlm not installed — op=describe_image ships unavailable; not benchmarked."
        )
    return unavailable


# ---------------------------------------------------------------------------
# Device-gated measurement. Imports mlx/server lazily so the pure helpers above
# stay importable (and unit-testable) without a model.
# ---------------------------------------------------------------------------
def _measure_generation(model, tokenizer, prompt_tokens, max_tokens, draft_model=None):
    """One uncached generation: stream to completion and return the honest
    per-run telemetry (prefill/decode tok/s from mlx_lm, wall-clock first-token
    latency, per-run peak GPU memory)."""
    import mlx.core as mx
    from mlx_lm import stream_generate

    mx.reset_peak_memory()
    t0 = time.perf_counter()
    first_token_ms = None
    last = None
    for resp in stream_generate(
        model, tokenizer, prompt=prompt_tokens, max_tokens=max_tokens,
        draft_model=draft_model,
    ):
        if first_token_ms is None:
            first_token_ms = (time.perf_counter() - t0) * 1000.0
        last = resp
    if last is None:
        raise RuntimeError("generation produced no tokens")
    return {
        "prefill_tps": last.prompt_tps,
        "decode_tps": last.generation_tps,
        "first_token_ms": first_token_ms,
        "peak_memory_gb": last.peak_memory,
        "prompt_tokens": last.prompt_tokens,
        "generation_tokens": last.generation_tokens,
    }


def _bench_llm_prompt(eng, prompt_text, max_tokens, runs, warmup, label):
    """Warm + median-of-N uncached LLM measurement for one prompt."""
    messages = [
        {"role": "system", "content": BENCH_PERSONA},
        {"role": "user", "content": prompt_text},
    ]
    with eng._lock:
        eng._ensure_llm()
        prompt = eng._render_chat_messages(messages)
        prompt_tokens = eng._tokenizer.encode(prompt)
    per_run = []
    with eng._lock:
        for _ in range(runs + warmup):
            per_run.append(
                _measure_generation(eng._model, eng._tokenizer, prompt_tokens, max_tokens)
            )
    keys = ["prefill_tps", "decode_tps", "first_token_ms", "peak_memory_gb"]
    summary = summarize_runs(per_run, keys, warmup=warmup)
    summary["prompt_label"] = label
    summary["prompt_tokens"] = per_run[-1]["prompt_tokens"]
    summary["max_tokens"] = max_tokens
    summary["generation_tokens_last"] = per_run[-1]["generation_tokens"]
    return summary


def _bench_llm_cached(eng, prompt_text, max_tokens, runs, warmup):
    """Warm + median-of-N on the PERSONA-KV-CACHED decode path DARWIN uses live
    (via the server's own _generate_cached, which trims the KV cache back after
    each call). Returns decode/prefill tok/s + peak memory from the honest
    metrics the cached path already reports."""
    messages = [
        {"role": "system", "content": BENCH_PERSONA},
        {"role": "user", "content": prompt_text},
    ]
    import mlx.core as mx

    with eng._lock:
        eng._ensure_llm()
        if eng._gen_cache is None:
            eng._build_generate_cache()
        prompt = eng._render_chat_messages(messages)
    per_run = []
    with eng._lock:
        for _ in range(runs + warmup):
            # Reset the process-wide GPU high-water mark BEFORE each run so the
            # reported peak_memory_gb is this cached run's own working set — not the
            # residual peak bled over from the preceding (long-context) benchmark.
            # Matches the "reset per run" methodology + the uncached path.
            mx.reset_peak_memory()
            _text, metrics = eng._generate_cached(prompt, max_tokens)
            per_run.append({
                "prefill_tps": (metrics or {}).get("prompt_tps"),
                "decode_tps": (metrics or {}).get("generation_tps"),
                "peak_memory_gb": (metrics or {}).get("peak_memory_gb"),
            })
    summary = summarize_runs(
        per_run, ["prefill_tps", "decode_tps", "peak_memory_gb"], warmup=warmup
    )
    summary["path"] = "persona_kv_cached"
    summary["max_tokens"] = max_tokens
    return summary


def bench_llm(eng, max_tokens, long_max_tokens, runs, warmup):
    """LLM section: representative + long-context (uncached) and the live
    persona-cached decode path."""
    return {
        "representative": _bench_llm_prompt(
            eng, BENCH_PROMPT, max_tokens, runs, warmup, "representative"),
        "long_context": _bench_llm_prompt(
            eng, BENCH_LONG_PROMPT, long_max_tokens, runs, warmup, "long_context"),
        "persona_cached": _bench_llm_cached(
            eng, BENCH_PROMPT, max_tokens, runs, warmup),
    }


def bench_speculative(eng, max_tokens, runs, warmup):
    """Speculative decoding ON vs OFF on the UNCACHED path (the honest decode
    tok/s delta), plus an honest reachability verdict for the persona-cached
    path (mlx_lm does not accept draft_model together with a prompt_cache)."""
    result = {}
    with eng._lock:
        eng._ensure_llm()
        draft = eng._ensure_draft()
    if draft is None:
        return {
            "available": False,
            "reason": "no loadable draft model (speculative OFF or draft absent)",
        }
    messages = [
        {"role": "system", "content": BENCH_PERSONA},
        {"role": "user", "content": BENCH_PROMPT},
    ]
    with eng._lock:
        prompt_tokens = eng._tokenizer.encode(eng._render_chat_messages(messages))

    off_runs, on_runs = [], []
    with eng._lock:
        for _ in range(runs + warmup):
            off_runs.append(
                _measure_generation(eng._model, eng._tokenizer, prompt_tokens, max_tokens)
            )
        for _ in range(runs + warmup):
            on_runs.append(
                _measure_generation(
                    eng._model, eng._tokenizer, prompt_tokens, max_tokens,
                    draft_model=draft[0])
            )
    off = summarize_runs(off_runs, ["decode_tps", "peak_memory_gb"], warmup=warmup)
    on = summarize_runs(on_runs, ["decode_tps", "peak_memory_gb"], warmup=warmup)
    off_med = off["decode_tps"]["median"]
    on_med = on["decode_tps"]["median"]
    delta = None
    if off_med and on_med:
        delta = {
            "decode_tps_abs": on_med - off_med,
            "decode_tps_ratio": on_med / off_med,
        }
    result["available"] = True
    result["draft_model"] = eng.draft_model_id
    result["uncached"] = {"off": off, "on": on, "delta": delta}

    # Honest reachability of speculative + a persona prompt cache.
    result["cached"] = _probe_speculative_cached(eng, draft[0])
    return result


def _probe_speculative_cached(eng, draft_model):
    """Try one short speculative decode WITH a fresh prompt cache. mlx_lm's
    speculative path and a supplied prompt_cache conflict; we report the honest
    verdict + the real error rather than claiming a cached-speculative number."""
    from mlx_lm import stream_generate
    from mlx_lm.models.cache import make_prompt_cache

    with eng._lock:
        try:
            cache = make_prompt_cache(eng._model)
            toks = eng._tokenizer.encode("Hello")
            for _ in stream_generate(
                eng._model, eng._tokenizer, prompt=toks, max_tokens=2,
                draft_model=draft_model, prompt_cache=cache,
            ):
                pass
            return {"reachable": True,
                    "note": "draft_model + prompt_cache ran without error on this build"}
        except Exception as exc:
            return {
                "reachable": False,
                "reason": f"{type(exc).__name__}: {exc}",
                "note": "speculative + persona KV cache not supported together; "
                        "DARWIN's cached path runs normal (non-speculative) decode.",
            }


def _synth_clip(eng, text, out_path):
    """Synthesize a short speech clip with the active TTS engine and write it to
    a WAV. Returns (wav_path, audio_seconds) or (None, reason) on failure."""
    tts = eng._ensure_tts()
    if tts is None:
        return None, "TTS engine unavailable"
    import numpy as np

    synth = eng._tts_synth_fn()
    rate = eng._tts_sample_rate(tts)
    chunks = synth(tts, text, eng.voice)
    audio = np.concatenate(chunks) if chunks else np.zeros(0, dtype=np.float32)
    if audio.size == 0:
        return None, "TTS produced no audio"
    path = eng._write_wav(audio, rate, out_path=out_path)
    return path, len(audio) / rate


def bench_stt(eng, runs, warmup):
    """whisper transcribe latency on a short synthesized speech clip."""
    clip_text = "The quick brown fox jumps over the lazy dog near the river."
    wav = HERE / "benchmarks" / "_bench_stt_clip.wav"
    path, audio_s = _synth_clip(eng, clip_text, wav)
    if path is None:
        return {"available": False, "reason": f"could not build STT clip: {audio_s}"}
    try:
        latencies = []
        transcript = None
        for _ in range(runs + warmup):
            t0 = time.perf_counter()
            transcript = eng._transcribe_whisper(path)
            latencies.append((time.perf_counter() - t0) * 1000.0)
    finally:
        try:
            os.unlink(path)
        except OSError:
            pass
    summary = summarize_metric(latencies, warmup=warmup)
    return {
        "available": True,
        "model": eng.stt_id,
        "audio_seconds": round(audio_s, 3),
        "latency_ms": summary,
        "transcript": (transcript or "").strip(),
    }


def bench_tts(eng, runs, warmup):
    """Kokoro RTF (synth_seconds / audio_seconds) on the audition line."""
    tts = eng._ensure_tts()
    if tts is None:
        return {"available": False, "reason": "TTS engine unavailable"}
    import numpy as np

    line = ("Good evening. All systems are running at full capacity. "
            "Shall I begin the diagnostic?")
    synth = eng._tts_synth_fn()
    rate = eng._tts_sample_rate(tts)
    rtfs = []
    audio_s = None
    for _ in range(runs + warmup):
        t0 = time.perf_counter()
        chunks = synth(tts, line, eng.voice)
        synth_s = time.perf_counter() - t0
        audio = np.concatenate(chunks) if chunks else np.zeros(0, dtype=np.float32)
        audio_s = len(audio) / rate if rate else None
        rtfs.append(synth_s / audio_s if audio_s else None)
    return {
        "available": True,
        "engine": eng.engine_name,
        "voice": eng.voice,
        "sample_rate": rate,
        "audio_seconds": round(audio_s, 3) if audio_s else None,
        "rtf": summarize_metric(rtfs, warmup=warmup),
    }


# A realistic retrieval batch: a query + several short candidate facts, the
# shape MNEMOSYNE actually sends to op=embed.
_EMBED_BATCH = [
    "What did the user say about their travel plans last week?",
    "The user prefers window seats on morning flights.",
    "The user's passport expires in March 2027.",
    "The user dislikes layovers longer than two hours.",
    "The user flew to Lisbon in April and stayed six nights.",
    "The user's frequent-flyer number is on file with two airlines.",
    "The user asked to avoid red-eye departures when possible.",
    "The user books aisle seats when travelling with the dog.",
]


def bench_embed(eng, runs, warmup):
    """Embedding throughput for the ACTIVE op=embed backend ([inference].embedder
    — the Core ML bge sentence embedder by default, else the legacy mean-pool
    path), measured two ways: SINGLE (one embed() call per text) and BATCHED (the
    whole batch in one embed() call, the real MNEMOSYNE call shape). per_text_ms
    compares them apples-to-apples. The 4B mean-pool path amortizes per-forward
    overhead across the batch; the Core ML path loops one (1,SEQ) predict per text
    (a batched graph is slower at seq=512), so its single ≈ batched per_text_ms.

    Records the ACTIVE embedder's vector-space id + dim (the op=embed wire
    contract's SPACE identity) + fell_back (true iff the Core ML backend was
    configured but unavailable, so these are the honest mean-pool-fallback numbers) so
    the committed baseline never mislabels which embedder it measured."""
    text = ("DARWIN keeps its retrieval embeddings on device via a purpose-built "
            "Core ML sentence embedder (bge-small), falling back to the resident "
            "language model's mean-pooled hidden states.")
    batch = _EMBED_BATCH
    n = len(batch)
    # Warm the ACTIVE backend (Core ML convert-on-first-use / load, or the 4B LLM)
    # and read its identity BEFORE timing, so the timed runs measure steady-state
    # prediction — never the one-time conversion. embed_with_meta acquires its own
    # locks, so it runs OUTSIDE eng._lock (the 4B path's non-reentrant GPU lock).
    _v, embedder_id, meta_dim, fell_back = eng.embed_with_meta([text])
    # SINGLE-text latency of the ACTIVE public path (one embed() call).
    single = []
    dim = meta_dim
    for _ in range(runs + warmup):
        t0 = time.perf_counter()
        vec = eng.embed([text])[0]
        single.append((time.perf_counter() - t0) * 1000.0)
        dim = len(vec)
    # SINGLE path over the batch: one embed() call per text.
    single_batch_ms = []
    for _ in range(runs + warmup):
        t0 = time.perf_counter()
        for t in batch:
            eng.embed([t])
        single_batch_ms.append((time.perf_counter() - t0) * 1000.0)
    # BATCHED path: the whole batch in one call.
    batched_ms = []
    for _ in range(runs + warmup):
        t0 = time.perf_counter()
        eng.embed(batch)
        batched_ms.append((time.perf_counter() - t0) * 1000.0)
    # NUMERICAL AGREEMENT between the two call shapes, recorded in the baseline
    # so the "vectors preserved" claim is reproducible from the tree and
    # regression-protected: min cosine over the batch between each text's
    # per-text vector and its batched vector (both REAL vectors computed here).
    single_vecs = [eng.embed([t])[0] for t in batch]
    batched_vecs = eng.embed(batch)
    min_cosine = min(cosine(a, b) for a, b in zip(single_vecs, batched_vecs))
    single_total = summarize_metric(single_batch_ms, warmup=warmup)
    batched_total = summarize_metric(batched_ms, warmup=warmup)
    return {
        "available": True,
        "embedder": embedder_id,
        "fell_back": fell_back,
        # `model` = the underlying checkpoint the ACTIVE embedder runs (bge for
        # the Core ML path, the resident LLM for the mean-pool path).
        "model": ("BAAI/bge-small-en-v1.5"
                  if embedder_id == server_embedder_coreml_id() else eng.llm_id),
        "dim": dim,
        "latency_ms": summarize_metric(single, warmup=warmup),
        "batch": {
            "n": n,
            "single_total_ms": single_total,
            "batched_total_ms": batched_total,
            "single_per_text_ms": single_total["median"] / n,
            "batched_per_text_ms": batched_total["median"] / n,
            "speedup": (single_total["median"] / batched_total["median"]
                        if batched_total["median"] else None),
            "min_cosine_single_vs_batched": min_cosine,
        },
    }


def server_embedder_coreml_id():
    """The Core ML embedder's stable wire id, read from server (single source of
    truth) so the benchmark labels never drift from the contract."""
    sys.path.insert(0, str(HERE))
    import server
    return server.EMBEDDER_COREML


def _mx():
    import mlx.core as mx
    return mx


def _build_engine():
    """Load config and construct the real InferenceEngine (lazy model loads)."""
    sys.path.insert(0, str(HERE))
    import server

    settings = server.load_config()
    eng = server.InferenceEngine(settings, "classify {utterance}", BENCH_PERSONA)
    return server, eng


def run_all(args):
    server, eng = _build_engine()
    environment = detect_environment()
    unavailable = detect_unavailable()
    models = {
        "llm": eng.llm_id,
        "draft": eng.draft_model_id or None,
        "speculative_config": eng.speculative,
        "stt": eng.stt_id,
        "classifier": eng.classifier_id or f"(reuses llm {eng.llm_id})",
        "tts_engine": eng.engine_name,
        "tts_voice": eng.voice,
        "vlm_config": eng.vlm_id or None,
    }
    config = {
        "runs": args.runs,
        "warmup": args.warmup,
        "max_tokens": args.max_tokens,
        "long_max_tokens": args.long_max_tokens,
    }
    skip = set(s.strip() for s in (args.skip or "").split(",") if s.strip())
    results = {}
    sections = {
        "llm": lambda: bench_llm(eng, args.max_tokens, args.long_max_tokens,
                                 args.runs, args.warmup),
        "speculative": lambda: bench_speculative(eng, args.max_tokens,
                                                 args.runs, args.warmup),
        "stt": lambda: bench_stt(eng, args.runs, args.warmup),
        "tts": lambda: bench_tts(eng, args.runs, args.warmup),
        "embed": lambda: bench_embed(eng, args.runs, args.warmup),
    }
    for name, fn in sections.items():
        if name in skip:
            results[name] = {"skipped": True}
            continue
        print(f"[benchmark] running {name} ...", file=sys.stderr)
        t0 = time.perf_counter()
        try:
            results[name] = fn()
        except Exception as exc:  # a section failure is honest, not fatal
            import traceback

            traceback.print_exc()
            results[name] = {"error": f"{type(exc).__name__}: {exc}"}
        print(f"[benchmark] {name} done in {time.perf_counter() - t0:.1f}s",
              file=sys.stderr)
    return build_report(environment, models, config, results, unavailable, METHODOLOGY)


def _default_out(environment):
    slug = chip_slug(environment.get("chip", "unknown"))
    return HERE / "benchmarks" / f"baseline_{slug}.json"


def main(argv=None):
    p = argparse.ArgumentParser(description="DARWIN on-device inference benchmark")
    p.add_argument("--runs", type=int, default=5, help="timed runs per measurement")
    p.add_argument("--warmup", type=int, default=1, help="warm-up runs discarded")
    p.add_argument("--max-tokens", dest="max_tokens", type=int, default=128,
                   help="decode length for the representative + speculative runs")
    p.add_argument("--long-max-tokens", dest="long_max_tokens", type=int, default=64,
                   help="decode length for the long-context run")
    p.add_argument("--skip", default="", help="comma list: llm,speculative,stt,tts,embed")
    p.add_argument("--json", action="store_true", help="print the result JSON to stdout")
    p.add_argument("--out", default=None, help="write the result JSON to this path")
    args = p.parse_args(argv)

    if args.runs < 1:
        p.error("--runs must be >= 1")
    if args.warmup < 0:
        p.error("--warmup must be >= 0")

    report = run_all(args)
    out = Path(args.out) if args.out else _default_out(report["environment"])
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(report, indent=2) + "\n")
    print(f"[benchmark] wrote {out}", file=sys.stderr)
    if args.json:
        print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
