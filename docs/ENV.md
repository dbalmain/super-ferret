# Environment variables and debug seams

Every environment variable the workspace reads, built by grepping the source for
`env::var`, `var_os`, `env!` and `.env(` (2026-10-26, from 93563b6). When you
add one, add its row here in the same commit.

Four kinds, which differ in who may rely on them:

- **Supported overrides** work in release builds and are part of how ferret is
  run: a user, the packaging or a test may set them.
- **Debug-only injections** compile only under `debug_assertions`. Release
  builds have no hook, so nothing outside the tests may depend on them. They
  exist so integration tests can drive a real production path: a barrier on a
  real checkpoint, a real publication failure, a delayed real thread.
- **Unit-test subprocess markers** are read by test code only. A test
  re-executes its own test binary with the marker set, and the child runs the
  branch the marker names; nothing else sets them.
- **Tooling** is read by tests, build scripts, benches and scripts, never by
  ferret itself.

Grep note: while any variable whose name starts with `FERRET_TEST_` is set,
`ferret grep` never routes through the daemon (`test_injections` in
`crates/ferret/src/grep.rs`, in every build), so an injection meant for the
local process cannot be bypassed by a resident one.

## Supported overrides

Durations are milliseconds and sizes are bytes. Unless noted, an unparsable
value is ignored and the default applies.

| Variable                         | Default                                                          | Read in                                                          | What it does                                                                         | Covered by                                         |
| -------------------------------- | ---------------------------------------------------------------- | ---------------------------------------------------------------- | ------------------------------------------------------------------------------------ | -------------------------------------------------- |
| `FERRET_INDEX`                   | `$XDG_DATA_HOME/ferret`                                          | `ferret` `cli.rs`, `find.rs`, `grep.rs`, `batch.rs`, `daemon.rs` | The index directory, after `--index` and before XDG. Empty means unset.              | `tests/cli.rs`, every fixture                      |
| `FERRET_NO_DAEMON`               | unset                                                            | `ferret` `daemon/client.rs`                                      | Set to anything (even empty): never connect to or spawn a daemon; answer in-process. | `tests/cli.rs`, `tests/daemon.rs`, fixtures        |
| `FERRET_DAEMON_BIN`              | `ferretd` beside the running executable                          | `ferret` `daemon/client.rs`                                      | The daemon binary a client spawns.                                                   | `tests/daemon.rs`, `tests/watch.rs`                |
| `FERRET_DAEMON_STARTUP_MS`       | 10000                                                            | `ferret` `daemon.rs`, `daemon/client.rs`                         | How long a spawned daemon and its client wait for startup.                           | `tests/daemon.rs`, `tests/watch.rs`                |
| `FERRET_DAEMON_IDLE_MS`          | 0 (stay up, D116)                                                | `ferret` `daemon.rs`                                             | Idle time after which the daemon exits; 0 never.                                     | `tests/daemon.rs`, fixtures (60000)                |
| `FERRET_STALE_NOTICE`            | on                                                               | `ferret` `daemon/client.rs`                                      | `0` silences the "no daemon answered; index is old" stderr notice.                   | `tests/cli.rs`                                     |
| `FERRET_SEARCH_THREADS`          | the index's `search_threads` setting, else available parallelism | `ferret` `engine.rs`, `grep.rs`                                  | Query worker threads; a positive integer, which beats the settings file.             | `tests/cli.rs`, `tests/grep.rs`, `engine/tests.rs` |
| `FERRET_GREP_FALLBACK`         | unset                                                            | `ferret` `grep.rs`                                               | A real rg that `ferret grep` runs on the same argv instead of refusing an unsupported command (D118 step 2); not after `-f` drained stdin, a pipe or a device. Each argument the parser took as `--rg` before it stopped is dropped from that argv (D128); the rest reach rg unchanged. | `tests/grep.rs`                                    |
| `FERRET_INDEX_NICE`              | 19                                                               | `ferret-crawl` `lib.rs`                                          | Nice of daemon index threads, 0–19; 0 leaves them at the caller's. Fixtures set 0.   | `tests/watch.rs`, fixtures                         |
| `FERRET_BACKSTOP_MS`             | 3600000                                                          | `ferret` `daemon/writer.rs`                                      | Interval of the daemon's full backstop crawl.                                        | `tests/watch.rs`                                   |
| `FERRET_POLL_MS`                 | 300000                                                           | `ferret` `daemon/writer.rs`                                      | Interval of the daemon's poll of roots with missing or unreliable watches.           | `tests/watch.rs`                                   |
| `FERRET_WATCH_CAP`               | 7/8 of `max_user_watches`                                        | `ferret` `daemon/writer.rs`                                      | Most inotify watches the daemon installs.                                            | `tests/watch.rs`                                   |
| `FERRET_INDEX_WORKERS`           | available parallelism, capped (`default_workers`)                | `ferret` `config.rs`                                             | Daemon crawl workers; at least 1.                                                    | `tests/watch.rs`                                   |
| `FERRET_BULK_BYTES_PER_SECOND`   | 33554432                                                         | `ferret` `config.rs`                                             | Rate limit on the daemon's bulk work.                                                | `tests/daemon.rs`                                  |
| `FERRET_FULL_MEMORY_BYTES`       | 3221225472                                                       | `ferret` `config.rs`                                             | Available memory a full rewalk needs to be admitted, scaled by catalog size.         | `config.rs` unit test                              |
| `FERRET_CHECKPOINT_MEMORY_BYTES` | 700000000                                                        | `ferret` `config.rs`                                             | Available memory a checkpoint needs to be admitted, scaled by catalog size.          | `config.rs` unit test                              |
| `FERRET_CHECKPOINT_DISK_BYTES`   | 700000000                                                        | `ferret` `config.rs`                                             | Free disk bulk work needs to be admitted, scaled by catalog size.                    | `config.rs` unit test                              |
| `FERRET_MEMORY_FLOOR_BYTES`      | 67108864                                                         | `ferret` `config.rs`                                             | The least memory any bulk admission requires.                                        | `config.rs` unit test, `tests/watch.rs`            |
| `FERRET_ADDITIONAL_MEMORY_BYTES` | 0                                                                | `ferret` `config.rs`                                             | Added to every bulk memory requirement.                                              | `config.rs` unit test                              |
| `FERRET_BATTERY_PAUSE`           | 1                                                                | `ferret` `config.rs`                                             | `0` keeps the daemon indexing on battery.                                            | `config.rs` unit test                              |
| `FERRET_SIGNAL_PROC`             | `/proc`                                                          | `ferret` `politeness.rs`                                         | Where the daemon reads pressure and load signals.                                    | `tests/daemon.rs`, fixtures                        |
| `FERRET_SIGNAL_POWER`            | `/sys/class/power_supply`                                        | `ferret` `politeness.rs`                                         | Where the daemon reads battery state.                                                | `tests/daemon.rs`, fixtures                        |

