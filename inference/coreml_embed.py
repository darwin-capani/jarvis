"""On-device Core ML sentence embedder (op=embed backend).

WHAT: a purpose-built contrastive sentence embedder — BAAI/bge-small-en-v1.5
(BERT, 33M params, 384-dim) — converted to Core ML (FP16 mlprogram,
compute_units=ALL, ANE-ELIGIBLE) and used as the on-device retrieval-embedding
backend, in place of mean-pooling the resident 4B LLM's hidden states.

WHY: retrieval QUALITY (recall@k / MRR) measured on a synthetic-but-
representative MNEMOSYNE-style set of short user facts (the eval harness +
labeled set + results are committed under inference/benchmarks/coreml_eval/),
plus LATENCY re-measured on this M1 Pro at the SHIPPED seq=512 / compute_units=
ALL config (the device-gated smoke in this file, `_smoke`):

    embedder                     recall@1  recall@3  recall@5   MRR    latency (seq=512, ComputeUnit.ALL)
    bge-small (this module,384d)  0.8241    0.9213    0.9861   0.9606  ~19.6ms/text (single OR K=8, looped)
    Qwen3-4B mean-pool (2560d)    0.2454    0.4630    0.5556   0.4235  ~124ms single / ~56.8ms/text batched

    Dramatically higher retrieval quality, and still faster than the 4B path:
    ~6.3x on single-text (19.6 vs 124 ms) and ~3x per text on the K=8 retrieval
    batch (19.0 vs 56.8 ms/text). NOTE the seq=512 latency is much higher than
    the seq=128 probe numbers (~2.5/1.7 ms) — a 4x-longer sequence with O(seq^2)
    attention — and at 512 a batched Core ML graph is SLOWER per text than
    looping the (1,512) graph, so this backend loops one text at a time (see the
    SEQ note). HONESTY on each number:
    - recall@k / MRR are SYNTHETIC-BUT-REPRESENTATIVE (Claude-authored labels
      over generated facts/queries, not a production corpus) — directional
      build-decision evidence, not a production guarantee. The facts are short
      (they fit in either seq), so these are SEQ-INDEPENDENT: the seq=512 change
      helps LONG document chunks (the docsearch case the recall set did not
      cover), not these short-fact scores. The bge model does PLAIN mean-pool
      with NO query-instruction prefix, so these are the plain-variant numbers
      (a query-instruction variant reaches recall@5 = 1.0 but is not what this
      module computes).
    - latency = MEASURED end-to-end median from `_smoke` at seq=512 under
      compute_units=ComputeUnit.ALL (the SHIPPED runtime config). The 4B numbers
      are the committed baseline (inference/benchmarks/baseline_m1_pro.json).

HONESTY — the ANE: compute_units=ComputeUnit.ALL makes the Apple Neural Engine
(and GPU) ELIGIBLE. Core ML schedules ANE/GPU/CPU at its own discretion and ANE
residency is unmeasurable without powermetrics. This module therefore claims
"Core ML, ANE-eligible" and cites only MEASURED end-to-end latency under
ComputeUnit.ALL — never that any op actually ran on the ANE.

TRUNCATION: inputs are tokenized to a FIXED sequence length of 512 tokens
(SEQ) — bge-small's native maximum. Inputs longer than 512 tokens are TRUNCATED
to the first 512, and when that actually happens `_encode` emits a throttled
WARNING naming how many inputs were capped (never silent). 512 covers
docsearch's ~1200-char (~300-token) English chunks comfortably, but note that
dense/CJK scripts run closer to one token per character, so a 1200-char CJK
chunk can exceed 512 tokens and lose its tail — index smaller chunks (lower
[docsearch].chunk_chars) if that matters for your corpus. 512 covers
every short user fact.

CONVERT-ON-FIRST-USE (ATOMIC): the compiled model + tokenizer are cached under
the SAME model-cache root the rest of the server uses ($HF_HOME, falling back to
~/.cache/huggingface — see `hf_cache_root`), in a `darwin-coreml/` subtree. If
the cache is absent OR fails a validate-LOAD (a crash / disk-full / concurrent
writer can leave a partial .mlpackage that merely EXISTS), the model is
converted ONCE from the HF bge checkpoint into a private temp dir, validate-
LOADED there, and only then atomically renamed into place — so a partial cache
is never trusted and is transparently reconverted (or the caller falls back).
Prefer EAGER warm at server startup (server.preload) so the first real op=embed
never pays conversion latency inside the daemon's request timeout. Conversion
needs torch + transformers (heavy, one-time); runtime prediction after
conversion needs only coremltools + the fast tokenizer.

The conversion recipe is the one proven in the ANE probe (its convert.py, the
provenance of this recipe) for the transformers 5.11 / coremltools 9.0 / torch
2.12 combo:
  - position_ids / token_type_ids are baked as CONSTANT buffers at the full
    (batch, seq) shape, so the traced graph emits no tensor-derived Python
    scalar (which trips a coremltools _int-cast bug).
  - BertModel._create_attention_masks is overridden to build the 4D additive
    mask with elementary ops (bypasses transformers 5.11 masking_utils, which
    coremltools 9.0's torch frontend does not support).
  - the additive masked value is a FINITE -1e4 (NOT finfo.min): in FP16
    finfo(fp32).min casts to -inf and 0 * -inf = NaN; exp(-1e4) underflows to 0
    in both fp16 and fp32, so masking stays exact.
  - masked mean-pool + L2-normalize are baked INTO the graph, so the model
    outputs a single normalized sentence vector per row.

This module imports only stdlib + numpy at top level; coremltools / torch /
transformers are imported lazily inside methods, so `import coreml_embed`
(and py_compile / pyflakes) succeed even in an env without them — an env
without the deps simply cannot build/load the Core ML model, and the caller
falls back to the 4B mean-pool path and reports the fallback (never silently).
"""
import logging
import math
import os
import shutil
import tempfile
import threading

