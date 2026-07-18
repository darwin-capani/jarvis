"""On-device Core ML cross-encoder RERANKER (op=rerank backend).

WHAT: a purpose-built cross-encoder reranker — cross-encoder/ms-marco-MiniLM-L-6-v2
(BERT, 6-layer / 384-hidden / ~22M params, ONE relevance logit per (query, passage)
pair) — converted to Core ML (FP16 mlprogram, compute_units=ALL, ANE-ELIGIBLE) and
used as stage TWO of a two-stage retrieval stack: the fast bge bi-encoder embedder
(inference/coreml_embed.py) recalls the top-K candidates by dense cosine, then this
cross-encoder RE-SCORES those K with full query x passage attention and the daemon
re-orders by the cross-encoder score. This is the standard SOTA RAG stack (cheap
recall -> precise rerank).

WHY a cross-encoder beats the bi-encoder it reranks: the bi-encoder embeds the query
and each passage INDEPENDENTLY and compares them by a single cosine — the query never
"sees" the passage. A cross-encoder concatenates [CLS] query [SEP] passage [SEP] and
runs joint self-attention, so every query token attends to every passage token; it
resolves the fine-grained relevance the single-vector cosine cannot. The cost is that
it must run one forward PER candidate (it cannot be precomputed/indexed like an
embedding), which is exactly why it is used only to rerank a bounded top-K, never to
score the whole corpus.

MEASURE-FIRST: this backend ships only because the two rankings were MEASURED head to
head on the committed synthetic-but-representative retrieval eval and the rerank
MEASURABLY improved ranking (same discipline that DROPPED speculative decoding +
quantized-KV when they measured as losses). The committed probe harness + results are
under inference/benchmarks/coreml_rerank_eval/ (reproducible from the tree). HONESTY:
the numbers are SYNTHETIC-but-representative (Claude-authored labels over generated
facts/queries), directional build-decision evidence, not a production guarantee.

HONESTY — the ANE: compute_units=ComputeUnit.ALL makes the Apple Neural Engine (and
GPU) ELIGIBLE. Core ML schedules ANE/GPU/CPU at its own discretion and ANE residency
is unmeasurable without powermetrics. This module therefore claims "Core ML, ANE-
eligible" and cites only MEASURED end-to-end latency under ComputeUnit.ALL — never
that any op actually ran on the ANE.

TRUNCATION: each (query, passage) pair is tokenized to a FIXED sequence length of SEQ
(512) tokens — the model's native maximum — with the PASSAGE truncated first
(truncation="only_second") so a long chunk loses its tail but the query stays intact.
When a pair is actually truncated `rerank` emits a THROTTLED warning naming how many
pairs were capped (never silent). 512 covers docsearch's ~1200-char (~300-token)
chunks plus a short query comfortably; dense/CJK chunks that run near one token per
char can still exceed it and lose their tail — lower [docsearch].chunk_chars if that
matters for your corpus.

CONVERT-ON-FIRST-USE (ATOMIC): identical discipline to coreml_embed.py — the compiled
model + tokenizer are cached under the SAME model-cache root the rest of the server
uses ($HF_HOME, falling back to ~/.cache/huggingface), in the shared `darwin-coreml/`
subtree. A missing / partial / corrupt / wrong-SEQ cache (a crash / disk-full /
concurrent writer can leave a .mlpackage that merely EXISTS) fails a validate-LOAD and
is transparently reconverted from the HF checkpoint into a private temp dir, validate-
LOADED there, and only then atomically renamed into place (never os.replace onto a
populated dir — .mlpackage IS a directory). Prefer EAGER warm at server startup
(server.preload) so the first real op=rerank never pays conversion latency inside the
daemon's request timeout. Conversion needs torch + transformers (heavy, one-time);
runtime prediction needs only coremltools + the fast tokenizer.

The conversion recipe MIRRORS coreml_embed.py's proven recipe (transformers 5.11 /
coremltools 9.0 / torch 2.12), with the ONE cross-encoder difference called out:
  - position_ids are baked as a CONSTANT buffer at the full (batch, seq) shape, so the
    traced graph emits no tensor-derived Python scalar (which trips a coremltools
    _int-cast bug).
  - token_type_ids are a real INPUT here (NOT a const-zero buffer like the embedder):
    a cross-encoder's segment ids distinguish the query (0) from the passage (1), so
    they carry meaning and must be fed per pair.
  - BertModel._create_attention_masks is overridden to build the 4D additive mask with
    elementary ops (bypasses transformers 5.11 masking_utils, unsupported by
    coremltools 9.0's torch frontend).
  - the additive masked value is a FINITE -1e4 (NOT finfo.min): in FP16 finfo(fp32).min
    casts to -inf and 0 * -inf = NaN; exp(-1e4) underflows to 0 in both fp16 and fp32,
    so masking stays exact.
  - the output is the classifier's SINGLE raw logit per row (num_labels=1) — the
    relevance score. The checkpoint's default head activation is Identity, and sigmoid
    is monotonic, so ranking by the raw logit is identical to ranking by a probability;
    we emit the raw logit (no activation baked in) and rank by it.

This module imports only stdlib + numpy at top level; coremltools / torch /
transformers are imported lazily inside methods, so `import coreml_rerank` (and
py_compile / pyflakes) succeed even in an env without them — an env without the deps
simply cannot build/load the Core ML model, and the caller falls back to the dense
retrieval order and reports the fallback (never silently).
"""
import logging
import math
import os
import shutil
import tempfile
import threading

