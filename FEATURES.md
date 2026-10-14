# cargo val features

Status: **built** (in tree, uncommitted) · **designed** · **queued** · **later** · **declined**. Detail lives in `specs/cargo-val/discipline.md`.

## Running
- **built** — stream tests as cargo reports each test binary; first result before discovery ends
- **built** — one process per test; child gets cargo's env, cwd, dylib path (cargo test / nextest parity)
- **built** — shard mode: tests run in shared processes per binary
- **built** — settle-or-solo: a shard settles a test only from a clean same-line libtest result; every other name is re-run alone and that solo run decides
- **built** — `--process auto|shard|test`: auto = shard on Windows, per-test elsewhere
- **built** — `--output auto|rich|plain` / `CARGO_VAL_OUTPUT`: auto is rich only on an interactive terminal outside CI (`CI` non-empty or `TERM=dumb` give plain); `NO_COLOR` suppresses ANSI in every mode
- **built** — serial / group / process records honored in both modes
- **built** — interrupt (Ctrl-C / SIGTERM) kills children in both modes
- **queued** — shard binaries in parallel
- **queued** — zero-allocation test for a shard launch
- **built** — `-- --test-threads N` / `RUST_TEST_THREADS`; `-j` stays cargo build jobs
- **built** — fail fast by default; `--no-fail-fast`, `--max-fail N`
- **built** — `--no-tests=fail|warn|pass`, zero tests run is never green (default fail, exit 4)
- **built** — exit status: 0 passed, 1 a test failed, 2 invalid arguments or settings or an invocation cargo refused before building (cargo-val prints its refusal line after cargo's own error), 3 the cargo build failed, 4 no tests ran under the fail policy, 128+signal interrupted
- **built** — a failed build prints each compiler-message `rendered` text (errors, plus the warnings of that build) verbatim to stderr before the build-failed line; a successful build prints none; `--message-format=json-diagnostic-rendered-ansi` only when colour is on, so colour off carries no ESC bytes
- **built** — `#[ignore]`d tests are their own outcome: listed per binary with `--list --ignored`, never launched, reported IGNORED; `-- --ignored` / `-- --include-ignored` (libtest meaning) or `CARGO_VAL_IGNORED=skip|only|include`; a child is classified by its libtest `test result:` line, so 0 passed is never PASS and an `--exact` launch that ran nothing is an error naming the test
- **built** — summary lines: `<binary>: discovered N, passed P, failed F, ignored I, filtered X` per test binary plus a `total:` line with wall time; counts that do not balance are an error naming the binary; zero executed tests follows `--no-tests`
- **built** — a test or bench executable is not listed or launched until every `bin` target of its package has emitted its compiler-artifact (so `env!("CARGO_BIN_EXE_<bin>")` exists); bin set from one `cargo metadata --no-deps`, else wait for `build-finished`; a build that fails (`build-finished` with `success:false`, or cargo EOF before `build-finished`) launches none of the waiting executables; `CARGO_VAL_BIN_GATE=off` disables it; `CARGO_VAL_LOG=debug` prints one stderr line per gate decision
- **built** — `-E` / `--filterset` / `CARGO_VAL_FILTERSET`: nextest filterset subset (`test() binary() package() all() none()`, `and & or | + not ! -`, parentheses; bare contains for `test()`, glob for `binary()` / `package()`, `~contains`, `=exact`, `#glob`) applied at discovery; `/regex/` is a parse-time error (no regex engine fits the no_std, no-heap binary)
- **built** — doc tests via one concurrent `cargo test --doc` child, counted in the same report
- **built** — build directories: in isolated mode the `--no-run` build goes to `<target dir>/cargo-val/<workspace key>/test` and the `--doc` build to `.../doc` (target dir from `cargo metadata`, key = FNV-1a of the workspace root), so no other cargo process, rust-analyzer, or cargo val shares their build lock; `--target-dir` (isolated, shared, or a path) / `CARGO_VAL_TARGET_DIR`, default shared (cargo's own dir, the one `cargo test` builds in; isolated as above, a path holds `test/` and `doc/`); a child blocked on a lock prints one `cargo-val: waiting for <lock> held by another cargo process (<dir>)` line, the package-cache lock named as global; cost of isolated: dependencies compile once more per workspace than plain `cargo test`, and the doc build compiles the library a second time
- **built** — rustc libdir query off the path to the first result (−19 ms)
- **built** — macOS spawn closes inherited fds (`POSIX_SPAWN_CLOEXEC_DEFAULT`)
- **built** — Ctrl-C / SIGTERM exit 130 / 143 via a self-pipe signal edge (`src/edge_signal_unix.rs`)
- **built** — signals forwarded to every child (cargo, listings, tests, doc group); no busy wait (macOS + Linux)
- **built** — duplicate-launch guard keyed on (test binary, test name); same-name two-binary regression test 10/10 macOS + Linux, fails every run with the old guard
- **built** — launch errors carry their cause (variant, errno); test name + binary printed at the failure site
- **queued** — signal target table owned by the driver, not a process-global (folds into the Prime move)
- **designed** — runs on Prime (executor + reactor); no hand-written event loop (today: hand loop calls `kevent` directly)
- **designed** — core (run FSM + stages) no_std and alloc-free; edges are opt-in backends: `edge-os` (Prime reactor: kqueue / epoll+pidfd / IOCP) and `edge-vm` (proxima-vm micro-VM per test)
- **designed** — platform differences only in edge pipes; one platform-neutral FSM orchestrates
- **built** — Windows: completion-port reads, one job object per run, below-normal priority, no thread pool; the native `windows-latest` run is the measurement
- **built** — Windows child creation: job via `JOB_LIST`, 3-handle list, no console window, absolute exe path

## Scheduling
- **built** — `#[proxima::test(serial)]` — nothing else in flight
- **built** — `#[proxima::test(group = "x")]` — at most N of x at once (nextest test-group meaning)
- **built** — `#[proxima::test(process)]` — share one process (with a group, or per binary)
- **built** — scheduling records v2 (flags + group) in a linker section of each test binary (v2 verified macOS + Linux debug/release; Windows unverified)
- **built** — cargo val reads the records and schedules by them, in both process modes
- **queued** — read `.config/nextest.toml` test-groups, threads-required, overrides
- **designed** — tests below normal OS priority while cargo builds; rustc keeps its cores
- **later** — slowest-first order from stored history

## Selection
- **later** — `--lf` / `--ff` / `--sw` from stored history
- **later** — tags + `-m "not gpu"`
- **later** — `-k` boolean name expression
- **queued** — libtest flags after `--`: `--skip`, `--exact`, `--list`, multiple filters (`--include-ignored` and `--ignored` are built)
- **later** — `--partition i/n`

## Reporting
- **designed** — SLOW line once per test at a threshold (default 60 s); never kills
- **later** — `--durations=N`
- **queued** — failure output names the test, no silent 4 KiB truncation; `--nocapture` / `--show-output`
- **later** — retries reported as flaky, never a quiet pass
- **declined** — JUnit until a CI needs it
- **declined** — debugger rerun of a failing test

## Matrix
- **designed** — matrix = Proxima pipe fanout over cells (`FanOut`), results merged with `FanIn`; cells from `cargo metadata` + config (no cargo-hack, never edits manifests)
- **designed** — compile check matrix: feature sets × target triples; first manual run found 13 defect classes in Proxima, fixes running
- **designed** — test matrix on declared cells, Linux cells on versailles
- **designed** — runtime matrix over env / conflaguration settings, no rebuild
- **designed** — per-test env (`env(...)`, `env_matrix(...)`), isolated by process-per-test
- **later** — matrix cells hosted in proxima-vm guests; needs proxima-vm full hardware emulation (owner goal: QEMU-class on Prime + pipes + FSM)
- **later** — board cells from config: e.g. a Raspberry Pi machine section composes device pipes; aarch64 runs on HVF here, devices emulated, no translator
- **later** — cross-arch cells (x86_64 etc.) need the proxima-vm translator

## Cassettes (Proxima)
- **built** — invalidation FSM core (S1–S3): 66 transition tests, collision refusal, 0 allocs over 100k lookups, no locks; S4–S8 (stamps, loader, harness rewire, deletions) queued
- **built** — content key: method, path, query, header allowlist, body (0 allocs)
- **designed** — missing cassette records by default; `strict` is a conflaguration toggle
- **designed** — store: target cache (default), next to test, or custom folder (hierarchy or flat)
- **designed** — precedence next-to-test > custom > target; warn only when copies differ


## Distribution
- **later** — build-once archive with path remap
- **later** — live distribution to ssh workers over proxima transport

## Config
- **designed** — one `RunConfig` builder, one method per axis; TOML sections mirror it; CLI sets builder values
- **designed** — every option is one conflaguration setting (builder ↔ TOML ↔ env parity)