Standard variables ferret reads, all in release builds:

| Variable                                                                       | Read in                           | Use                                                                                       |
| ------------------------------------------------------------------------------ | --------------------------------- | ----------------------------------------------------------------------------------------- |
| `HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_CONFIG_HOME` | `ferret` `xdg.rs`, `grep/live.rs` | Index, query log, cache and config locations; git's global excludes for `--live` grep.    |
| `XDG_RUNTIME_DIR`                                                              | `ferret` `daemon/endpoint.rs`     | The daemon socket directory; must be absolute, or there is no daemon.                     |
| `GIT_CONFIG_GLOBAL`                                                            | `ferret` `grep/live.rs`           | First place `--live` grep looks for `core.excludesFile`.                                  |
| `NO_COLOR`, `TERM`                                                             | `ferret` `grep/presentation.rs`   | `--color=auto`: no colour when `NO_COLOR` is set or `TERM=dumb`.                          |
| `RIPGREP_CONFIG_PATH`                                                          | `ferret` `grep.rs`                | Non-empty is refused: ferret grep reads no rg config file, so it asks for explicit flags. |
| `PATH`                                                                         | `ferret-query` `find/parse.rs`    | `-execdir`/`-okdir` refuse a relative `PATH` entry, as GNU find does.                     |

## Debug-only injections

All are `#[cfg(debug_assertions)]` at the read site; release builds compile the
hook out. A gate is a directory or file the test creates and removes; the
production code parks on it at a real checkpoint.

