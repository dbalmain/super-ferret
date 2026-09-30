#!/usr/bin/env python3
"""`ferret find` through the release binary: the S1a before/after harness.

Reproduce the before/after table (one benchmark at a time on the machine; the
numbers are wall-clock):

    cargo build --release -p ferret -p ferret-bench \\
        -p ferret-crawl --example dump -p ferret-catalog --example synthetic
    S=/path/to/scratch                     # anywhere with ~3 GB free
    mkdir -p $S/xdg

    # The $HOME catalog, indexed with the default ignore rules in an isolated
    # config, so nothing under the real ~/.config/ferret is read or written.
    env -u FERRET_INDEX XDG_CONFIG_HOME=$S/xdg/config XDG_DATA_HOME=$S/xdg/data \\
        XDG_STATE_HOME=$S/xdg/state XDG_CACHE_HOME=$S/xdg/cache \\
        target/release/ferret --index $S/cat-home index $HOME

    # The synthetic 10M: a walker dump with lstat columns, replicated 23 times.
    target/release/examples/dump --stat $HOME > $S/home-dump-stat.tsv
    target/release/examples/synthetic $S/home-dump-stat.tsv $S/cat-10m 23

    # The measurement, once per catalog. --xdg is the same directory the index
    # was built with, so the query log lands under it.
    for c in home 10m; do
        crates/ferret-bench/scripts/findbench.py \\
            --bin target/release/ferret --bench target/release/ferret-bench \\
            --index $S/cat-$c --xdg $S/xdg --label "$c, after" --out $S/find-$c.md
    done

For the after-numbers, rebuild both catalogs with the new binaries from the
same dump and the same $HOME, then run the same loop and compare the tables.

Per query it measures:
  evicted  posix_fadvise(DONTNEED) on the catalog (checked with fincore), then
           one fresh process; median of --evicted-runs.
  fresh    a fresh process on a warm page cache; median of --warm-runs after
           one warm-up run.
  in-proc  the same runs' total_us / first_row_us from the query log, which
           exclude exec, dynamic loading and teardown.
  max RSS  ru_maxrss from wait4 (KiB), the child's peak, printed in MiB. It
           includes a floor from this interpreter's own image at fork (the
           `--version` row measures it: subtract that, or compare like with
           like). Bytes read are decimal MB.

With --bench it appends `ferret-bench sections` (exact bytes and bytes per name
per section) and `ferret-bench open` (open and load_all, warm and evicted).
Every ferret run gets XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_STATE_HOME and
XDG_CACHE_HOME under --xdg, and FERRET_INDEX unset.
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
import time

# The D40 mix, as arguments to `ferret find`. `--version` is the RSS floor.
QUERIES = [
    ["case:Flamegraph"],
    ["flamegraph"],
    ["test"],
    ["ext:jpg"],
    ["src/**/*.rs"],
    [r"re:^test_.*\.py$"],
    [r"re:^[0-9a-f]{8}$"],
    ["mtime:<1d"],
    ["size:>100M"],
    ["ext:rs", "size:>10k"],
    ["*"],
]


def evict(catalog):
    fd = os.open(catalog, os.O_RDONLY)
    try:
        os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
    finally:
        os.close(fd)


def resident(catalog):
    out = subprocess.run(
        ["fincore", "-nb", "-o", "RES", catalog], capture_output=True, text=True
    )
    return int(out.stdout.split()[0])


def busy():
    """Whether a compile is running: it would perturb every timing."""
    return any(
        subprocess.run(["pgrep", "-x", name], capture_output=True).returncode == 0
        for name in ("cargo", "rustc")
    )


def loadavg():
    with open("/proc/loadavg") as f:
        return " ".join(f.read().split()[:3])


class Bench:
    def __init__(self, args):
        self.bin = os.path.abspath(args.bin)
        self.index = os.path.abspath(args.index)
        self.catalog = os.path.join(self.index, "catalog")
        xdg = os.path.abspath(args.xdg)
        self.env = dict(os.environ)
        self.env.pop("FERRET_INDEX", None)
        for var, sub in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
        ]:
            os.makedirs(os.path.join(xdg, sub), exist_ok=True)
            self.env[var] = os.path.join(xdg, sub)
        self.log = os.path.join(xdg, "state", "ferret", "log.jsonl")

    def run(self, argv, logged=True):
        t = time.perf_counter()
        p = subprocess.Popen(
            [self.bin, "--index", self.index, *argv],
            env=self.env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        _, status, ru = os.wait4(p.pid, 0)
        wall = (time.perf_counter() - t) * 1e3
        code = os.waitstatus_to_exitcode(status)
        result = dict(wall=wall, rss=ru.ru_maxrss, exit=code)
        if logged:
            with open(self.log, "rb") as f:
                f.seek(max(0, os.fstat(f.fileno()).st_size - 8192))
                line = json.loads(f.read().splitlines()[-1])
            assert line["query"] == argv[1:], (line["query"], argv)
            result.update(
                total=line["total_us"] / 1e3,
                first=(line["first_row_us"] or 0) / 1e3,
                rows=line["rows"],
                read=line["bytes_read"],
                stats=line.get("stats") or {},
                strategy=line["strategy"],
            )
        return result

    def measure(self, argv, evicted_runs, warm_runs):
        find = ["find", *argv]
        ev = []
        for _ in range(evicted_runs):
            evict(self.catalog)
            assert resident(self.catalog) == 0, "eviction failed"
            ev.append(self.run(find))
        self.run(find)  # warm-up
        fr = [self.run(find) for _ in range(warm_runs)]
        return ev, fr

    def floor(self, warm_runs):
        runs = [self.run(["--version"], logged=False) for _ in range(warm_runs)]
        return statistics.median(r["rss"] for r in runs)


def med(runs, key):
    return statistics.median(r[key] for r in runs)


def tool(cmd):
    out = subprocess.run(cmd, capture_output=True, text=True)
    if out.returncode != 0:
        sys.exit(f"{' '.join(cmd)} failed: {out.stderr}")
    return out.stdout


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--bin", required=True, help="the release ferret binary")
    ap.add_argument("--index", required=True, help="the catalog directory")
    ap.add_argument("--xdg", required=True, help="scratch directory for XDG dirs")
    ap.add_argument("--out", required=True, help="Markdown file to write")
    ap.add_argument("--label", required=True, help="a name for this run")
    ap.add_argument("--bench", help="the release ferret-bench binary (sections, open)")
    ap.add_argument("--evicted-runs", type=int, default=3)
    ap.add_argument("--warm-runs", type=int, default=5)
    ap.add_argument("--wait-quiet", action="store_true", help="wait out cargo/rustc")
    args = ap.parse_args()

    while args.wait_quiet and busy():
        print("cargo or rustc running; waiting", file=sys.stderr)
        time.sleep(60)
    load_before = loadavg()
    bench = Bench(args)
    floor = bench.floor(args.warm_runs)
    rows = []
    for q in QUERIES:
        ev, fr = bench.measure(q, args.evicted_runs, args.warm_runs)
        rows.append((q, ev, fr))
        print(
            " ".join(q),
            f"evicted {med(ev, 'wall'):.1f} ms, fresh {med(fr, 'wall'):.1f} ms, "
            f"in-proc {med(fr, 'total'):.1f} ms, rows {fr[0]['rows']}, "
            f"rss {med(fr, 'rss') / 1024:.0f} MiB",
            flush=True,
        )
    load_after = loadavg()

    with open(args.out, "w") as f:
        f.write(f"# ferret find, {args.label}\n\n")
        f.write(
            f"index `{bench.index}`, binary `{bench.bin}`; load average "
            f"{load_before} before, {load_after} after; cargo/rustc running "
            f"at the end: {busy()}.\n\n"
            f"Medians (evicted of {args.evicted_runs}, warm of {args.warm_runs}), "
            "ms. Evicted and fresh are process wall clock (exec to exit); "
            "in-proc is the query log's total_us, first is first_row_us. Max RSS "
            f"is the process's peak; a bare `--version` run reads "
            f"{floor / 1024:.0f} MiB on the same path, the floor.\n\n"
        )
        f.write(
            "| query | strategy | rows | evicted first / wall | fresh wall "
            "| in-proc first / total | max RSS | bytes read |\n"
        )
        f.write("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |\n")
        for q, ev, fr in rows:
            f.write(
                f"| `{' '.join(q)}` | {fr[0]['strategy']} | {fr[0]['rows']} | "
                f"{med(ev, 'first'):.1f} / {med(ev, 'wall'):.1f} | "
                f"{med(fr, 'wall'):.1f} | "
                f"{med(fr, 'first'):.1f} / {med(fr, 'total'):.1f} | "
                f"{med(fr, 'rss') / 1024:.0f} MiB | "
                f"{fr[0]['read'] / 1e6:.1f} MB |\n"
            )
        if args.bench:
            f.write("\n## Sections\n\n")
            f.write(tool([args.bench, "sections", bench.index]))
            f.write("\n## Open and load\n\n")
            f.write(tool([args.bench, "open", bench.index]))
    print(f"wrote {args.out}")


if __name__ == "__main__":
    main()