import numpy as np

from coreml_embed import cache_dir  # shared <hf-cache>/darwin-coreml/ subtree

log = logging.getLogger("darwin.coreml_rerank")

# Stable HF checkpoint this backend reranks with.
MODEL_ID = "cross-encoder/ms-marco-MiniLM-L-6-v2"
# STABLE wire id for this reranker (op=rerank `reranker` field). Opaque downstream —
# the daemon/HUD carry it verbatim to name which model produced the order. This
# backend pins ONE model at ONE seq, so the id is a fixed string.
RERANKER_ID = "coreml-ms-marco-minilm-l6-v2"
# Fixed sequence length: each (query, passage) pair is padded / truncated to this many
# tokens. Set to the checkpoint's NATIVE max_position_embeddings (512) so a query plus
# a full docsearch chunk (~300 tokens) fits without tail loss. Short fact pairs pad to
# 512 (padding is masked out, so their score is unchanged vs a shorter seq).
SEQ = 512
# Throttle for the truncation warning: log once, then again every N cumulative
# truncated pairs, so reranking a long-chunk corpus surfaces tail loss without a
# per-batch log flood.
WARN_TRUNC_EVERY = 128
# Hard cap on how many candidate passages one rerank call scores. The daemon reranks a
# bounded top-K (K is small: 20/50), but this guards a pathological caller from
# enqueueing an unbounded number of forwards under the predict lock. Surfaced as a
# ValueError (never a silent clamp) so a mis-sized K is a loud config bug, not a
# quietly-dropped tail.
MAX_PASSAGES = 256

# ONE fixed-shape (1, SEQ) graph, looped per (query, passage) pair. Mirrors the
# embedder's MEASURED finding at seq=512 / ComputeUnit.ALL: a fixed (1, 512) graph
# looped per row beats a large batched graph (the big shape spills off Core ML's
# efficient path), and it is simpler (no pad-to-batch / discard logic). The reranker's
# realistic workload is K pairs sharing one query, all looped through this graph.

# Compiled-model / tokenizer artifact names under the per-model cache dir.
_MODEL_NAME = "rerank_b1.mlpackage"
_TOK_DIRNAME = "tokenizer"


# ---- PURE helpers (no model / no tokenizer / no I/O — unit-tested) ----------


def pad_pair(ids_row, type_row, seq=SEQ):
    """PURE. Pad/truncate ONE (input_ids, token_type_ids) pair to a dense (1, seq)
    int32 array each + its (1, seq) int32 attention mask. The row is truncated to the
    first `seq` ids (the honest length cap) and right-padded with 0; the mask is 1 on
    real tokens, 0 on padding. token_type_ids pad with 0 (the query segment id, an
    inert choice for masked pad positions). Returns (ids, types, mask)."""
    ids = np.zeros((1, seq), dtype=np.int32)
    types = np.zeros((1, seq), dtype=np.int32)
    mask = np.zeros((1, seq), dtype=np.int32)
    r = ids_row[:seq]
    tr = type_row[:seq]
    k = len(r)
    if k:
        ids[0, :k] = np.asarray(r, dtype=np.int32)
        types[0, :k] = np.asarray(tr[:k], dtype=np.int32)
        mask[0, :k] = 1
    return ids, types, mask


