#!/usr/bin/env python3
"""D65C: serialize real builds, lifecycle queries and S2's 100-round churn.

Use an isolated refreshed catalog and index-freeze corpus in --run-dir.
Every index is a fresh follow, never a converted codec-1 index. Logs contain
classes, counts and timings, never source paths, terms or document bytes.
"""

import argparse
import filecmp
from pathlib import Path
import shutil
import subprocess


ROOT = Path(__file__).resolve().parents[3]
CODECS = {"control": "1", "h0": "h", "c1": "c1", "c2": "c2", "c3": "c3"}


def run(command, log):
    for binary in ("ferret-bench", "term-dict-bench", "pair-fp-bench"):
        result = subprocess.run(["pgrep", "-x", binary], capture_output=True)
        if result.returncode != 1:
            raise RuntimeError(f"preflight refused: {binary} active or pgrep failed")
    print(f"running {command[1]} -> {log.name}", flush=True)
    with log.open("w") as output:
        subprocess.run(command, stdout=output, stderr=subprocess.STDOUT, check=True)
    print(f"finished {log.name}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("arm", choices=CODECS)
    parser.add_argument("phase", choices=("build", "query", "churn", "all"))
    parser.add_argument("--run-dir", type=Path, required=True)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/ferret-bench")
    parser.add_argument("--cleanup-states", action="store_true")
    args = parser.parse_args()
    base, arm, binary = args.run_dir, args.arm, str(args.binary)
    catalog, corpus, index = base / "catalog", base / "corpus", base / arm
    if args.phase in ("build", "all"):
        run([binary, "index-build", str(catalog), str(index), "--codec", CODECS[arm],
             "--corpus", str(corpus), "--snapshots"], base / f"{arm}-build.log")
    if args.phase in ("query", "all"):
        for state in ("first", "steady", "full"):
            path = index if state == "full" else index.with_suffix(f".{state}")
            command = [binary, "content-query", str(catalog), str(path),
                       "df1", "df2-100", "absent", "mid", "common", "--terms",
                       str(base / "control"), "--corpus", str(corpus)]
            if arm != "control":
                command += ["--compare", str(base / "control")]
            run(command, base / f"{arm}-{state}-query.log")
            if args.cleanup_states and state != "full":
                shutil.rmtree(path)
        # The fully merged trigram bytes are independent of term codec. Share
        # only a byte-identical file after measurements, keeping valid indexes.
        if arm != "control":
            reference = list((base / "control").glob("*.tri"))
            outputs = list(index.glob("*.tri"))
            if len(reference) == len(outputs) == 1 and filecmp.cmp(reference[0], outputs[0], shallow=False):
                temporary = outputs[0].with_suffix(".dedup")
                temporary.hardlink_to(reference[0])
                temporary.replace(outputs[0])
    if args.phase in ("churn", "all"):
        run([binary, "index-churn", str(base / f"{arm}-churn"), "100", "--codec",
             CODECS[arm]], base / f"{arm}-churn.log")


if __name__ == "__main__":
    main()
