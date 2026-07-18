#!/usr/bin/env python
"""
Two-stage retrieval MEASURE-FIRST probe: does adding a Core ML cross-encoder RERANK
stage on top of the shipped bge bi-encoder MEASURABLY improve ranking?

This is the adoption gate for inference/coreml_rerank.py (the op=rerank backend) and
the [inference].reranker config, run with the SAME discipline that DROPPED speculative
decoding + quantized-KV when they measured as losses: the reranker ships ONLY if the
rerank wins here. A latency cost with no ranking win is a NO-GO.

Two rankings are compared per query over the SAME committed corpus + labeled queries
(../coreml_eval/eval_set.json, 100 facts / 36 queries, 1-3 relevant each):

  A  bge dense cosine over ALL 100 facts  (the CURRENT shipped behavior — the Core ML
     bge bi-encoder in inference/coreml_embed.py, raw symmetric, top-k by cosine).
  B  bge retrieve top-K THEN cross-encoder rerank those K and re-order by the
     cross-encoder relevance logit (inference/coreml_rerank.py). Measured at K=20 and
     K=50. The tail below rank K keeps its dense order (a real reranker only reorders
     the retrieved shortlist).

Metrics (binary relevance): nDCG@10, recall@1/3/5, MRR. Plus dense recall@K (how many
relevant facts stage A even PUT in the top-K — the ceiling stage B can recover to) and
MEASURED rerank latency per query at K=20/50 on this machine.

Both stages are the SHIPPED on-device Core ML modules (converted on first use, cached
under the HF cache root), so this harness is FULLY REPRODUCIBLE from the tree:
    .venv/bin/python inference/benchmarks/coreml_rerank_eval/eval_rerank.py
(the first run pays the one-time bge + cross-encoder Core ML conversion).

HONESTY:
  - SYNTHETIC-but-representative eval (Claude-authored labels over generated
    MNEMOSYNE-style facts), directional build/no-build evidence, NOT ground-truth
    production data. Same corpus + label set the bge embedder adoption used.
  - Ranking A here reproduces the committed bge_384d_plain numbers (raw symmetric
    cosine — DARWIN's live NeuralEmbeddingProvider ranking), recomputed live, plus
    nDCG@10 which the embedder eval did not record.
  - Latency = MEASURED end-to-end median under compute_units=ComputeUnit.ALL (the
    SHIPPED runtime config). The cross-encoder is ANE-ELIGIBLE; this never claims any
    op ran on the ANE — only measured wall-clock.
  - recall@k for k<=K after rerank is bounded by dense recall@K (rerank only reorders
    the shortlist, it cannot retrieve a fact stage A missed) — reported so a rerank
    "win" is never confused with a recall the reranker did not actually produce.
"""
import json
import math
import os
import statistics
import sys
import time

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
BENCH = os.path.dirname(HERE)                 # inference/benchmarks
INFERENCE = os.path.dirname(BENCH)            # inference
sys.path.insert(0, INFERENCE)

os.environ.setdefault("TOKENIZERS_PARALLELISM", "false")

# The committed corpus + labels (shared with the bge embedder adoption eval).
EVAL = json.load(open(os.path.join(BENCH, "coreml_eval", "eval_set.json")))
FACTS = EVAL["facts"]
QUERIES = EVAL["queries"]
FACT_IDS = [f["id"] for f in FACTS]
FACT_TEXTS = [f["text"] for f in FACTS]
FACT_POS = {fid: i for i, fid in enumerate(FACT_IDS)}
QUERY_TEXTS = [q["text"] for q in QUERIES]

KS = (20, 50)  # the two rerank shortlist depths measured


def l2norm(mat):
    mat = np.asarray(mat, dtype=np.float64)
    n = np.linalg.norm(mat, axis=1, keepdims=True)
    n = np.where(n < 1e-12, 1.0, n)
    return mat / n


# --------------------------------------------------------------------------
# Metrics over a RANKED list of fact ids (best first), binary relevance.
# --------------------------------------------------------------------------
def _dcg(ranked_ids, rel, k):
    s = 0.0
    for i, fid in enumerate(ranked_ids[:k]):
        if fid in rel:
            s += 1.0 / math.log2(i + 2)  # gain 1, discount log2(rank+1)
    return s


def ndcg_at_k(ranked_ids, rel, k):
    # Ideal DCG = all |rel| relevant facts packed at the top ranks (capped at k).
    n_ideal = min(len(rel), k)
    idcg = sum(1.0 / math.log2(i + 2) for i in range(n_ideal))
    dcg = _dcg(ranked_ids, rel, k)
    return dcg / idcg if idcg > 0 else 0.0