| Variable                                                        | Read in                             | Injection                                                                                              | Covered by                                                               |
| --------------------------------------------------------------- | ----------------------------------- | ------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------ |
| `FERRET_CATALOG_TEST_FAIL`                                      | `ferret-catalog` `publication.rs`   | Fails the publication point it names (`ManifestRename`, say) on every visit, in any process.           | `tests/cli.rs`, `tests/daemon.rs`, `ferret-crawl` `tests/partial.rs`     |
| `FERRET_LOCK_DIAG`                                              | `ferret-catalog` `lock.rs`          | A file to append, on a refused catalog lock, the `/proc/locks` holders and every process with it open. | fixtures set it; `tests/daemon.rs`, `tests/watch.rs` print it on failure |
| `FERRET_CONTENT_TEST_GATE`                                      | `ferret` `daemon/writer/content.rs` | Parks content indexing at a named phase's cancellation checkpoint.                                     | `tests/daemon.rs`, `tests/watch.rs`                                      |
| `FERRET_CRAWL_TEST_GATE`                                        | `ferret-crawl` `index.rs`           | Parks each crawl worker after its priority decision, recording its thread id.                          | `tests/watch.rs`                                                         |
| `FERRET_DAEMON_TEST_LOAD_GATE`                                  | `ferret` `daemon/writer.rs`         | Holds the daemon in `loading` while the gate exists.                                                   | `tests/daemon.rs`                                                        |
| `FERRET_DAEMON_LOAD_DELAY_MS`                                   | `ferret` `daemon/writer.rs`         | Sleeps before the daemon loads its engine.                                                             | `tests/daemon.rs`                                                        |
| `FERRET_DAEMON_TEST_PIN_GATE`                                   | `ferret` `daemon.rs`                | Parks the first pin selection that claims the gate's `hold` file.                                      | `tests/daemon.rs`                                                        |
| `FERRET_DAEMON_TEST_STATS_REACHED`                              | `ferret` `daemon.rs`                | A file written when a stats request reaches its budget read.                                           | `tests/daemon.rs`                                                        |
| `FERRET_DAEMON_TEST_ACCEPT_ERROR`                               | `ferret` `daemon.rs`                | A file holding an errno name; the accept loop injects that error once and removes the file.            | `tests/daemon.rs`                                                        |
| `FERRET_DAEMON_TEST_BUILD`                                      | `ferret` `daemon.rs`                | Replaces the build id the daemon advertises.                                                           | `tests/daemon.rs`                                                        |
| `FERRET_DAEMON_TEST_CONTEXT`                                    | `ferret` `daemon.rs`                | Replaces the security context the daemon advertises.                                                   | `tests/daemon.rs`                                                        |
| `FERRET_DAEMON_TEST_FORMAT`                                     | `ferret` `daemon.rs`                | Replaces the catalog format the daemon advertises.                                                     | `tests/daemon.rs`                                                        |
| `FERRET_DAEMON_TEST_READER_EXIT_DELAY_MS`                       | `ferret` `daemon.rs`                | Widens the window before a connection's reader shuts the socket.                                       | `tests/daemon.rs`                                                        |
| `FERRET_DAEMON_TEST_STOP_DELAY_MS`                              | `ferret` `daemon/writer.rs`         | Holds the writer's stop window open (D110).                                                            | `tests/daemon.rs`                                                        |
| `FERRET_WRITER_TEST_COMMAND_DELAY_MS`                           | `ferret` `daemon/writer.rs`         | Sleeps on the writer before a queued command's crawl.                                                  | `tests/daemon.rs`, `tests/watch.rs`                                      |
| `FERRET_WATCH_TEST_REFRESH_DELAY_MS`                            | `ferret` `daemon/writer.rs`         | Sleeps at the start of a watch refresh.                                                                | `tests/watch.rs`                                                         |
| `FERRET_WATCH_TEST_SCOPES`                                      | `ferret` `daemon/writer.rs`         | Caps the watch scope table (default 100000).                                                           | `tests/watch.rs`                                                         |
| `FERRET_TEST_REFUSE_THREADS`                                    | `ferret-query` `verify.rs`          | `NAME=K`: the first K threads named NAME start, later ones are refused, as under `RLIMIT_NPROC`.       | `tests/daemon.rs`, `tests/grep.rs`                                       |
| `FERRET_TEST_GREP_APPEND_PATH`, `FERRET_TEST_GREP_APPEND_BYTES` | `ferret` `grep.rs`                  | Appends the bytes to the path after grep's first read, so a file grows mid-search.                     | `tests/grep.rs`                                                          |

## Unit-test subprocess markers

Read only by test code, unit or integration, each by the test that sets it in a
child copy of its own binary.