def scrub_score(x):
    """PURE. Map a non-finite score (NaN / +-Inf) to a large-magnitude FINITE
    sentinel so the JSON response stays strict-valid AND the degenerate row sorts to
    the BOTTOM rather than poisoning the order. The model runs in FP16; this is the
    last-line guard that NaN/Inf never reaches the wire. Returns a float."""
    if math.isfinite(x):
        return float(x)
    if x == math.inf:
        return 1.0e30
    return -1.0e30  # NaN or -inf -> sort to the bottom


def normalize_text(text):
    """PURE. An empty / whitespace-only query or passage still needs a score, so it
    falls back to a single space (so the tokenizer yields at least one real content
    position). Mirrors coreml_embed.normalize_text."""
    if text is None:
        return " "
    return text if text.strip() else " "


class CoreMLRerankerUnavailable(RuntimeError):
    """Raised when the Core ML reranker cannot be built or loaded (conversion failure,
    missing deps, coremltools issue). The engine catches this so op=rerank reports the
    honest fallback (fell_back=True) and the daemon keeps the dense retrieval order —
    never silently."""


class CoreMLReranker:
    """Loads/caches the Core ML cross-encoder + tokenizer and scores a batch of
    (query, passage) pairs to ONE relevance logit each (ONE fixed (1, SEQ) graph,
    looped per pair). Convert-on-first-use; thread-safe (its own lock guards lazy load
    + predict — it does NOT touch the engine's MLX GPU lock, so reranking runs
    independently of LLM generation, exactly like the embedder)."""

    def __init__(self, root=None):
        self._dir = cache_dir(root, model_id=MODEL_ID)
        self._lock = threading.Lock()
        self._loaded = False
        self._tokenizer = None
        self._model = None  # the (1, SEQ) MLModel, looped per pair
        self._trunc_seen = 0        # cumulative truncated pairs (for the warn throttle)
        self._trunc_warned_at = 0   # _trunc_seen value at the last warn

    def _validate_predict(self, model):
        """Run a tiny (1, SEQ) predict to VALIDATE a compiled graph actually runs at
        the SHIPPED shape and returns the right output shape. Presence on disk is NOT
        integrity: a truncated / partial-write .mlpackage (crash / disk-full /
        concurrent writer) can load-open yet fail to predict, and an OLD cache compiled
        at a different SEQ fails this shape check — either way this raises so the cache
        is reconverted."""
        ids = np.zeros((1, SEQ), dtype=np.int32)
        ids[:, 0] = 101  # any real token id; content is irrelevant to validation
        types = np.zeros((1, SEQ), dtype=np.int32)
        mask = np.zeros((1, SEQ), dtype=np.int32)
        mask[:, 0] = 1
        out = np.asarray(
            model.predict(
                {"input_ids": ids, "attention_mask": mask, "token_type_ids": types}
            )["score"]
        )
        # (1, 1) classifier logit per row.
        if out.shape not in ((1, 1), (1,)):
            raise ValueError(
                f"compiled model output shape {out.shape} != expected (1, 1)"
            )

    def _load_from(self, d):
        """VALIDATE-LOAD the tokenizer + the compiled model from directory `d`: load
        each, then run `_validate_predict` so a partial/corrupt/wrong-SEQ package is
        rejected (never trusted on mere presence). Returns (tokenizer, model) on
        success; raises on any problem."""
        import coremltools as ct
        from transformers import AutoTokenizer

        tok = AutoTokenizer.from_pretrained(os.path.join(d, _TOK_DIRNAME))
        model = ct.models.MLModel(
            os.path.join(d, _MODEL_NAME), compute_units=ct.ComputeUnit.ALL
        )
        self._validate_predict(model)
        return tok, model

    def ensure_loaded(self):
        """Convert-on-first-use (ATOMIC) then load the compiled model + tokenizer.
        Idempotent + thread-safe. The cache is trusted only if it VALIDATE-LOADS (see
        `_load_from`); a missing / partial / corrupt / wrong-SEQ cache is reconverted
        into a temp dir and atomically published BEFORE it is loaded from. Raises
        CoreMLRerankerUnavailable on any failure so the caller falls back."""
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
                raise CoreMLRerankerUnavailable(
                    f"Core ML reranker deps unavailable: {', '.join(missing)} not installed"
                )
            try:
                loaded = self._try_load_final()
                if loaded is None:
                    self._convert_atomic()  # ONE-TIME; validates before publishing
                    loaded = self._try_load_final()
                    if loaded is None:
                        raise CoreMLRerankerUnavailable(
                            "Core ML reranker cache failed validate-load after conversion"
                        )
                self._tokenizer, self._model = loaded
            except CoreMLRerankerUnavailable:
                raise
            except Exception as e:
                raise CoreMLRerankerUnavailable(
                    f"Core ML reranker build/load failed: {e}"
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
                "Core ML reranker cache at %s not usable (%s); reconverting",
                self._dir, e,
            )
            return None

    def _convert_atomic(self):
        """Convert into a PRIVATE temp dir under the cache root, validate-LOAD it
        there, then atomically rename it into the final path — so a crash / disk-full /
        concurrent second writer can never leave a partial cache the loader would
        trust. Mirrors coreml_embed._convert_atomic exactly (the review-hardened atomic
        publish: unique vacate target, publish-race tolerance, finally-swept strays).
        Raises on any failure (nothing is published)."""
        parent = os.path.dirname(self._dir)  # <root>/darwin-coreml
        os.makedirs(parent, exist_ok=True)
        tmp = tempfile.mkdtemp(prefix=".convert-", dir=parent)
        trash = None  # bound BEFORE the try: the finally below references it
        try:
            self._convert_into(tmp)
            # VALIDATE-LOAD in the temp dir BEFORE publishing: a partial/corrupt write
            # is caught here and never becomes the trusted cache.
            self._load_from(tmp)
            # Publish. os.replace of a directory is atomic within one filesystem (tmp
            # is a sibling of self._dir), BUT os.replace onto a NON-EMPTY dir raises
            # ENOTEMPTY — and .mlpackage IS a directory. So we never replace a
            # populated dir: we ATOMICALLY move any existing dir aside first (a unique
            # fresh name -> a pure atomic rename), then replace into the vacated name.
            # Two processes converting at once end with a valid dir whoever wins the
            # last rename; a loser whose replace fails just loads the winner's
            # already-published valid cache instead of crashing to the fallback.
            if os.path.exists(self._dir):
                trash = tempfile.mkdtemp(prefix=".stale-", dir=parent)
                os.rmdir(trash)  # os.replace onto a NON-EXISTENT name = pure rename
                try:
                    os.replace(self._dir, trash)  # atomic vacate
                except OSError:
                    trash = None  # a concurrent process already moved/replaced it
            try:
                os.replace(tmp, self._dir)
                tmp = None
            except OSError:
                # Lost the publish race: a concurrent process already put a dir in
                # place. If it validate-loads it's the winner's good cache — keep it and
                # drop ours; otherwise re-raise so we don't trust it.
                if self._try_load_final() is None:
                    raise
        finally:
            if tmp is not None and os.path.isdir(tmp):
                shutil.rmtree(tmp, ignore_errors=True)
            # The vacated old dir is trash whether we published or raised (its contents
            # already failed validate-load or were superseded) — clean it in the
            # FINALLY so no exit path leaks it. Crash-leaked .stale-* strays from
            # EARLIER runs are also swept. Deliberately NOT swept: .convert-* dirs — one
            # may be a CONCURRENT process's live in-flight conversion; its own finally
            # cleans it.
            if trash is not None and os.path.isdir(trash):
                shutil.rmtree(trash, ignore_errors=True)
            try:
                for entry in os.listdir(parent):
                    if entry.startswith(".stale-"):
                        stray = os.path.join(parent, entry)
                        # Only sweep NON-EMPTY strays: a leaked vacated dir has
                        # contents, while an EMPTY .stale-* may be a CONCURRENT
                        # process's just-created mkdtemp artifact (deleting it would
                        # break that process's vacate rmdir). An empty leak is harmless
                        # — mkdtemp always mints fresh names.
                        if (
                            stray != trash
                            and os.path.isdir(stray)
                            and os.listdir(stray)
                        ):
                            shutil.rmtree(stray, ignore_errors=True)
            except OSError:
                pass  # sweeping strays is best-effort housekeeping only

    def _convert_into(self, target_dir):
        """ONE-TIME conversion of the HF cross-encoder to the fixed (1, SEQ) Core ML
        package + the saved tokenizer, written under `target_dir` (a temp dir; the
        caller publishes it atomically). Uses the mirrored recipe (module docstring).
        Heavy: needs torch + transformers and downloads the checkpoint (honoring
        HF_HOME) on the first ever run."""
        import types

        import coremltools as ct
        import torch
        from transformers import AutoModelForSequenceClassification, AutoTokenizer

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

        class CrossEncoderScore(torch.nn.Module):
            """BERT cross-encoder -> pooler -> classifier -> ONE relevance logit."""

            def __init__(self, model, batch, seq):
                super().__init__()
                self.model = model
                pos = (
                    torch.arange(seq, dtype=torch.long)
                    .unsqueeze(0)
                    .expand(batch, seq)
                    .contiguous()
                )
                self.register_buffer("pos_ids", pos)

            def forward(self, input_ids, attention_mask, token_type_ids):
                out = self.model(
                    input_ids=input_ids,
                    attention_mask=attention_mask,
                    token_type_ids=token_type_ids,
                    position_ids=self.pos_ids,
                )
                return out.logits  # (batch, 1) raw relevance logit

        def trace_and_convert(model, batch, out_path):
            wrapper = CrossEncoderScore(model, batch, seq).eval()
            ex_ids = torch.randint(0, 1000, (batch, seq), dtype=torch.int64)
            ex_mask = torch.ones((batch, seq), dtype=torch.int64)
            ex_types = torch.zeros((batch, seq), dtype=torch.int64)
            with torch.no_grad():
                traced = torch.jit.trace(
                    wrapper, (ex_ids, ex_mask, ex_types), check_trace=False
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
                    ct.TensorType(
                        name="token_type_ids", shape=(batch, seq), dtype=np.int32
                    ),
                ],
                outputs=[ct.TensorType(name="score")],
                convert_to="mlprogram",
                compute_precision=ct.precision.FLOAT16,
                minimum_deployment_target=ct.target.macOS15,
                compute_units=ct.ComputeUnit.ALL,
            )
            mlmodel.save(out_path)

        os.makedirs(target_dir, exist_ok=True)
        tok = AutoTokenizer.from_pretrained(MODEL_ID)
        model = AutoModelForSequenceClassification.from_pretrained(
            MODEL_ID, attn_implementation="eager"
        ).eval()
        model.bert._create_attention_masks = types.MethodType(
            _simple_masks, model.bert
        )
        if model.config.num_labels != 1:
            raise CoreMLRerankerUnavailable(
                f"{MODEL_ID} num_labels {model.config.num_labels} != 1 "
                "(not a single-logit cross-encoder reranker)"
            )
        if getattr(model.config, "max_position_embeddings", SEQ) < SEQ:
            raise CoreMLRerankerUnavailable(
                f"{MODEL_ID} max_position_embeddings "
                f"{model.config.max_position_embeddings} < seq {SEQ}"
            )
        trace_and_convert(model, 1, os.path.join(target_dir, _MODEL_NAME))
        tok.save_pretrained(os.path.join(target_dir, _TOK_DIRNAME))

    def _encode(self, query, passages):
        """Tokenize (query, passage) PAIRS to lists of (input_ids, token_type_ids),
        truncated to SEQ with the PASSAGE truncated first (truncation="only_second")
        so the query stays intact. Empty/whitespace-only query or passage falls back to
        a single space. When any pair is actually truncated (its full token length
        exceeded SEQ), emit a THROTTLED warning so tail loss is never silent."""
        q = normalize_text(query)
        ps = [normalize_text(p) for p in passages]
        enc = self._tokenizer(
            [q] * len(ps),
            ps,
            truncation="only_second",
            max_length=SEQ,
            return_attention_mask=False,
        )
        ids = enc["input_ids"]
        types = enc.get("token_type_ids")
        if types is None:  # defensive: a tokenizer without segment ids -> all-query
            types = [[0] * len(row) for row in ids]
        n_trunc = getattr(enc, "num_truncated_tokens", None)
        if n_trunc is None:
            n_capped = sum(1 for row in ids if len(row) >= SEQ)
        else:
            vals = n_trunc if isinstance(n_trunc, (list, tuple)) else [n_trunc]
            n_capped = sum(1 for v in vals if v and v > 0)
        if n_capped:
            self._warn_truncated(n_capped, len(ids))
        return ids, types

    def _warn_truncated(self, n_capped, total):
        """Throttled (once per WARN_TRUNC_EVERY occurrences) truncation warning, so
        reranking a corpus of over-long chunks logs the tail loss without flooding the
        log on every batch."""
        self._trunc_seen += n_capped
        if self._trunc_seen - self._trunc_warned_at >= WARN_TRUNC_EVERY or self._trunc_warned_at == 0:
            self._trunc_warned_at = self._trunc_seen
            log.warning(
                "coreml rerank: %d/%d pair(s) exceeded the %d-token cap and the "
                "passage tail was truncated (not scored); %d truncated so far. Lower "
                "[docsearch].chunk_chars if this corpus has long/dense chunks.",
                n_capped, total, SEQ, self._trunc_seen,
            )

    def _predict(self, ids, types, mask):
        """Run one (1, SEQ) predict; returns the scalar relevance logit."""
        out = self._model.predict(
            {"input_ids": ids, "attention_mask": mask, "token_type_ids": types}
        )
        return float(np.asarray(out["score"], dtype=np.float32).ravel()[0])

    def rerank(self, query, passages):
        """Score a batch of `passages` against `query` -> list of relevance logits, in
        input order (higher = more relevant; the CALLER sorts). Empty passage list ->
        []. Each pair is one (1, SEQ) predict, looped (mirrors the embedder's measured
        seq=512 finding). Scores are scrubbed so no NaN/Inf reaches the wire. The batch
        cap (MAX_PASSAGES) is enforced here; the length cap by tokenization.
        Thread-safe."""
        if not passages:
            return []
        if len(passages) > MAX_PASSAGES:
            raise ValueError(
                f"op=rerank batch of {len(passages)} passages exceeds the "
                f"{MAX_PASSAGES} cap"
            )
        self.ensure_loaded()
        id_lists, type_lists = self._encode(query, passages)
        out = []
        with self._lock:
            for ids_row, type_row in zip(id_lists, type_lists):
                ids, types, mask = pad_pair(ids_row, type_row, SEQ)  # (1, SEQ)
                out.append(scrub_score(self._predict(ids, types, mask)))
        return out

    def reference_scores(self, query, passages):
        """DEVICE/DEP-GATED faithfulness reference: compute the SAME cross-encoder in
        torch fp32 (no Core ML) for the smoke test to compare against (Spearman ~= 1.0
        + tiny abs delta confirms the FP16 Core ML graph is faithful). Needs torch +
        transformers. Returns a list of relevance logits in input order."""
        import types as _types

        import torch
        from transformers import AutoModelForSequenceClassification

        def _simple_masks(
            self_enc, attention_mask, encoder_attention_mask,
            embedding_output, encoder_hidden_states, past_key_values,
        ):
            dtype = embedding_output.dtype
            if attention_mask is None:
                return None, None
            am = attention_mask.to(dtype)
            return (1.0 - am)[:, None, None, :] * (-1.0e4), None

        model = AutoModelForSequenceClassification.from_pretrained(
            MODEL_ID, attn_implementation="eager"
        ).eval()
        model.bert._create_attention_masks = _types.MethodType(
            _simple_masks, model.bert
        )
        self.ensure_loaded()
        id_lists, type_lists = self._encode(query, passages)
        out = []
        with torch.no_grad():
            for ids_row, type_row in zip(id_lists, type_lists):
                ids, types, mask = pad_pair(ids_row, type_row, SEQ)
                pos = torch.arange(SEQ, dtype=torch.long).unsqueeze(0)
                logit = model(
                    input_ids=torch.from_numpy(ids).long(),
                    attention_mask=torch.from_numpy(mask).long(),
                    token_type_ids=torch.from_numpy(types).long(),
                    position_ids=pos,
                ).logits
                out.append(scrub_score(float(logit.numpy().ravel()[0])))
        return out


