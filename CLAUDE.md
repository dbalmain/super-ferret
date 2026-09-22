# Super Ferret — working guide

Linux desktop search in Rust. Read [docs/DESIGN.md](docs/DESIGN.md) for the
shape and [docs/ROADMAP.md](docs/ROADMAP.md) for where work stands; decisions,
with their reasons, are in [docs/DECISIONS.md](docs/DECISIONS.md).

## Gates

All green with zero warnings before a change is done:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Style: `~/style-guide/rust.md` and `~/style-guide/common.md`
(github.com/dbalmain/style-guide).

## Where things go

| Change                                         | Crate            |
| ---------------------------------------------- | ---------------- |
| ignore rules, size cap, binary check           | `ferret-policy`  |
| walking roots, change detection, hashing       | `ferret-crawl`   |
| names, inodes, documents, name search, storage | `ferret-catalog` |
| what a token is                                | `ferret-text`    |
| a new index structure                          | `ferret-index`   |
| matching a candidate's bytes                   | `ferret-verify`  |
| query syntax, planning, result rows            | `ferret-query`   |
| CLI flags, output, config, query log           | `ferret`         |

The crate graph is enforced: `crates/ferret/tests/layering.rs` fails if any
crate's `[dependencies]` differ from the graph in DESIGN.md § Crates. To add a
dependency, change the design line in the same commit — and if the new edge
points somewhere the design says a crate "knows nothing about", stop and raise
it instead.

## Rules

- `unsafe` is denied workspace-wide. Where a measurement justifies it (D11),
  allow it on the item with a comment saying why.
- Compiler-steering code — an `#[inline(always)]`/`#[inline(never)]` chosen for
  speed, an `asm!` hint, a `target_feature`-gated SIMD arm — goes in the crate's
  toolchain ledger, with the bench row and number that justified it. The first
  such item creates the ledger, copying intpack's pattern (`build.rs` records
  rustc; `src/toolchain.rs` fails a test when it moves). SIMD arms compile for
  tests on any x86_64 and run under runtime detection.
- Open questions for Dave go into DECISIONS.md as briefs: question, named
  options, tradeoff per option, recommendation, and the fact that would change
  it. He answers inline with `> Dave:` comments.
- Offloaded agents: never `git push`; never edit `Cargo.toml` or `Cargo.lock`
  unless the brief says to; one benchmark run at a time on the machine.