import numpy as np

log = logging.getLogger("darwin.coreml_embed")

# Stable HF checkpoint this backend embeds with.
MODEL_ID = "BAAI/bge-small-en-v1.5"
# STABLE wire id for this embedder's vector space (op=embed `embedder` field).
# The daemon/docsearch store this with the index and compare it by STRING
# EQUALITY only (opaque space id); a store stamped with a different id is a
# different space and forces reindex. This backend pins ONE model at ONE seq, so
# the id is a fixed string. MUST match server.py EMBEDDER_COREML.
EMBEDDER_ID = "coreml-bge-small-en-v1.5"
# Output dimension (bge-small hidden size). Fixed by the checkpoint.
DIM = 384
# Fixed sequence length: inputs are padded / truncated to this many tokens. Set
# to bge-small's NATIVE max_position_embeddings (512) — NOT a smaller cap —
# because docsearch chunks are ~1200 chars (~300 WordPiece tokens); a 128 cap
# silently dropped ~60% of every document chunk from its vector (a real
# file-search regression the short-memory-fact recall eval never exercised). 512
# covers those chunks natively and safely. Short facts pad to 512 (padding is
# masked out, so their vectors are unchanged vs a shorter seq).
SEQ = 512
# Throttle for the truncation warning: log once, then again every N cumulative
# truncated inputs, so indexing a long-chunk corpus surfaces tail loss without
# a per-batch log flood.
WARN_TRUNC_EVERY = 128
# ONE fixed-shape (1, SEQ) graph, looped per text. MEASURED on this M1 Pro at
# seq=512 / ComputeUnit.ALL: looping the (1, 512) graph is ~1.57x FASTER per text
# than a fixed (8, 512) batched graph (~19.7 vs ~31 ms/text for K=8) — the large
# (8, 512) shape spills off Core ML's efficient path, so batching at this seq is
# counterproductive. A single graph is therefore both faster for the real
# workload AND simpler (no pad-to-batch / discard logic). (This inverts the
# seq=128 finding, where a batched graph won — the seq change moved the tradeoff.)

