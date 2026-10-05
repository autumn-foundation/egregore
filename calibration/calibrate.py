#!/usr/bin/env python3
"""Issue #221 calibration: measure top-score distributions for answerable vs
unanswerable queries against a freshly embedded egregore checkout.

Usage: calibrate.py <eg-binary> <data-dir> <repo-corpus> <unanswerable-set>

For every query it runs `eg query semantic --format json` and records the
verdict's best_score. It then reports, per class, the score distribution and
how the candidate thresholds (confident >= 0.55, weak in [0.50, 0.55),
abstain < 0.50) classify each class.
"""
import json
import subprocess
import sys

EG, DATA_DIR, CORPUS, UNANSWERABLE = sys.argv[1:5]


def best_score_for(query_text):
    out = subprocess.run(
        [EG, "query", "semantic", query_text, "--data-dir", DATA_DIR,
         "--format", "json"],
        capture_output=True, text=True, timeout=300,
    )
    if out.returncode != 0:
        return ("error", out.returncode, (out.stderr or out.stdout)[-200:])
    verdict = None
    first_row_score = None
    for line in out.stdout.splitlines():
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(obj, dict) and "confidence" in obj:
            verdict = obj["confidence"]
        elif isinstance(obj, dict) and "score" in obj and first_row_score is None:
            first_row_score = obj["score"]
    if verdict is not None:
        return ("ok", verdict["verdict"], float(verdict["best_score"]))
    if first_row_score is not None:
        return ("ok", "no-verdict-line", float(first_row_score))
    return ("empty", None, None)


def classify(score):
    if score >= 0.39:
        return "confident"
    if score >= 0.34:
        return "weak"
    return "abstain"


def main():
    corpus = json.load(open(CORPUS))["queries"]
    held_out = json.load(open(UNANSWERABLE))["queries"]

    answerable = [q for q in corpus if q["class"] != "ambiguous"]
    ambiguous = [q for q in corpus if q["class"] == "ambiguous"]

    groups = [
        ("answerable-corpus", answerable),
        ("ambiguous-corpus", ambiguous),
        ("held-out-unanswerable", held_out),
    ]
    results = {}
    for name, queries in groups:
        rows = []
        for q in queries:
            status, verdict, score = best_score_for(q["text"])
            rows.append({"id": q["id"], "status": status,
                         "verdict": verdict, "best_score": score})
            print(f"[{name}] {q['id']}: {status} {verdict} {score}", flush=True)
        results[name] = rows

    json.dump(results, open("scores.json", "w"), indent=1)

    print("\n=== distribution ===")
    for name, rows in results.items():
        ok = [r for r in rows if r["status"] == "ok"]
        dist = {}
        for r in ok:
            dist[classify(r["best_score"])] = dist.get(classify(r["best_score"]), 0) + 1
        errs = [r for r in rows if r["status"] != "ok"]
        scores = sorted(r["best_score"] for r in ok)
        print(f"{name}: n={len(rows)} ok={len(ok)} errs={len(errs)}")
        print(f"  bands: {dist}")
        if scores:
            print(f"  min={scores[0]:.4f} p25={scores[len(scores)//4]:.4f} "
                  f"median={scores[len(scores)//2]:.4f} max={scores[-1]:.4f}")

    # Acceptance check from the issue: >=90% of unanswerable land in
    # abstain|weak, >=85% of answerable land in confident.
    unans = [r for r in results["ambiguous-corpus"] + results["held-out-unanswerable"]
             if r["status"] == "ok"]
    ans = [r for r in results["answerable-corpus"] if r["status"] == "ok"]
    unans_ok = sum(1 for r in unans if classify(r["best_score"]) in ("abstain", "weak"))
    ans_ok = sum(1 for r in ans if classify(r["best_score"]) == "confident")
    print("\n=== acceptance ===")
    if unans:
        print(f"unanswerable abstain|weak: {unans_ok}/{len(unans)} "
              f"({unans_ok/len(unans)*100:.1f}% need >=90%)")
    else:
        print(f"unanswerable abstain|weak: NO SUCCESSFUL QUERIES ({len(results['ambiguous-corpus']) + len(results['held-out-unanswerable'])} attempted)")
    if ans:
        print(f"answerable confident: {ans_ok}/{len(ans)} "
              f"({ans_ok/len(ans)*100:.1f}% need >=85%)")
    else:
        print(f"answerable confident: NO SUCCESSFUL QUERIES ({len(results['answerable-corpus'])} attempted)")


if __name__ == "__main__":
    main()
