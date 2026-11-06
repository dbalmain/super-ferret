#!/usr/bin/env python3
"""Run and time the exact extended correctness set, using this worktree's target."""

import json
from pathlib import Path
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
EXPECTED = {
    ("ferret_index", "lib", "cursor::tests::the_algebra_matches_kleenes_logic_on_many_random_trees"),
    ("ferret_index", "lib", "segment::tests::mutation_fuzz_long"),
    ("ferret_index", "lib", "store::tests::a_long_churn_stays_correct"),
    ("ferret_query", "lib", "find::tests::differential_against_pinned_gnu"),
    ("ferret_verify", "lib", "trigram::tests::every_match_satisfies_the_derived_query_long"),
    ("ferret", "lib", "protocol::tests::mutation_fuzz_long"),
    ("content", "test", "text_queries_equal_the_oracle_at_every_coverage_long"),
    ("find_m2a", "test", "exec_batch_sizes_against_pinned_gnu"),
    ("find_m2a", "test", "regex_dialects_against_pinned_gnu"),
    ("watch", "test", "generated_bursts_long"),
}
# The private crawl measurement stays local and needs an external catalog.
MEASUREMENTS = {
    ("ferret_crawl", "lib", "tests::index::fault_pass_timing"),
}


def main():
    started = time.monotonic()
    build = subprocess.run(
        ["cargo", "test", "--workspace", "--release", "--no-run",
         "--target-dir", str(ROOT / "target"), "--message-format=json"],
        cwd=ROOT, stdout=subprocess.PIPE, text=True, check=True,
    )
    found = {}
    excluded = set()
    for line in build.stdout.splitlines():
        artifact = json.loads(line)
        if artifact.get("reason") != "compiler-artifact":
            continue
        if not artifact.get("executable") or not artifact["profile"]["test"]:
            continue
        target = artifact["target"]
        listing = subprocess.check_output(
            [artifact["executable"], "--list", "--ignored"], cwd=ROOT, text=True,
        )
        for test in listing.splitlines():
            if not test.endswith(": test"):
                continue
            key = (target["name"], target["kind"][0], test.removesuffix(": test"))
            if key in MEASUREMENTS:
                excluded.add(key)
            elif key in found:
                raise RuntimeError(f"duplicate extended test: {key}")
            else:
                found[key] = artifact["executable"]
    if set(found) != EXPECTED:
        raise RuntimeError(
            f"extended set changed: missing={EXPECTED - found.keys()}, "
            f"unexpected={found.keys() - EXPECTED}"
        )
    print(f"Collected {len(found)} extended tests; excluded {len(excluded)} measurement(s)",
          flush=True)
    run_started = time.monotonic()
    failures = []
    for key, executable in sorted(found.items()):
        print(f"EXTENDED starting {key[0]}::{key[2]}", flush=True)
        before = time.monotonic()
        result = subprocess.run(
            [executable, key[2], "--exact", "--ignored", "--nocapture"], cwd=ROOT,
        )
        print(f"EXTENDED {key[0]}::{key[2]}: {time.monotonic() - before:.3f} s "
              f"(exit {result.returncode})", flush=True)
        if result.returncode:
            failures.append(key)
    print(f"EXTENDED count={len(found)} failed={len(failures)} "
          f"tests_wall={time.monotonic() - run_started:.3f} s "
          f"total_wall={time.monotonic() - started:.3f} s", flush=True)
    return bool(failures)


if __name__ == "__main__":
    sys.exit(main())
