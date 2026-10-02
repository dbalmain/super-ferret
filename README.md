# Super Ferret

Desktop search for Linux, in Rust: filenames, metadata, content and regex over a
personal machine's files, with a dense index and a light footprint.

Early days — this repository is the project's design record and, as it arrives,
its code.

- [Goals](docs/GOALS.md)
- [Roadmap](docs/ROADMAP.md)
- [Design](docs/DESIGN.md)
- [Decisions](docs/DECISIONS.md) — every question asked, with options, tradeoffs
  and the answer
- [Research](docs/research/) — the 2026-09 survey and measured baseline

`ferret index DIR` adds an indexed root. `ferret find [PATH...] [EXPRESSION]`
then uses GNU find syntax over catalog visibility, respecting ignore rules.
It keeps filesystem traversal order; metadata predicates use live `lstat`.
An explicitly named ignored start is walked live, with no nested ignore rules.
Use `ferret find -I ...` or `--no-ignore` for an unrestricted live walk without
an index. Add `find_no_ignore = true` to `~/.config/ferret/config` (or
`$XDG_CONFIG_HOME/ferret/config`) to make that mode the default.

A pasted `find … -delete` skips ignored files and still exits 0. Failed selected
deletions still exit 1: for example, removing a visible directory containing
ignored files fails because it is not empty. Default mode exits 1 with guidance
to re-index or use `-I` if its index is missing/incompatible, a start cannot be
resolved. New names discovered during traversal are visible and walked live;
even names that match ignore rules appear until the next index. Re-index after
changing ignore rules. Both find modes exit 0 for no matches and 1 for errors.

Related repositories:

- [intpack](https://github.com/dbalmain/intpack) — integer-sequence codecs for
  the posting lists
- [intpack-bench](https://github.com/dbalmain/intpack-bench) — the codec
  benchmark harness; results summarised in [docs/intpack/](docs/intpack/)

Licence: MIT OR Apache-2.0.
