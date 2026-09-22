#!/usr/bin/env python3
"""Language-agnostic test runner for the Cox regexp course.

Your implementation is any executable that speaks the CLI documented in
../index.html. This file never imports your code.

Examples:
  python3 run_tests.py --bin ./nfa nfa
  python3 run_tests.py --bin ./vm nfa submatch
  python3 run_tests.py --bin ./searcher index
  python3 run_tests.py --bin ./prog all
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
TESTDATA = os.path.join(HERE, "testdata")
CORPUS = os.path.join(HERE, "corpus")

STAGES = ("nfa", "dfa", "vm", "submatch", "search", "syntax", "index")


def load_json(name):
    path = os.path.join(TESTDATA, name)
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def run_bin_raw(binpath, args, timeout):
    """Run the binary. Returns (CompletedProcess, None) or (None, message)."""
    try:
        proc = subprocess.run(
            [binpath, *args],
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except FileNotFoundError:
        return None, f"executable not found: {binpath}"
    except subprocess.TimeoutExpired:
        return None, f"timeout after {timeout}s"
    return proc, None


def run_bin(binpath, args, timeout):
    """Run the binary and parse one JSON object from stdout."""
    proc, err = run_bin_raw(binpath, args, timeout)
    if err:
        return None, err
    if proc.returncode == 2:
        return None, f"exit 2 (bad regexp). stderr: {proc.stderr.strip()[:400]}"
    if proc.returncode != 0:
        return None, f"exit {proc.returncode}. stderr: {proc.stderr.strip()[:400]}"
    raw = proc.stdout.strip()
    if not raw:
        return None, "empty stdout (debug belongs on stderr)"
    try:
        return json.loads(raw), None
    except json.JSONDecodeError as e:
        return None, f"stdout is not JSON: {e}: {raw[:200]!r}"


def fmt_case(case):
    p = case.get("pattern", "")
    t = case.get("text", "")
    return f"{p!r} vs {t!r}"


def check_groups_field(groups, want):
    """The protocol is as strict as the course text: groups is null on a
    non-match, else an array of [start, end) integer pairs or null."""
    if want is None:
        if groups is not None:
            return f"expected 'groups': null on a non-match, got {groups!r}"
        return None
    if not isinstance(groups, list):
        return f"expected 'groups' to be an array, got {groups!r}"
    for i, g in enumerate(groups):
        if g is None:
            continue
        if (
            not isinstance(g, list)
            or len(g) != 2
            or not all(isinstance(x, int) and not isinstance(x, bool) for x in g)
        ):
            return f"group {i}: expected [start, end] integers or null, got {g!r}"
    if groups != want:
        return f"groups: got {groups!r}, want {want!r}"
    return None


def check_match_field(got, want_match):
    if not isinstance(got, dict) or "match" not in got:
        return f"expected object with boolean 'match', got {got!r}"
    if not isinstance(got["match"], bool):
        return f"expected boolean 'match', got {got['match']!r}"
    if got["match"] != want_match:
        return f"match: got {got['match']!r}, want {want_match!r}"
    return None


def run_match_file(binpath, filename, command, timeout):
    data = load_json(filename)
    failed = []
    passed = 0
    for case in data["cases"]:
        if case.get("skip"):
            continue
        tmo = case.get("timeout_s", timeout)
        got, err = run_bin(binpath, [command, case["pattern"], case["text"]], tmo)
        if err:
            failed.append((fmt_case(case), err))
            continue
        err = check_match_field(got, case["match"])
        if err:
            failed.append((fmt_case(case), err))
        else:
            passed += 1
    return passed, failed


def run_submatch(binpath, timeout):
    data = load_json("submatch.json")
    failed = []
    passed = 0
    for case in data["cases"]:
        if case.get("skip"):
            continue
        got, err = run_bin(
            binpath, ["submatch", case["pattern"], case["text"]], timeout
        )
        if err:
            failed.append((fmt_case(case), err))
            continue
        err = check_match_field(got, case["match"])
        if err:
            failed.append((fmt_case(case), err))
            continue
        err = check_groups_field(got.get("groups"), case["groups"])
        if err:
            failed.append((fmt_case(case), err))
        else:
            passed += 1
    return passed, failed


def run_index(binpath, timeout):
    data = load_json("index.json")
    corpus_names = {
        name
        for name in os.listdir(CORPUS)
        if os.path.isfile(os.path.join(CORPUS, name))
    }
    failed = []
    passed = 0
    for case in data["cases"]:
        if case.get("skip"):
            continue
        got, err = run_bin(
            binpath,
            ["index-search", "--corpus", CORPUS, case["pattern"]],
            timeout,
        )
        if err:
            failed.append((case["pattern"], err))
            continue
        if not isinstance(got, dict):
            failed.append((case["pattern"], f"expected object, got {got!r}"))
            continue
        hits = got.get("hits")
        cands = got.get("candidates")
        if (
            not isinstance(hits, list)
            or not all(isinstance(x, str) for x in hits)
            or not isinstance(cands, list)
            or not all(isinstance(x, str) for x in cands)
        ):
            failed.append(
                (
                    case["pattern"],
                    "expected 'hits' and 'candidates' to be arrays of strings",
                )
            )
            continue
        hits_s = sorted(hits)
        cands_s = sorted(cands)
        if len(hits_s) != len(set(hits_s)) or len(cands_s) != len(set(cands_s)):
            failed.append(
                (case["pattern"], "'hits' and 'candidates' must not contain duplicates")
            )
            continue
        unknown_names = sorted((set(hits_s) | set(cands_s)) - corpus_names)
        if unknown_names:
            failed.append(
                (
                    case["pattern"],
                    f"expected corpus basenames, got unknown names {unknown_names}",
                )
            )
            continue
        want_hits = sorted(case["hits"])
        if hits_s != want_hits:
            failed.append(
                (case["pattern"], f"hits: got {hits_s}, want {want_hits}")
            )
            continue
        missing = [h for h in want_hits if h not in cands_s]
        if missing:
            failed.append(
                (
                    case["pattern"],
                    f"candidates dropped true hits {missing} "
                    f"(candidates={cands_s}). The index must be a superset.",
                )
            )
            continue
        if "candidates" in case:
            want_c = sorted(case["candidates"])
            if cands_s != want_c:
                failed.append(
                    (
                        case["pattern"],
                        f"candidates: got {cands_s}, want {want_c}",
                    )
                )
                continue
        passed += 1
    return passed, failed


def run_syntax(binpath, timeout):
    """Malformed patterns must be rejected with exit 2, not reinterpreted.

    The course requires this; nothing else in the suite exercises it, so a
    parser that silently treats '(' as a literal passes every other stage.
    """
    data = load_json("syntax.json")
    failed = []
    passed = 0
    for case in data["cases"]:
        if case.get("skip"):
            continue
        proc, err = run_bin_raw(
            binpath, ["match", case["pattern"], case.get("text", "")], timeout
        )
        if err:
            failed.append((repr(case["pattern"]), err))
            continue
        if proc.returncode == 2:
            passed += 1
        elif proc.returncode == 0:
            failed.append(
                (
                    repr(case["pattern"]),
                    f"accepted a malformed pattern (exit 0, stdout "
                    f"{proc.stdout.strip()[:80]!r}); want exit 2",
                )
            )
        else:
            failed.append(
                (
                    repr(case["pattern"]),
                    f"exit {proc.returncode}; want exit 2 for a bad regexp. "
                    f"stderr: {proc.stderr.strip()[:200]}",
                )
            )
    return passed, failed


def run_stage(binpath, stage, timeout):
    if stage in ("nfa", "dfa", "vm"):
        p1, f1 = run_match_file(binpath, "match.json", "match", timeout)
        p2, f2 = run_match_file(binpath, "pathological.json", "match", timeout)
        return p1 + p2, f1 + f2
    if stage == "search":
        return run_match_file(binpath, "search.json", "search", timeout)
    if stage == "submatch":
        return run_submatch(binpath, timeout)
    if stage == "syntax":
        return run_syntax(binpath, timeout)
    if stage == "index":
        return run_index(binpath, timeout)
    raise SystemExit(f"unknown stage {stage}")


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--bin", required=True, help="path to your executable")
    p.add_argument(
        "stages",
        nargs="+",
        help="nfa dfa vm submatch search syntax index  (or 'all')",
    )
    p.add_argument("--timeout", type=float, default=2.0, help="seconds per case")
    args = p.parse_args()
    stages = list(args.stages)
    if stages == ["all"]:
        stages = list(STAGES)
    unknown = [s for s in stages if s not in STAGES]
    if unknown:
        raise SystemExit(f"unknown stages {unknown}; choose from {STAGES}")

    binpath = os.path.abspath(args.bin)
    any_fail = False
    for stage in stages:
        t0 = time.time()
        passed, failed = run_stage(binpath, stage, args.timeout)
        dt = time.time() - t0
        if failed:
            any_fail = True
            print(f"FAIL  {stage}  {passed} passed, {len(failed)} failed  ({dt:.2f}s)")
            for name, err in failed:
                print(f"  - {name}: {err}")
        else:
            print(f"ok    {stage}  {passed} passed  ({dt:.2f}s)")
    sys.exit(1 if any_fail else 0)


if __name__ == "__main__":
    main()
