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
then takes GNU find syntax and answers from the index: ignored paths do not
exist to it, and it sees the tree as of the last `ferret index`.
`ferret find -I` (or `--no-ignore`) walks the disk live with no ignore rules,
and `find_no_ignore = true` in `~/.config/ferret/config` makes that the default.
Both walk in parallel: parents still come before children (after them under
`-depth` and `-delete`), but sibling order need not be GNU's. Exit status is 0
on success, matches or not, and 1 on any error. [docs/FIND.md](docs/FIND.md)
is the full contract.

Related repositories:

- [intpack](https://github.com/dbalmain/intpack) — integer-sequence codecs for
  the posting lists
- [intpack-bench](https://github.com/dbalmain/intpack-bench) — the codec
  benchmark harness; results summarised in [docs/intpack/](docs/intpack/)

Licence: MIT OR Apache-2.0.