# Compiled-model / tokenizer artifact names under the per-model cache dir.
_MODEL_NAME = "emb_b1.mlpackage"
_TOK_DIRNAME = "tokenizer"


def hf_cache_root():
    """The model-cache ROOT the rest of the server/installer uses: $HF_HOME if
    set (the installer persists it into state/env.sh), else ~/.cache/huggingface
    — the exact resolution scripts/doctor.sh checks. The Core ML artifacts live
    in a `darwin-coreml/` subtree of this root so they share the ONE cache the
    installer manages (never the repo)."""
    root = os.environ.get("HF_HOME")
    if root:
        return root
    return os.path.join(os.path.expanduser("~"), ".cache", "huggingface")


def _safe_model_slug(model_id):
    """A filesystem-safe leaf name for a HF repo id (org/name -> org--name)."""
    return model_id.replace("/", "--")


def cache_dir(root=None, model_id=MODEL_ID):
    """Directory holding this model's compiled Core ML packages + tokenizer,
    under `<hf_cache_root>/darwin-coreml/<safe-model-id>/`."""
    base = root if root is not None else hf_cache_root()
    return os.path.join(base, "darwin-coreml", _safe_model_slug(model_id))


# ---- PURE helpers (no model / no tokenizer / no I/O — unit-tested) ----------


def pad_batch(id_lists, seq=SEQ):
    """PURE. Pad/truncate a list of token-id lists to a dense (n, seq) int32
    array + its (n, seq) int32 attention mask. Each row is truncated to the
    first `seq` ids (the honest length cap) and right-padded with 0; the mask is
    1 on real tokens, 0 on padding. Right-padding + the model's mask means a pad
    position never affects a real token's hidden state and is excluded from the
    mean-pool. Returns (ids, mask)."""
    n = len(id_lists)
    ids = np.zeros((n, seq), dtype=np.int32)
    mask = np.zeros((n, seq), dtype=np.int32)
    for i, row in enumerate(id_lists):
        r = row[:seq]
        k = len(r)
        if k:
            ids[i, :k] = np.asarray(r, dtype=np.int32)
            mask[i, :k] = 1
    return ids, mask


def scrub_vector(vec):
    """PURE. Map any non-finite component (NaN / +-Inf) to 0.0 so the JSON
    response stays strict-valid (a degenerate-but-finite vector keeps the whole
    batch from failing). The model L2-normalizes in-graph; this is the last-line
    guard that NaN/Inf never reach the wire. Returns a new list[float]."""
    return [float(x) if math.isfinite(x) else 0.0 for x in vec]


def normalize_text(text):
    """PURE. An input that is empty or whitespace-only still needs a vector, so
    it falls back to a single space (so the tokenizer yields at least one real
    content position). Mirrors the 4B path's _embed_encode empty-input guard."""
    if text is None:
        return " "
    return text if text.strip() else " "


class CoreMLEmbedderUnavailable(RuntimeError):
    """Raised when the Core ML embedder cannot be built or loaded (conversion
    failure, missing deps, coremltools issue). The engine catches this to fall
    back to the 4B mean-pool path and REPORT the fallback (never silent)."""


