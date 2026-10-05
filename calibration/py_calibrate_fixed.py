#!/usr/bin/env python3
"""Issue #221 repaired calibration (2026-09-23).

Measures top-score distributions for answerable vs unanswerable queries
against a reduced egregore checkout, using the EXACT candidate population
that `candidate_from_record` (src/embeddings.rs) selects: File and Symbol
nodes only. The 2026-09-22 run embedded every node with non-empty text
(4,501 nodes incl. Diagnostics/PanicRiskSites vs 15,098 actual candidates)
and omitted src/cli/mod.rs, invalidating its thresholds. This script fixes
both flaws.

Candidate text uses the exact `candidate_text` format:
    summary + "\npath: {path}" + "\nname: {name}"

Stale corpus paths are mapped to current locations:
    src/cli.rs   -> src/cli/mod.rs
    src/query.rs -> src/query/mod.rs

Threshold rule (measured, documented for both boundaries):
    T_conf = floor(5th percentile of answerable best scores, 2dp)
             -> >=95% of answerable classify confident (bar: >=85%)
    T_weak = ceil(90th percentile of unanswerable best scores, 2dp)
             -> >=90% of unanswerable classify abstain (bar: >=90% weak|abstain)
    Requires T_weak < T_conf, else the classes do not separate.

Usage: py_calibrate_fixed.py <graph.jsonl> <relevance-corpus> <unanswerable-set>
Writes py-fixed-scores.json and py-fixed-top3.json in cwd.
"""
import json
import math
import sys

GRAPH, CORPUS, UNANSWERABLE = sys.argv[1:4]


def candidate_text(summary, path, name):
    parts = [summary or ""]
    if path:
        parts.append(f"path: {path}")
    if name:
        parts.append(f"name: {name}")
    return "\n".join(parts)


def load_candidates(graph_path):
    """Exactly the candidate_from_record population: File/Symbol nodes."""
    ids, docs = [], []
    with open(graph_path) as f:
        for line in f:
            r = json.loads(line)
            if r.get("record_type") != "node":
                continue
            if r.get("kind") not in ("File", "Symbol"):
                continue
            text = candidate_text(r.get("summary"), r.get("repo_relative_path"), r.get("name"))
            if text.strip():
                ids.append(r.get("id"))
                docs.append(text)
    return ids, docs


def percentile(sorted_vals, pct):
    if not sorted_vals:
        return None
    idx = min(int(pct / 100 * len(sorted_vals)), len(sorted_vals) - 1)
    return sorted_vals[idx]


def main():
    from sentence_transformers import SentenceTransformer
    import numpy as np

    print("Loading model...", flush=True)
    model = SentenceTransformer("sentence-transformers/all-MiniLM-L6-v2")

    print("Loading candidates (File/Symbol nodes only)...", flush=True)
    ids, docs = load_candidates(GRAPH)
    print(f"Candidates: {len(docs)}", flush=True)
    assert docs, "no candidates found - filter is wrong"

    corpus = json.load(open(CORPUS))["queries"]
    held_out = json.load(open(UNANSWERABLE))["queries"]
    answerable = [q for q in corpus if q["class"] != "ambiguous"]
    ambiguous = [q for q in corpus if q["class"] == "ambiguous"]

    print("Embedding candidates...", flush=True)
    doc_emb = model.encode(docs, show_progress_bar=False, batch_size=32)
    doc_emb = doc_emb / np.linalg.norm(doc_emb, axis=1, keepdims=True)

    def top3(query_text):
        q_emb = model.encode([query_text], show_progress_bar=False)
        q_emb = q_emb / np.linalg.norm(q_emb)
        sims = doc_emb @ q_emb[0]
        top_idx = np.argsort(sims)[-3:][::-1]
        return [(ids[i], float(sims[i])) for i in top_idx]

    results, tops = {}, {}
    for name, queries in [("answerable", answerable), ("ambiguous", ambiguous),
                          ("held-out", held_out)]:
        print(f"\n=== {name} ===", flush=True)
        rows = []
        for q in queries:
            hits = top3(q["text"])
            s = hits[0][1]
            rows.append({"id": q["id"], "score": round(s, 4)})
            tops[q["id"]] = [{"record_id": rid, "score": round(sc, 4)} for rid, sc in hits]
            print(f"{q['id']}: {s:.4f}", flush=True)
        results[name] = rows

    json.dump(results, open("py-fixed-scores.json", "w"), indent=1)
    json.dump(tops, open("py-fixed-top3.json", "w"), indent=1)

    ans = sorted(r["score"] for r in results["answerable"])
    # The unanswerable set is the held-out set ONLY. The "ambiguous" queries
    # are excluded from threshold calibration: by definition their
    # answerability is unknown, so including them (e.g. q027 at 0.5080)
    # would contaminate the unanswerable distribution.
    unans = sorted(r["score"] for r in results["held-out"])
    print(f"\nanswerable (n={len(ans)}): min={ans[0]:.4f} p5={percentile(ans,5):.4f} "
          f"p10={percentile(ans,10):.4f} median={percentile(ans,50):.4f} max={ans[-1]:.4f}")
    print(f"held-out unanswerable (n={len(unans)}): min={unans[0]:.4f} median={percentile(unans,50):.4f} "
          f"p90={percentile(unans,90):.4f} p95={percentile(unans,95):.4f} max={unans[-1]:.4f}")

    t_conf = math.floor(percentile(ans, 5) * 100) / 100
    t_weak = math.ceil(percentile(unans, 90) * 100) / 100
    print(f"\nT_conf (floor p5 answerable) = {t_conf:.2f}")
    print(f"T_weak (ceil p90 unanswerable) = {t_weak:.2f}")
    if not (t_weak < t_conf):
        print("FAIL: classes do not separate (T_weak >= T_conf)")
        sys.exit(1)

    def cls(s):
        return "confident" if s >= t_conf else ("weak" if s >= t_weak else "abstain")

    ans_conf = sum(1 for s in ans if cls(s) == "confident")
    unans_below = sum(1 for s in unans if cls(s) in ("weak", "abstain"))
    unans_abst = sum(1 for s in unans if cls(s) == "abstain")
    print(f"answerable confident: {ans_conf}/{len(ans)} ({ans_conf/len(ans)*100:.1f}%, bar >=85%)")
    print(f"unanswerable weak|abstain: {unans_below}/{len(unans)} ({unans_below/len(unans)*100:.1f}%, bar >=90%)")
    print(f"unanswerable abstain: {unans_abst}/{len(unans)} ({unans_abst/len(unans)*100:.1f}%)")
    print(f"THRESHOLDS {t_conf:.2f} {t_weak:.2f}")


if __name__ == "__main__":
    main()