def metrics_for(rankings):
    """rankings: dict query_id -> ranked fact-id list (best first). Returns the
    aggregate summary over all queries."""
    ks = (1, 3, 5)
    agg = {f"recall@{k}": [] for k in ks}
    agg.update({f"success@{k}": [] for k in ks})
    rr, ndcg = [], []
    for q in QUERIES:
        rel = set(q["relevant"])
        ranked = rankings[q["id"]]
        first_rank = next((r + 1 for r, fid in enumerate(ranked) if fid in rel), None)
        rr.append(1.0 / first_rank if first_rank else 0.0)
        for k in ks:
            hit = len(set(ranked[:k]) & rel)
            agg[f"recall@{k}"].append(hit / len(rel))
            agg[f"success@{k}"].append(1.0 if hit > 0 else 0.0)
        ndcg.append(ndcg_at_k(ranked, rel, 10))
    summary = {k: round(float(np.mean(v)), 4) for k, v in agg.items()}
    summary["MRR"] = round(float(np.mean(rr)), 4)
    summary["nDCG@10"] = round(float(np.mean(ndcg)), 4)
    return summary


def dense_recall_at(dense_order, k):
    """Avg over queries of |relevant in dense top-k| / |relevant| — the CEILING a
    rerank of depth k can recover to (it cannot retrieve what A missed)."""
    vals = []
    for q in QUERIES:
        rel = set(q["relevant"])
        topk = set(dense_order[q["id"]][:k])
        vals.append(len(topk & rel) / len(rel))
    return round(float(np.mean(vals)), 4)


def probe(emb, rr, note=None):
    """MEASURE stage A (dense) vs stage B (dense top-K + rerank) over the committed
    corpus and return the full results dict (no file I/O, no printing). `emb` is a
    loaded coreml_embed.CoreMLEmbedder, `rr` a loaded coreml_rerank.CoreMLReranker.
    Reused by both this harness's main() and inference/benchmark.py so the baseline
    re-measures the SAME two-stage numbers, never a stale copy."""
    from coreml_rerank import MODEL_ID as RERANK_MODEL_ID
    from coreml_rerank import RERANKER_ID, SEQ
    from coreml_embed import EMBEDDER_ID, MODEL_ID as EMBED_MODEL_ID

    if note:
        print(note, flush=True)
    fact_vecs = np.array(emb.embed(FACT_TEXTS), dtype=np.float32)
    query_vecs = np.array(emb.embed(QUERY_TEXTS), dtype=np.float32)

    F = l2norm(fact_vecs)
    Q = l2norm(query_vecs)
    sims = Q @ F.T  # [n_queries, n_facts] cosine

    # ---- Ranking A: dense cosine over all facts --------------------------
    dense_order = {}
    for qi, q in enumerate(QUERIES):
        order = np.argsort(-sims[qi])
        dense_order[q["id"]] = [FACT_IDS[i] for i in order]

    reranked_order = {k: {} for k in KS}
    rerank_latencies = {k: [] for k in KS}
    for q in QUERIES:
        qid = q["id"]
        full = dense_order[qid]
        for k in KS:
            shortlist = full[:k]
            passages = [FACT_TEXTS[FACT_POS[fid]] for fid in shortlist]
            t = time.perf_counter()
            scores = rr.rerank(q["text"], passages)
            rerank_latencies[k].append((time.perf_counter() - t) * 1000.0)
            # Reorder the shortlist by cross-encoder score (desc); the tail below rank
            # k keeps its dense order (a real reranker only reorders the shortlist).
            new_head = [fid for _, fid in sorted(
                zip(scores, shortlist), key=lambda p: -p[0])]
            reranked_order[k][qid] = new_head + full[k:]

    # ---- summaries -------------------------------------------------------
    summary_A = metrics_for(dense_order)
    summary_B = {k: metrics_for(reranked_order[k]) for k in KS}
    dense_recall_K = {k: dense_recall_at(dense_order, k) for k in KS}
    lat = {k: {
        "median_ms": round(statistics.median(rerank_latencies[k]), 2),
        "p90_ms": round(sorted(rerank_latencies[k])[int(0.9 * (len(rerank_latencies[k]) - 1))], 2),
        "per_pair_ms": round(statistics.median(rerank_latencies[k]) / k, 2),
    } for k in KS}

    # ---- verdict derived from the numbers, not hand-set ------------------
    # GO iff the rerank MEASURABLY improves ranking at some K without regressing the
    # top-5 recall: nDCG@10 up AND (recall@1 up OR MRR up), recall@5 not worse.
    def wins(b):
        return (
            b["nDCG@10"] > summary_A["nDCG@10"]
            and (b["recall@1"] > summary_A["recall@1"] or b["MRR"] > summary_A["MRR"])
            and b["recall@5"] >= summary_A["recall@5"] - 1e-9
        )
    go = any(wins(summary_B[k]) for k in KS)
    best_k = max(KS, key=lambda k: (summary_B[k]["nDCG@10"], summary_B[k]["recall@1"]))

    results = {
        "eval_meta": {
            "about": ("Two-stage retrieval measure-first probe: bge dense (A) vs bge "
                      "retrieve top-K + Core ML cross-encoder rerank (B). SYNTHETIC-but-"
                      "representative (Claude-authored labels), directional build/no-build "
                      "evidence, NOT ground-truth production data."),
            "corpus_facts": len(FACTS),
            "queries": len(QUERIES),
            "relevant_per_query_min": min(len(q["relevant"]) for q in QUERIES),
            "relevant_per_query_max": max(len(q["relevant"]) for q in QUERIES),
            "stage_a_embedder": EMBEDDER_ID,
            "stage_a_model": EMBED_MODEL_ID,
            "stage_b_reranker": RERANKER_ID,
            "stage_b_model": RERANK_MODEL_ID,
            "stage_b_seq": SEQ,
            "rerank_depths_K": list(KS),
            "compute_units": "ComputeUnit.ALL (ANE-eligible; latency is measured "
                             "end-to-end, never a claim any op ran on the ANE)",
            "metric_defs": {
                "recall@k": "avg over queries of |relevant in top-k| / |relevant|",
                "success@k": "avg over queries of 1[any relevant in top-k]",
                "MRR": "mean reciprocal rank of the first relevant fact",
                "nDCG@10": "avg over queries of DCG@10 / ideal-DCG@10 (binary gain)",
                "dense_recall@K": "avg over queries of |relevant in dense top-K| / "
                                  "|relevant| — the ceiling a rerank of depth K can "
                                  "recover to (rerank reorders, never retrieves)",
            },
        },
        "A_dense": {"name": "bge dense cosine over all facts (shipped behavior)",
                    "summary": summary_A},
        "B_reranked": {
            str(k): {
                "name": f"bge top-{k} + cross-encoder rerank",
                "dense_recall@K": dense_recall_K[k],
                "rerank_latency": lat[k],
                "summary": summary_B[k],
            } for k in KS
        },
        "verdict": {
            "go": bool(go),
            "best_K": best_k,
            "deltas_best_minus_A": {
                m: round(summary_B[best_k][m] - summary_A[m], 4)
                for m in ("nDCG@10", "recall@1", "recall@3", "recall@5", "MRR")
            },
            "call": (
                f"GO: the cross-encoder rerank MEASURABLY improves ranking "
                f"(best at K={best_k}); ship it config-gated on top of the bge "
                f"bi-encoder as the standard two-stage stack."
                if go else
                "NO-GO: the cross-encoder rerank does NOT measurably improve ranking "
                "over the bge dense order on this eval; the latency cost is not worth "
                "it. Do NOT ship (same discipline that dropped speculative decoding)."
            ),
            "honesty": ("SYNTHETIC-but-representative eval, directional evidence not a "
                        "production guarantee. recall@k (k<=K) after rerank is bounded "
                        "by dense_recall@K. Latency is measured end-to-end under "
                        "ComputeUnit.ALL; ANE-eligible, never claimed to run on the ANE."),
        },
    }
    return results