class CoreMLEmbedder:
    """Loads/caches the Core ML bge model + tokenizer and embeds a batch of
    strings to 384-dim L2-normalized vectors (ONE fixed (1, SEQ) graph, looped
    per text — see the _MODEL_NAME/SEQ note for why a batched graph is not used
    at seq=512). Convert-on-first-use; thread-safe (its own lock guards lazy load
    + predict — it does NOT touch the engine's MLX GPU lock, so embedding runs
    independently of LLM generation)."""

    def __init__(self, root=None):
        self._dir = cache_dir(root)
        self._lock = threading.Lock()
        self._loaded = False
        self._tokenizer = None
        self._model = None  # the (1, SEQ) MLModel, looped per text
        self._trunc_seen = 0        # cumulative truncated inputs (for the warn throttle)
        self._trunc_warned_at = 0   # _trunc_seen value at the last warn

    def _validate_predict(self, model):
        """Run a tiny (1, SEQ) predict to VALIDATE a compiled graph actually runs
        at the SHIPPED shape and returns the right output shape. Presence on disk
        is NOT integrity: a truncated / partial-write .mlpackage (crash /
        disk-full / concurrent writer) can load-open yet fail to predict, and an
        OLD cache compiled at a different SEQ fails this shape check — either way
        this raises so the cache is reconverted."""
        ids = np.zeros((1, SEQ), dtype=np.int32)
        ids[:, 0] = 101  # any real token id; content is irrelevant to validation
        mask = np.zeros((1, SEQ), dtype=np.int32)
        mask[:, 0] = 1
        out = np.asarray(model.predict({"input_ids": ids, "attention_mask": mask})["embedding"])
        if out.shape != (1, DIM):
            raise ValueError(
                f"compiled model output shape {out.shape} != expected {(1, DIM)}"
            )

    def _load_from(self, d):
        """VALIDATE-LOAD the tokenizer + the compiled model from directory `d`:
        load each, then run `_validate_predict` so a partial/corrupt/wrong-SEQ
        package is rejected (never trusted on mere presence). Returns
        (tokenizer, model) on success; raises on any problem."""
        import coremltools as ct
        from transformers import AutoTokenizer

        tok = AutoTokenizer.from_pretrained(os.path.join(d, _TOK_DIRNAME))
        model = ct.models.MLModel(
            os.path.join(d, _MODEL_NAME), compute_units=ct.ComputeUnit.ALL
        )
        self._validate_predict(model)
        return tok, model

    def ensure_loaded(self):
        """Convert-on-first-use (ATOMIC) then load the compiled model +
        tokenizer. Idempotent + thread-safe. The cache is trusted only if it
        VALIDATE-LOADS (see `_load_from`); a missing / partial / corrupt /
        wrong-SEQ cache is reconverted into a temp dir and atomically published
        BEFORE it is loaded from. Raises CoreMLEmbedderUnavailable on any failure
        so the caller falls back."""
        if self._loaded:
            return
        with self._lock:
            if self._loaded:
                return
            import importlib.util

            missing = [
                m for m in ("coremltools", "transformers")
                if importlib.util.find_spec(m) is None
            ]
            if missing:  # deps absent -> honest unavailable (caller falls back)
                raise CoreMLEmbedderUnavailable(
                    f"Core ML embedder deps unavailable: {', '.join(missing)} not installed"
                )
            try:
                loaded = self._try_load_final()
                if loaded is None:
                    self._convert_atomic()  # ONE-TIME; validates before publishing
                    loaded = self._try_load_final()
                    if loaded is None:
                        raise CoreMLEmbedderUnavailable(
                            "Core ML embedder cache failed validate-load after conversion"
                        )
                self._tokenizer, self._model = loaded
            except CoreMLEmbedderUnavailable:
                raise
            except Exception as e:
                raise CoreMLEmbedderUnavailable(
                    f"Core ML embedder build/load failed: {e}"
                ) from e
            self._loaded = True

    def _try_load_final(self):
        """Validate-load from the FINAL cache dir; return (tok, model) or None
        (missing / partial / corrupt / wrong-SEQ) so the caller reconverts."""
        if not os.path.isdir(self._dir):
            return None
        try:
            return self._load_from(self._dir)
        except Exception as e:
            log.warning(
                "Core ML embedder cache at %s not usable (%s); reconverting",
                self._dir, e,
            )
            return None

    def _convert_atomic(self):
        """Convert into a PRIVATE temp dir under the cache root, validate-LOAD it
        there, then atomically rename it into the final path — so a crash /
        disk-full / concurrent second writer can never leave a partial cache the
        loader would trust. Raises on any failure (nothing is published)."""
        parent = os.path.dirname(self._dir)  # <root>/darwin-coreml
        os.makedirs(parent, exist_ok=True)
        tmp = tempfile.mkdtemp(prefix=".convert-", dir=parent)
        trash = None  # bound BEFORE the try: the finally below references it
        try:
            self._convert_into(tmp)
            # VALIDATE-LOAD in the temp dir BEFORE publishing: a partial/corrupt
            # write is caught here and never becomes the trusted cache.
            self._load_from(tmp)
            # Publish. `os.replace` of a directory is atomic within one
            # filesystem (tmp is a sibling of self._dir), BUT os.replace onto a
            # NON-EMPTY dir raises ENOTEMPTY — and .mlpackage IS a directory. So
            # we never replace a populated dir: we ATOMICALLY move any existing
            # dir aside first (renaming a dir to a fresh name is atomic), then
            # replace into the vacated name. Two processes converting at once end
            # with a valid dir whoever wins the last rename; a loser whose
            # replace fails just loads the winner's already-published valid cache
            # (see ensure_loaded) instead of crashing to the mean-pool fallback.
            if os.path.exists(self._dir):
                # UNIQUE vacate target (review-caught: a PID-derived name is
                # reused across PID recycling, so a crash-leaked .stale-<pid>
                # dir could make a later vacate hit ENOTEMPTY and resurface the
                # fallback degradation this path exists to prevent). mkdtemp
                # gives a fresh empty dir; rmdir it so os.replace renames onto a
                # NON-EXISTENT name (a pure atomic rename).
                trash = tempfile.mkdtemp(prefix=".stale-", dir=parent)
                os.rmdir(trash)
                try:
                    os.replace(self._dir, trash)  # atomic vacate
                except OSError:
                    trash = None  # a concurrent process already moved/replaced it
            try:
                os.replace(tmp, self._dir)
                tmp = None
            except OSError:
                # Lost the publish race: a concurrent process already put a dir
                # in place. If it validate-loads it's the winner's good cache —
                # keep it and drop ours; otherwise re-raise so we don't trust it.
                if self._try_load_final() is None:
                    raise
        finally:
            if tmp is not None and os.path.isdir(tmp):
                shutil.rmtree(tmp, ignore_errors=True)
            # The vacated old dir is trash whether we published or raised (its
            # contents already failed validate-load or were superseded) — clean
            # it in the FINALLY so no exit path leaks it (review-caught: it was
            # cleaned only on the success path, and a crash-leaked stray could
            # cascade into the exact fallback degradation this path prevents).
            # Crash-leaked .stale-* strays from EARLIER runs are also swept.
            # Deliberately NOT swept: .convert-* dirs — one may be a CONCURRENT
            # process's live in-flight conversion; its own finally cleans it.
            if trash is not None and os.path.isdir(trash):
                shutil.rmtree(trash, ignore_errors=True)
            try:
                for entry in os.listdir(parent):
                    if entry.startswith(".stale-"):
                        stray = os.path.join(parent, entry)
                        # Only sweep NON-EMPTY strays: a leaked vacated dir has
                        # contents, while an EMPTY .stale-* may be a CONCURRENT
                        # process's just-created mkdtemp artifact (deleting it
                        # would break that process's vacate rmdir). An empty
                        # leak is harmless — mkdtemp always mints fresh names.
                        if (
                            stray != trash
                            and os.path.isdir(stray)
                            and os.listdir(stray)
                        ):
                            shutil.rmtree(stray, ignore_errors=True)
            except OSError:
                pass  # sweeping strays is best-effort housekeeping only

    def _convert_into(self, target_dir):
        """ONE-TIME conversion of the HF bge checkpoint to the fixed (1, SEQ)
        Core ML package + the saved tokenizer, written under `target_dir` (a temp
        dir; the caller publishes it atomically). Uses the proven recipe (module
        docstring). Heavy: needs torch + transformers and downloads the
        checkpoint (honoring HF_HOME) on the first ever run."""
        import types

        import coremltools as ct
        import torch
        from transformers import AutoModel, AutoTokenizer

        seq = SEQ

        def _simple_masks(
            self_enc, attention_mask, encoder_attention_mask,
            embedding_output, encoder_hidden_states, past_key_values,
        ):
            # Elementary-op replacement for BertModel._create_attention_masks:
            # finite -1e4 (not finfo.min) so fp16 never does 0 * -inf = NaN.
            dtype = embedding_output.dtype
            if attention_mask is None:
                return None, None
            am = attention_mask.to(dtype)
            add = (1.0 - am)[:, None, None, :] * (-1.0e4)
            return add, None

        class MeanPoolNorm(torch.nn.Module):
            """BERT encoder -> masked mean-pool -> L2-normalize -> sentence vec."""

            def __init__(self, encoder, batch, seq):
                super().__init__()
                self.encoder = encoder
                pos = (
                    torch.arange(seq, dtype=torch.long)
                    .unsqueeze(0)
                    .expand(batch, seq)
                    .contiguous()
                )
                self.register_buffer("pos_ids", pos)
                self.register_buffer(
                    "tok_type", torch.zeros(batch, seq, dtype=torch.long)
                )

            def forward(self, input_ids, attention_mask):
                out = self.encoder(
                    input_ids=input_ids,
                    attention_mask=attention_mask,
                    token_type_ids=self.tok_type,
                    position_ids=self.pos_ids,
                )
                last = out.last_hidden_state
                mask = attention_mask.unsqueeze(-1).to(last.dtype)
                summed = (last * mask).sum(dim=1)
                counts = mask.sum(dim=1).clamp(min=1e-9)
                mean = summed / counts
                norm = mean.norm(p=2, dim=-1, keepdim=True).clamp(min=1e-12)
                return mean / norm

        def trace_and_convert(encoder, batch, out_path):
            wrapper = MeanPoolNorm(encoder, batch, seq).eval()
            ex_ids = torch.randint(0, 1000, (batch, seq), dtype=torch.int64)
            ex_mask = torch.ones((batch, seq), dtype=torch.int64)
            with torch.no_grad():
                traced = torch.jit.trace(
                    wrapper, (ex_ids, ex_mask), check_trace=False
                )
            mlmodel = ct.convert(
                traced,
                inputs=[
                    ct.TensorType(
                        name="input_ids", shape=(batch, seq), dtype=np.int32
                    ),
                    ct.TensorType(
                        name="attention_mask", shape=(batch, seq), dtype=np.int32
                    ),
                ],
                outputs=[ct.TensorType(name="embedding")],
                convert_to="mlprogram",
                compute_precision=ct.precision.FLOAT16,
                minimum_deployment_target=ct.target.macOS15,
                compute_units=ct.ComputeUnit.ALL,
            )
            mlmodel.save(out_path)

        os.makedirs(target_dir, exist_ok=True)
        tok = AutoTokenizer.from_pretrained(MODEL_ID)
        enc = AutoModel.from_pretrained(
            MODEL_ID, attn_implementation="eager"
        ).eval()
        enc._create_attention_masks = types.MethodType(_simple_masks, enc)
        if enc.config.hidden_size != DIM:
            raise CoreMLEmbedderUnavailable(
                f"{MODEL_ID} hidden size {enc.config.hidden_size} != expected {DIM}"
            )
        if getattr(enc.config, "max_position_embeddings", SEQ) < SEQ:
            raise CoreMLEmbedderUnavailable(
                f"{MODEL_ID} max_position_embeddings "
                f"{enc.config.max_position_embeddings} < seq {SEQ}"
            )
        trace_and_convert(enc, 1, os.path.join(target_dir, _MODEL_NAME))
        tok.save_pretrained(os.path.join(target_dir, _TOK_DIRNAME))

    def _encode(self, texts):
        """Tokenize a list of strings to a list of token-id lists, truncated to
        SEQ. Empty/whitespace-only inputs fall back to a single space. When any
        input is actually truncated (its full token length exceeded SEQ), emit a
        THROTTLED warning so tail loss is never silent — the tail of an
        over-long chunk is not in its vector, which recall should know about."""
        norm = [normalize_text(t) for t in texts]
        enc = self._tokenizer(
            norm, truncation=True, max_length=SEQ, return_attention_mask=False
        )
        ids = enc["input_ids"]
        # Detect real truncation without a second full tokenize: a row at exactly
        # SEQ was capped iff its untruncated length would exceed SEQ. The fast
        # tokenizer reports overflow via `num_truncated_tokens` when asked; fall
        # back to the len==SEQ proxy if that field is absent.
        n_trunc = getattr(enc, "num_truncated_tokens", None)
        if n_trunc is None:
            n_capped = sum(1 for row in ids if len(row) >= SEQ)
        else:
            vals = n_trunc if isinstance(n_trunc, (list, tuple)) else [n_trunc]
            n_capped = sum(1 for v in vals if v and v > 0)
        if n_capped:
            self._warn_truncated(n_capped, len(ids))
        return ids

    def _warn_truncated(self, n_capped, total):
        """Throttled (once per WARN_TRUNC_EVERY occurrences) truncation warning,
        so indexing a corpus of over-long chunks logs the tail loss without
        flooding the log on every batch."""
        self._trunc_seen += n_capped
        if self._trunc_seen - self._trunc_warned_at >= WARN_TRUNC_EVERY or self._trunc_warned_at == 0:
            self._trunc_warned_at = self._trunc_seen
            log.warning(
                "coreml embed: %d/%d input(s) exceeded the %d-token cap and were "
                "truncated (tail not embedded); %d truncated so far. Lower "
                "[docsearch].chunk_chars if this corpus has long/dense chunks.",
                n_capped, total, SEQ, self._trunc_seen,
            )

    def _predict(self, ids, mask):
        """Run one (1, SEQ) predict; returns a numpy (1, DIM) array."""
        out = self._model.predict({"input_ids": ids, "attention_mask": mask})
        return np.asarray(out["embedding"], dtype=np.float32)

    def embed(self, texts):
        """Embed a batch of strings -> list of DIM-dim L2-normalized float
        vectors, in input order. Empty batch -> []. Each text is one (1, SEQ)
        predict, looped (measured faster than a batched graph at seq=512 — see
        the module SEQ note). Vectors are scrubbed so no NaN/Inf reaches the
        wire. The batch/length caps are enforced by the caller (batch) and
        tokenization (length). Thread-safe."""
        if not texts:
            return []
        self.ensure_loaded()
        id_lists = self._encode(texts)
        out = []
        with self._lock:
            for ids_row in id_lists:
                ids, mask = pad_batch([ids_row], SEQ)  # (1, SEQ)
                vec = self._predict(ids, mask)[0]
                out.append(scrub_vector(vec.tolist()))
        return out

    def reference_vectors(self, texts):
        """DEVICE/DEP-GATED faithfulness reference: compute the SAME recipe in
        torch fp32 (no Core ML) for `texts`, for the smoke test to compare
        against (cosine ~= 1.0 confirms the FP16 Core ML graph is faithful).
        Needs torch + transformers. Returns a list of DIM-dim vectors."""
        import types

        import torch
        from transformers import AutoModel

        def _simple_masks(
            self_enc, attention_mask, encoder_attention_mask,
            embedding_output, encoder_hidden_states, past_key_values,
        ):
            dtype = embedding_output.dtype
            if attention_mask is None:
                return None, None
            am = attention_mask.to(dtype)
            return (1.0 - am)[:, None, None, :] * (-1.0e4), None

        enc = AutoModel.from_pretrained(
            MODEL_ID, attn_implementation="eager"
        ).eval()
        enc._create_attention_masks = types.MethodType(_simple_masks, enc)
        self.ensure_loaded()
        id_lists = self._encode(texts)
        ids, mask = pad_batch(id_lists, SEQ)
        ii = torch.from_numpy(ids).long()
        mm = torch.from_numpy(mask).long()
        pos = torch.arange(SEQ, dtype=torch.long).unsqueeze(0).expand(len(id_lists), SEQ)
        tt = torch.zeros(len(id_lists), SEQ, dtype=torch.long)
        with torch.no_grad():
            out = enc(
                input_ids=ii, attention_mask=mm,
                token_type_ids=tt, position_ids=pos,
            )
            last = out.last_hidden_state
            m = mm.unsqueeze(-1).to(last.dtype)
            mean = (last * m).sum(dim=1) / m.sum(dim=1).clamp(min=1e-9)
            normed = mean / mean.norm(p=2, dim=-1, keepdim=True).clamp(min=1e-12)
        return [scrub_vector(r) for r in normed.numpy().tolist()]


