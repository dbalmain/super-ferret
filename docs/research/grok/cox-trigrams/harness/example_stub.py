#!/usr/bin/env python3
"""CLI skeleton. Replace the function bodies. Speaks the course protocol so
the harness can run against you from the first compiling binary.

  python3 example_stub.py match a a
"""

import json
import sys


def cmd_match(pattern, text):
    # Whole-string match. Return True/False.
    return False


def cmd_search(pattern, text):
    # Substring match.
    return False


def cmd_submatch(pattern, text):
    # Return (matched, groups or None).
    # groups[0] is [start, end) of the whole match; then each '('.
    return False, None


def cmd_index_search(corpus_dir, pattern):
    # Return (candidates, hits), each a list of basenames.
    return [], []


def main(argv):
    if len(argv) < 2:
        print("usage: match|search|submatch|index-search ...", file=sys.stderr)
        return 1
    cmd = argv[1]
    if cmd in ("match", "search", "submatch"):
        if len(argv) != 4:
            print(f"usage: {cmd} PATTERN TEXT", file=sys.stderr)
            return 1
        pattern, text = argv[2], argv[3]
        if cmd == "submatch":
            ok, groups = cmd_submatch(pattern, text)
            print(json.dumps({"match": bool(ok), "groups": groups}))
        else:
            fn = cmd_match if cmd == "match" else cmd_search
            print(json.dumps({"match": bool(fn(pattern, text))}))
        return 0
    if cmd == "index-search":
        if len(argv) != 5 or argv[2] != "--corpus":
            print("usage: index-search --corpus DIR PATTERN", file=sys.stderr)
            return 1
        cands, hits = cmd_index_search(argv[3], argv[4])
        print(json.dumps({"candidates": cands, "hits": hits}))
        return 0
    print(f"unknown command {cmd}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