def main():
    from coreml_embed import CoreMLEmbedder
    from coreml_rerank import CoreMLReranker, MODEL_ID as RERANK_MODEL_ID

    print("Stage A: embedding corpus + queries with the shipped Core ML bge ...", flush=True)
    emb = CoreMLEmbedder()
    emb.ensure_loaded()
    print("Stage B: reranking dense top-K with the Core ML cross-encoder ...", flush=True)
    rr = CoreMLReranker()
    rr.ensure_loaded()

    results = probe(emb, rr)
    out = os.path.join(HERE, "results.json")
    json.dump(results, open(out, "w"), indent=2)

    summary_A = results["A_dense"]["summary"]
    summary_B = {k: results["B_reranked"][str(k)]["summary"] for k in KS}
    dense_recall_K = {k: results["B_reranked"][str(k)]["dense_recall@K"] for k in KS}
    lat = {k: results["B_reranked"][str(k)]["rerank_latency"] for k in KS}
    best_k = results["verdict"]["best_K"]

    # ---- print table -----------------------------------------------------
    cols = ["nDCG@10", "recall@1", "recall@3", "recall@5", "MRR"]
    print("\n" + "=" * 92)
    print(f"TWO-STAGE RETRIEVAL  |  corpus={len(FACTS)} facts, {len(QUERIES)} queries")
    print(f"A = bge dense (shipped)   B = bge top-K + cross-encoder rerank ({RERANK_MODEL_ID})")
    print("=" * 92)
    hdr = f"{'ranking':<34}" + "".join(f"{c:>11}" for c in cols)
    print(hdr)
    print("-" * len(hdr))
    print(f"{'A: dense (all facts)':<34}" + "".join(f"{summary_A[c]:>11.4f}" for c in cols))
    for k in KS:
        s = summary_B[k]
        print(f"{f'B: rerank top-{k}':<34}" + "".join(f"{s[c]:>11.4f}" for c in cols))
    print("-" * len(hdr))
    for k in KS:
        print(f"  dense recall@{k} (ceiling) = {dense_recall_K[k]:.4f}   "
              f"rerank latency median = {lat[k]['median_ms']:.1f} ms "
              f"({lat[k]['per_pair_ms']:.2f} ms/pair)")
    print("-" * len(hdr))
    print(f"VERDICT: {results['verdict']['call']}")
    print(f"deltas (best K={best_k} minus A): {results['verdict']['deltas_best_minus_A']}")
    print(f"\nsaved -> {out}")


if __name__ == "__main__":
    main()