def _cosine(a, b):
    dot = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(x * x for x in b))
    return dot / (na * nb + 1e-12)


def _smoke():
    """DEVICE-GATED smoke: build/load the Core ML embedder, embed a couple of
    fixed strings, print MEASURED single/batched latency + the FP16-vs-torch
    faithfulness cosine + a similar/unrelated separation sanity. Run once by
    hand (NOT in CI):  .venv/bin/python inference/coreml_embed.py"""
    import statistics
    import time

    emb = CoreMLEmbedder()
    emb.ensure_loaded()
    print(f"loaded Core ML embedder from {emb._dir}", flush=True)

    fixed = [
        "The user prefers dark mode and lives in the Pacific timezone.",
        "The user's project is named DARWIN.",
    ]
    # Faithfulness: Core ML (FP16) vs torch (fp32) on the fixed strings.
    cml = emb.embed(fixed)
    ref = emb.reference_vectors(fixed)
    faith = [round(_cosine(a, b), 6) for a, b in zip(cml, ref)]
    print(f"dim = {len(cml[0])}  (expected {DIM})")
    print(f"faithfulness cosine (CoreML fp16 vs torch fp32): {faith}")

    # Similar / unrelated separation sanity.
    pairs = [
        ("similar", "A man is playing an acoustic guitar.", "Someone is strumming a guitar."),
        ("unrelated", "The stock market fell sharply this week.", "My cat likes to sleep on the couch."),
    ]
    for kind, a, b in pairs:
        va, vb = emb.embed([a])[0], emb.embed([b])[0]
        print(f"  [{kind:>9}] cosine {_cosine(va, vb):+.4f}")

    # Latency: single text, and the realistic K=8 retrieval batch (looped).
    batch8 = [
        "What timezone does the user live in?",
        "The user lives in the Pacific timezone.",
        "The user prefers dark mode across all apps.",
        "The user's primary language is English.",
        "The user drinks coffee, not tea.",
        "The user's project is named DARWIN.",
        "The user owns an Apple M1 Pro laptop.",
        "The user usually works late at night.",
    ]

    def med(fn, n=10, warmup=3):
        for _ in range(warmup):
            fn()
        runs = []
        for _ in range(n):
            t0 = time.perf_counter()
            fn()
            runs.append((time.perf_counter() - t0) * 1000.0)
        return statistics.median(runs)

    single_ms = med(lambda: emb.embed([fixed[0]]))
    batch_ms = med(lambda: emb.embed(batch8))
    long_chunk = ("word " * 300).strip()  # ~300 tokens: the docsearch chunk case
    long_ms = med(lambda: emb.embed([long_chunk]))
    print(f"single-text latency: {single_ms:.2f} ms")
    print(f"K=8 batch latency: {batch_ms:.2f} ms  ({batch_ms / 8:.2f} ms/text, looped)")
    print(f"long chunk (~300 tok) latency: {long_ms:.2f} ms  "
          f"(dim {len(emb.embed([long_chunk])[0])})")


if __name__ == "__main__":
    _smoke()