| Variable                                          | Owning test module                        |
| ------------------------------------------------- | ----------------------------------------- |
| `FERRET_TEST_COMPLETION_CHILD`                    | `ferret-query/src/verify.rs`              |
| `FERRET_TEST_CONTROLLER_CASE`                     | `ferret/src/config.rs`                    |
| `FERRET_TEST_LATE_SPILL`                          | `ferret/src/grep/groups.rs`               |
| `FERRET_TEST_STREAM_PANIC`                        | `ferret/src/grep/groups.rs`               |
| `FERRET_TEST_CAPTURE_LOW_DESCRIPTORS`             | `ferret/src/grep/groups/tests/capture.rs` |
| `FERRET_TEST_BIND_NAMESPACE`                      | `ferret/tests/watch.rs`                   |
| `FERRET_CATALOG_SPECIAL_CHILD`                    | `ferret-catalog/tests/special_files.rs`   |
| `FERRET_INDEX_SPECIAL_CHILD`                      | `ferret-index/src/store/tests.rs`         |
| `FERRET_CRAWL_TEST_IN_NAMESPACE`                  | `ferret-crawl/src/tests/lifecycle.rs`     |
| `FERRET_FD_BOUND_ROOT`                            | `ferret-crawl/src/tests/parallel.rs`      |
| `FERRET_FD_PRESSURE`, `FERRET_FD_PRESSURE_BASE`   | `ferret-crawl/src/tests/content.rs`       |
| `FERRET_FIFO_CHILD`, `FERRET_FIFO_ROOT`           | `ferret-crawl/src/tests/mod.rs`           |
| `FERRET_UNDURABLE_CHILD`, `FERRET_UNDURABLE_BASE` | `ferret-crawl/src/tests/partial.rs`       |

## Tooling

| Variable                                                                               | Read by                                                     | Use                                                                                                                                                         |
| -------------------------------------------------------------------------------------- | ----------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `FERRET_GNU_FIND`                                                                      | `tests/support/gnu_find.rs`                                 | The pinned GNU findutils oracle; the dev shell sets it, and tests fail without it.                                                                          |
| `FERRET_RG`                                                                            | `ferret/tests/grep.rs`, `daemon.rs`, `shadow.rs`            | The pinned rg for grep differential tests; set by the dev shell.                                                                                            |
| `FERRET_PYTHON`                                                                        | `ferret/tests/grep.rs`, `daemon.rs`, `grep/groups.rs` tests | Drives real-pty tests with `os.openpty`; set by the dev shell.                                                                                              |
| `FERRET_PROTOCOL_FUZZ_ITERS`                                                           | `ferret/src/protocol.rs` (ignored test)                     | Iterations of the long protocol mutation fuzz.                                                                                                              |
| `FERRET_SEGMENT_FUZZ_ITERS`                                                            | `ferret-index/src/segment/tests.rs`                         | Iterations of the long segment fuzz.                                                                                                                        |
| `FERRET_WATCH_BURST_ROUNDS`                                                            | `ferret/tests/watch.rs` (ignored test)                      | Rounds of the long generated watch-burst test; 1000 by default.                                                                                             |
| `FERRET_FAULT_BENCH`                                                                   | `ferret-crawl/src/tests/index.rs` (ignored)                 | A catalog directory to time the fault pass over.                                                                                                            |
| `SCAN_FERRET`, `SCAN_FERRET_INDEX`, `SCAN_XDG`, `SCAN_PER_CLASS`, `SCAN_E2E_PER_CLASS` | `ferret-bench` `sparse/screens/scan.rs`                     | The scan screen's binary, index, XDG base and sample sizes.                                                                                                 |
| `FERRET_BUILD`                                                                         | `ferret/build.rs` → `env!` in `daemon.rs`                   | Compile time: the build id a daemon advertises (debug builds can replace it above).                                                                         |
| `FERRET_VERIFY_RUSTC`, `RUSTC`                                                         | `ferret-verify/build.rs` → `toolchain.rs`                   | Compile time: the rustc the toolchain ledger (D11) was measured with.                                                                                       |
| `FERRET_REAL_RG`, `FERRET_BIN`, `FERRET_SHADOW_DIR`, `FERRET_SHADOW_STDIN`             | `contrib/agents/rg`, `contrib/agents/ferret-shadow`, `contrib/agents/ferret-shadow-report` | The shadow-comparison wrapper: the real rg, the ferret binary, the log directory, and whether stdin was readable. The flake's packaging sets the first two. |
| `FERRET_ANSWER`, `FERRET_SHADOW`, `FERRET_SHADOW_EVERY`                                | `contrib/agents/rg`                                         | Who answers an agent's rg call (`rg`, the default, or `ferret`, D118 step 2); `0` compares nothing; compare one call in N (1 when rg answers, 10 when ferret does). |