def _spearman(a, b):
    """PURE. Spearman rank correlation (rank-then-Pearson) for the smoke faithfulness
    check — ranking is what the reranker exists to get right, so we compare ORDER, not
    absolute logits."""
    def ranks(xs):
        order = sorted(range(len(xs)), key=lambda i: xs[i])
        r = [0.0] * len(xs)
        for rank, i in enumerate(order):
            r[i] = float(rank)
        return r
    ra, rb = ranks(a), ranks(b)
    n = len(a)
    ma, mb = sum(ra) / n, sum(rb) / n
    num = sum((x - ma) * (y - mb) for x, y in zip(ra, rb))
    da = math.sqrt(sum((x - ma) ** 2 for x in ra))
    db = math.sqrt(sum((y - mb) ** 2 for y in rb))
    return num / (da * db + 1e-12)


def _smoke():
    """DEVICE-GATED smoke: build/load the Core ML reranker, score a fixed query
    against a mix of relevant + irrelevant passages, print the scores + ranking, the
    FP16-vs-torch faithfulness (Spearman + max abs logit delta), and MEASURED per-pair
    / K=20 / K=50 latency. Run once by hand (NOT in CI):
        .venv/bin/python inference/coreml_rerank.py"""
    import statistics
    import time

    rr = CoreMLReranker()
    rr.ensure_loaded()
    print(f"loaded Core ML reranker from {rr._dir}", flush=True)

    query = "What kind of coffee does the user drink?"
    passages = [
        "The user drinks oat-milk cortados and never regular coffee.",   # relevant
        "The user's favorite coffee shop is Rook and Bean on Elm Street.",  # related
        "The user always books flights on Delta Air Lines.",              # irrelevant
        "The user grew up in Lisbon, Portugal.",                          # irrelevant
        "The user is vegetarian but eats fish occasionally.",            # weakly related
    ]
    scores = rr.rerank(query, passages)
    ref = rr.reference_scores(query, passages)
    order = sorted(range(len(passages)), key=lambda i: -scores[i])
    print(f"query: {query!r}")
    print("ranked (Core ML fp16):")
    for rank, i in enumerate(order):
        print(f"  {rank + 1}. score {scores[i]:+.4f}  {passages[i][:56]!r}")
    sp = _spearman(scores, ref)
    max_abs = max(abs(a - b) for a, b in zip(scores, ref))
    print(f"faithfulness vs torch fp32: spearman={sp:.4f}  max|dlogit|={max_abs:.4f}")

    # Latency: one pair, and the realistic K=20 / K=50 rerank batches (looped).
    def med(fn, n=8, warmup=3):
        for _ in range(warmup):
            fn()
        runs = []
        for _ in range(n):
            t0 = time.perf_counter()
            fn()
            runs.append((time.perf_counter() - t0) * 1000.0)
        return statistics.median(runs)

    pool = (passages * 11)[:50]  # 50 passages, short-fact regime
    single_ms = med(lambda: rr.rerank(query, passages[:1]))
    k20_ms = med(lambda: rr.rerank(query, pool[:20]))
    k50_ms = med(lambda: rr.rerank(query, pool[:50]))
    print(f"single-pair latency: {single_ms:.2f} ms")
    print(f"K=20 rerank latency: {k20_ms:.2f} ms  ({k20_ms / 20:.2f} ms/pair)")
    print(f"K=50 rerank latency: {k50_ms:.2f} ms  ({k50_ms / 50:.2f} ms/pair)")


if __name__ == "__main__":
    _smoke()
