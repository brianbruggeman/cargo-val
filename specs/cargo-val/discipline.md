# cargo-val discipline log

The 16-point gate (see `/disciplined-component`): build+tiers / tests+N / lint /
micro-bench / compare-bench / E2E / opt-sweep / SIMD-SM-nodyn / O(1) / Cfg-API /
home-turf / internal-audit / no-magic-numbers / re-provable / Δ.

Harness: `tools/val-bench` (Rust; arms interleaved per round; wall, user/sys CPU,
max RSS from `wait4`; per-arm test-count cross-check; `quiet_box` from the GPU
lock). Raw records: `~/.cache/cargo-val/bench/<run>/*.jsonl`.

## E1 — cargo val end to end vs cargo test and cargo nextest

| Build+Tiers | Tests (N) | Lint | Micro | Compare | E2E | Opt | SIMD/SM | O(1) | Cfg/API | Home-turf | Audit | Consts | Re-prove | Δ | Notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| host build exit 0; lib `x86_64-unknown-none` exit 0 | 101 (`cargo test --tests --no-fail-fast`, 2026-10-11) | pedantic: not clean crate-wide | none | PENDING run c62 | this row | not done | not done | not audited | partial (runtime settings via conflaguration) | PENDING run c62 | FAILED: hand-written event loop instead of Prime (owner 2026-10-10) | not audited | see below | – | runtime move to Prime in progress |

**Incumbent design points:**
- `cargo test` (libtest): one process per test binary, threads inside it, binaries
  run one after another. Built for many small tests in few binaries.
- `cargo nextest` 0.9.146: one process per test, scheduled across binaries, tokio
  1.53.1 multi-thread runtime (`nextest-runner` 0.124.0 `src/runner/imp.rs:364`).
  Built for large suites, isolation, and cross-binary parallelism.

**Bill-mover filter (owner, 2026-10-11):** wall time only matters when
`cargo test` takes 10+ minutes; suites that finish in a minute or two do not
move anyone's day. The 80% case is therefore a LONG suite: a multi-crate
workspace, many test binaries, thousands of tests, heavy-tailed durations.
That is nextest's design point (cross-binary scheduling), so nextest is the
incumbent to beat there and `cargo test` is the floor. The small-fixture arms
below are overhead probes, not verdict arms. Long-suite arms: PENDING (real
workspace + synthetic heavy-tail generator).

**Overhead-probe bands (superseded as verdict arms by the filter above):**
- 80% case: a light suite run on every edit — many fast tests, wall time
  dominated by startup, discovery and process launch. Arm: `full` (17 light
  tests, 5 binaries). `design-favors: incumbent` for both.
- Warm: filtered runs during development. Arm: `filter4` (`order_seed`, 4
  tests). `design-favors: incumbent` (cargo test runs one binary in-process).
- Cold to warm: CPU-heavy suites. Arms: `scale2`, `scale10` (loop bounds ×2,
  ×10). `design-favors: ours` and nextest (cross-binary parallelism).

**Re-prove command (c62):**
`cd tools/val-bench && cargo run --release -- --fixture ../../tests/fixtures/workspace [--filter order_seed | --loop-scale N] --jobs 10 --warmup 1 --iterations 20 --timeout-secs 180 --output <jsonl>`
after `cargo install --path . --locked --force` at the repo root.

### Bench results

| run | binary | arm | cargo test median | nextest median | cargo val median | val/test | val/nextest | CoV range | quiet |
|---|---|---|---|---|---|---|---|---|---|
| c54b lookahead_pair20 | a04ddb04 (old) | full, 20 rounds | 70.28 ms | not run | 69.86 ms | 0.994 | – | 1.5–2.6% | not recorded |
| c54b slot_pair17_isolated | a04ddb04 (old) | full, 20 rounds | 69.94 ms | not run | 69.66 ms (17 slots), 70.91 ms (10) | 0.996 / 1.014 | – | 1.0–2.1% | not recorded |
| c54b workload_10m_pair20 | a04ddb04 (old) | scale10, 20 rounds | 365.81 ms | 328.68 ms | 240.44 ms | 0.657 | 0.732 | 0.3–3.3% | not recorded |
| smoke (1 round) | a701d828 | filter4 | 46.76 ms | 155.18 ms | 163.04 ms | 3.49 | 1.05 | n=1, no CoV | false (load 4.9, lock held) |
| c62 | 46c5243b | full | 364.76 ms | 335.62 ms | 259.08 ms | 0.710 | 0.772 | 0.4–6.0% | no (load 7.4–10.2) — UNEXPLAINED: all arms ~5x the c54b full row and ~3.5x scale2; source fixture bound is 1_000_000_u64 (branch_fixture.rs:2). Excluded until explained |
| c62 | 46c5243b | filter4 | 115.59 ms | 204.45 ms | 128.24 ms | 1.109 | 0.627 | 1.1–30.0% (cargo val 30%) | no (load 10.2–10.6) |
| c62 | 46c5243b | scale2 | 104.69 ms | 189.18 ms | 102.76 ms | 0.982 | 0.543 | 2.7–4.4% | no (load 11.9–12.1) |
| c62 | 46c5243b | scale10 | 380.25 ms | 374.17 ms | 290.11 ms | 0.763 | 0.775 | 3.7–9.3% | no (load 13.9–15.1) |

**c62 `full` and `filter4` are RETRACTED.** Cause proven by toggle
(c62-debug): every fixture shared one `CARGO_TARGET_DIR`; cargo's fingerprint
for the fixture's test binaries is path-relative and mtime-checked, so the
unscaled fixture (old mtimes) reused the 10M binaries the scaled-10 copy had
left on disk. Unscaled timing moved 370–450 ms -> 80–85 ms when the stale
binary was replaced, and back when it was restored. `scale2` and `scale10`
rebuilt their own binaries (fresh mtimes) and stand. Harness fixed: per-fixture
target dir slug (`tools/val-bench/src/fixture.rs` `target_slug`), and
`quiet_box` now also requires load_1m <= `--quiet-load` (default 3.0).

| run | binary | arm | cargo test median | nextest median | cargo val median | val/test | val/nextest | CoV range | quiet |
|---|---|---|---|---|---|---|---|---|---|
| c62-debug rerun (10 rounds) | 46c5243b | full | 140.29 ms | 319.61 ms | 169.92 ms | 1.211 | 0.532 | 14.6–32.3% | no (load 11.5–14.2) |

**Real long suite — Proxima, 35 of 47 crates (12 model/GPU crates excluded),
4327 tests, 66 binaries** (`proxima-prime-windows-io` @ 8417f133, target
`~/.cache/cargo-val/bench/longsuite-target`, logs
`~/.cache/cargo-val/bench/proxima-race2/`). cargo val binary 0e4c8d5a (after the
child-environment parity fix). Parity gate first: cargo val 4327 PASS / 0 FAIL;
nextest 4327 passed; sorted name diff vs `cargo nextest list` = 0 lines.
3 rounds, order rotated TNV/NVT/VTN, jobs 10, `timeout 300`, load_1m 3.8–8.3
(not quiet).

| arm | walls (s) | median | CPU user+sys (s) | max RSS | tests | design-favors |
|---|---|---|---|---|---|---|
| cargo test | 300.00 (timeout, hung in fork_server), 300.01 (timeout, hung), 50.77 | n/a | 30.7, 30.5, 49.5 | 145–714 MB | incomplete, incomplete, 4386 incl. doctests | incumbent floor |
| nextest | 19.21, 18.92, 19.67 | 19.21 | 70.0, 69.9, 70.4 | 251 MB | 4327, 4327, 4327 (1 failed in r3) | **incumbent** |
| cargo val | 18.31, 18.39, 18.54 | 18.39 | 51.1, 51.9, 52.0 | 251 MB | 4327 x3, 0 failed | ours |

**SUSPECT — unmatched concurrency.** This race passed only `--jobs 10` to cargo
val; `--jobs` is forwarded to cargo as BUILD jobs (`src/cli.rs:119`), while
cargo val's test concurrency is `CARGO_VAL_JOBS` (`src/host/unix_driver.rs:242-264`),
defaulting to the build machine's `available_parallelism` (`build.rs:28-30`).
nextest ran `-j 10`. Re-run with `CARGO_VAL_JOBS=10` before using these rows.
(val-bench runs — c62, c64, c66 — set CARGO_VAL_JOBS and are not affected.)

Status `plausible`: n=3, host not quiet. cargo val's slowest run (18.54 s) is
below nextest's fastest (18.92 s); median 4.3% lower wall, 26% lower CPU. The
cargo test arm is not comparable: it hung twice in fork_server tests (in-process
threads + fork) and its completed run includes doctests that neither other arm
runs. Unexplained: `prime::windows_port windows_port_udp_loopback_payload`
failed in cargo test r1/r2 and nextest r3, never under cargo val (n=3; flaky
test or isolation difference, not traced).

**Time to first result, warm build (c64, cargo val 0e4c8d5a, logs
`~/.cache/cargo-val/bench/c64/`).** Metric: spawn -> first per-test result line
(`first_result_ns`), test binaries already built. Host NOT quiet: Proxima run
load_1m 7–44 (an unrelated `cargo check --workspace` ran concurrently), small
fixture 8–15. Wall times from the Proxima run are therefore not used.

| suite | arm | first result median (min–max) | first output median | tests | note |
|---|---|---|---|---|---|
| Proxima (5 rounds) | cargo val | 492 ms (429–588) | 440 ms | 4327 | |
| Proxima | nextest | 1561 ms (1471–1819) | 768 ms | 4327 | |
| Proxima | cargo test | 881 ms (776–1788) | 387 ms | 1954 | fail-fast stop (see below) |
| small, 17 tests (10 rounds) | cargo test | 61.9 ms (48–149) | – | 17 | |
| small | cargo val | 151.3 ms (123–203) | = first result | 17 | |
| small | nextest | 224.1 ms (160–306) | – | 17 | |

Read: on the real suite cargo val's first result arrives ~3.2x sooner than
nextest's (nextest lists all 66 binaries first) and ~1.8x sooner than cargo
test's. On the 17-test fixture cargo val is ~89 ms behind cargo test.
Unexplained, unmeasured candidates for that fixed cost: the per-run
`rustc --print target-libdir` spawn added by the child-environment fix, and
cargo val's own `cargo test --no-run` invocation. Needs a profile, not a guess.

Startup components (c65, small fixture, 15 interleaved rounds, `/usr/bin/time
-p` at 10 ms resolution, load 7–9; logs `~/.cache/cargo-val/bench/c65/`):
`rustc --print target-libdir` 20 ms; `cargo test --no-run --message-format=json`
(fresh) 20 ms; one `--list` 0–10 ms; one single-test run 10 ms; `cargo val`
total 100 ms (90–100 after the first run; first run 870 ms); `cargo test` total
70 ms. Spawn sites read from source (not observed at runtime): 1 cargo
(`unix_driver.rs:855`), 1 rustc (`unix_rustc.rs:58`, called at
`unix_driver.rs:1020` before cargo is spawned at `:1050`, so serial on the
path to the first test), 6 listings (`:650`, one per artifact incl. a 0-test lib
binary), 17 per-test processes (`:747`) = 25 children vs cargo test's 1 cargo + 6
binaries. Candidates for the gap, in measured size: the serial rustc query
(~20 ms, removable by running it concurrently with cargo or caching it per
rustc binary), then process-per-test spawns (inherent to isolation; cost per
spawn not yet isolated at this timer resolution).

Unexplained: under cargo test (fail-fast, 6/6 rounds) `proxima-primitives`
`pipe::isolate::tests::timeout_returns_503_and_increments_metric` panics
("intentional test panic", `proxima-test/src/harness.rs:173`); under nextest and
cargo val (process per test) it passes. Same test failed in the c63 race's cargo
test rounds.

Separate hazard observed (not investigated): the first execution of a freshly
linked test binary stalled 316 s in `_dyld_start` while `syspolicyd` ran at
~44% CPU. Warmup absorbs it; the 180 s run timeout would kill such a run.

c62 CPU and memory (means of 20 measured records, ms / MiB):

| run | cargo test user/sys/RSS | nextest user/sys/RSS | cargo val user/sys/RSS |
|---|---|---|---|
| filter4 | 333.1 / 12.8 / 17.9 | 438.8 / 79.8 / 34.9 | 350.4 / 22.6 / 17.7 |
| scale2 | 278.0 / 14.0 / 17.7 | 465.4 / 114.3 / 35.0 | 333.0 / 45.9 / 17.5 |
| scale10 | 1323.0 / 20.8 / 17.9 | 1572.3 / 134.4 / 35.1 | 1463.3 / 110.9 / 17.6 |

c62 test counts: every record agreed across arms (0 mismatch, 0 failed, 0
timed out, 240 measured records). Raw: `~/.cache/cargo-val/bench/c62/`.

The smoke row is one sample on a loaded host; it is a measurement of nothing
beyond "the harness runs". The c54b rows are a binary that no longer matches the
source.

### Frequency-weighted scorecard

PENDING c62. Old-binary reading, status `plausible` at best (stale binary, no
nextest arm on the 80% case, quiet-box state unrecorded): 80% case tied within
noise (target is >3% faster than cargo test: not met); filtered case slower in
the old evidence; CPU-heavy case faster than both.

### Ideas queued (owner discussion 2026-10-11; each needs its own card and measurement)

| idea | expected lever | measurement that decides it |
|---|---|---|
| stored per-test duration history (rolling median/p90 in the target dir) | feeds launch order, build order, per-test slow threshold | makespan with vs without history on proxima (n>=5 rounds) |
| longest-first launch order from history | finish time set by the slow tail starts earlier | proxima wall, same history, ordered vs unordered |
| build order from history (slow-test packages first) | slow tests start earlier; costs build parallelism (target-dir lock serialises two cargo invocations) | first-result and wall, incremental touch of proxima-core |
| tests at lower OS priority while cargo builds (macOS background QoS, Linux nice, Windows BELOW_NORMAL via job object) | rustc keeps every core it wants; tests soak idle cores in cargo's serial tail | build finish time unchanged within noise AND wall reduced |
| read `.config/nextest.toml` (proxima already declares `real-checkpoint`/`heavy-runtime` groups and `threads-required = "num-test-threads"` at `.config/nextest.toml:7-60`): honor test-groups, threads-required, overrides+filters; decide terminate-after (honor when the project sets it), retries, profiles | model/GPU tests run serialized inside the normal run instead of being excluded | max-live per group observed = its max-threads; Proxima run with model crates included |
| `serial = true` (runs alone == threads-required num-test-threads) and `serial = "<name>"` (mutually serial set == group max-threads 1), in `.config/cargo-val.toml` with nextest's override/filter syntax, so nextest's own config stays valid | one-line serialization without declaring groups | same max-live observation; nextest still parses its config unchanged |
| distributed testing, staged: (1) `--partition count:i/n` static sharding like nextest; (2) build-once archive of test binaries + dylib paths + package data with path remap on the worker; (3) live distribution over proxima transport to ssh workers, history-driven placement, results streamed back | multi-minute suites; concurrent macOS/Linux/Windows coverage from one command | (1) union of n partitions == full run, no overlap; (2) archive run on versailles == local results; (3) wall vs single host on a multi-minute suite |
| proxima cassettes as a cache (gaps from source read 2026-10-11): key ignores headers and (by default) body (`proxima-recording/src/replay/keying.rs:74-131`); missing file with no mode set records live (`proxima-test/src/harness.rs:35-42`) — no strict CI default; load-side duplicates last-wins silently (`replay/mod.rs:239`); harness reads only PROXIMA_CASSETTE while policy reads both (`harness.rs:36` vs `src/cassette_config.rs:220-228`); no format/key-schema version or upstream identity in `CassetteMeta` (`proxima-recording/src/replay/meta.rs:12-24`); staleness `max_age` off by default; no verify mode | cassettes become a trustworthy cache: content-keyed, versioned, strict in CI, verifiable against live | a changed header/body produces a miss; CI with a missing cassette fails; verify run reports stale count |
| owner rule 2026-10-11: cassette cache invalidation is a sans-IO FSM driven through pipes, like everything else — no env reads, file-existence checks, or clocks inside the decision; the edges feed events (loaded meta, request, now, policy) and the FSM returns decisions (replay / miss-fail / record / verify-diff / stale) | one auditable place for every invalidation rule; deterministic, testable from recorded events alone | worked-example table of every transition; zero-IO unit tests over recorded cassette events |
| fail-fast vs run-to-completion (owner 2026-10-11: "two settings, run to completion and a fast fail"). Today neither exists: cargo val always runs to completion; `--no-fail-fast` is only forwarded to cargo's build (grep of src/cli.rs, src/host/unix_driver.rs, src/outcomes.rs, src/runner.rs for fail_fast/max_fail = 0 hits). Native form: default fail-fast (cargo test and nextest both default to it); `--no-fail-fast` = run to completion; `--max-fail N` (nextest) with fail-fast = 1; conflaguration setting + builder parity, CLI wins. On trip: stop launching, stop reading listings, let in-flight tests finish (no kill), summary reports not-run count | a red run ends sooner; a full report when asked | fixture with 1 failing + 16 passing tests: fail-fast launches no test after the failure is observed (launch-after-failure count = 0, in-flight finish count reported); --no-fail-fast runs 17/17; --max-fail 2 stops after the 2nd failure |
| doc tests, step 1 (queued behind fail-fast in the cargo-val driver agent): one concurrent `cargo test --doc` child per run at lower priority, its libtest lines parsed into the same report/summary, obeying fail-fast and counting toward totals; zero doc tests where requested is reported, never a silent pass | parity with `cargo test` + nextest users' manual second step; doctests overlap the unit run | fixture with 1 passing + 1 failing doctest: cargo val doc PASS/FAIL == `cargo test --doc` counts; zero-doctest case surfaced |
| doc tests, step 2 (spike first): edition-2024 merged doctests compile one binary per crate; if rustdoc can persist it (flag believed unstable — verify), list and schedule each doctest as its own process with groups/history/fail-fast | doctests scheduled like every other test | spike: does a persisted merged-doctest binary answer `--list --format terse` and `--exact`? count listed == `cargo test --doc` count |
| doc tests, step 3 (depends on step 2): content-keyed doctest cache — key = doctest source text + digest of the linked crate rlib; unchanged doctest against unchanged crate skips compile and reuses the recorded result | removes the per-run doctest recompile cost (usually the dominant doctest cost; unmeasured here) | second run with no source change: doctests compiled = 0; changing one doctest recompiles exactly 1 |
| FINAL (owner 2026-10-11: "okay three distinct flags"): `serial`, `group = "x"`, `process` as below; building in proxima-prime-windows-io. Membership (owner Q: "how do we know x and y share a process?"): explicit only — same `group` name + `process`, or no group + `process` = implicit `@binary:<name>` group. A process is one test binary, so tests in different binaries NEVER share: a `group+process` spanning K binaries runs as K shared processes (one per binary), the group's N limit applied across all K (N=1 -> one after another). Runner may later split a large implicit group into several processes for parallelism (history-driven); results stay per test. | | |
| owner refinement #2 2026-10-11 ("maybe process is a flag that modifies group"), SUPERSEDES the next two entries: `group = "x"` keeps nextest meaning (at most N of x at once, separate processes, N from `[test-groups.x] max-threads`, default 1); `process` is a FLAG on the group: `group = "x", process` = every test of x runs in ONE shared process with libtest threads capped at N (N=1: same process, one by one). `serial` = nothing else in flight. Errors: `serial`+`group`. `process` WITHOUT `group` (owner Q "when would we want process without group?") is allowed: implicit group per test binary, named `@binary:<binary-name>` (the `@` prefix nextest reserves for tool groups, so it cannot collide with user names) — all of that binary's `process` tests share one process, concurrency up to the run's test-threads. Use: tiny tests where spawn cost dominates (Windows), tests sharing in-process OnceLock fixtures; same isolation trade as a process group. Process sharing is a property of the GROUP: members disagreeing on `process` is a cargo val error naming them; also settable as `[test-groups.x] process = true` (config wins). Record (proxima-test/src/sched.rs:55-70): mode byte -> flags {serial, process} + the single group name; stays 328 bytes; version 2 | fewer names than a separate `process = "x"`; nextest-compatible group meaning preserved | mixed-`process` group -> error listing members; 4-test `group+process` -> 1 spawn, 4 result lines; N=1 observed max-live-in-process = 1 |
| owner refinement 2026-10-11 ("one more so not to conflate"), NOT built — THREE orthogonal settings: `serial` (no other test in flight == nextest threads-required num-test-threads); `group = "x"` (at most N of group x concurrently in separate processes, N from `[test-groups.x] max-threads`, default 1 — identical to nextest test-group); `process = "x"` (tests of x share one process: `binary t1 t2 ... --exact`). Combos: process+group = one process, libtest threads capped at the group's max-threads; process+serial = one process that runs alone, sequential inside; group+serial = compile error (serial makes the group limit moot). Record: mode byte -> serial flag + optional group name + optional process name (second 64-byte field, 328 -> 392 bytes, version 2). Supersedes the two-axis entry below. |  |  |
| owner idea 2026-10-11, NOT built: process-group and serial are orthogonal. Process axis: isolated (default, one test per process) vs shared `process = "<name>"` — all tests of the group run in ONE process (`binary t1 t2 ... --exact`). Concurrency axis: parallel vs `serial` (one at a time; inside a shared process that is `--test-threads=1`). Combined = one process, tests one by one. Today's built `#[proxima::test]` rejects `serial`+`group` and `group` means nextest-style mutual serialization in separate processes (proxima-test/src/sched.rs:45-52 modes 0/1/2) — record becomes two flags + name (uncommitted, change in place). Name the process axis `process`, not `group`, so it does not collide with nextest's `test-group` (a concurrency limit) read from .config/nextest.toml | expensive per-process setup (proxima harness keeps a per-process runtime, proxima-test/src/harness.rs:357-365; model loads) paid once per group; one model copy in memory instead of N (the Mac crashes: 6-10 processes x 4.7-12 GB) | a 4-test process group runs in 1 process (spawn count 1, 4 result lines parsed); serial process group observed max-live-tests-in-process = 1; a test aborting the process marks the remaining group tests not-run with the crash output attached |
| RUNTIME MATRIX + env isolation (owner 2026-10-11: "not just compile time feature, but runtime feature and even environment variable isolation per process or possibly per test"), NOT built: cells also vary runtime settings/env without rebuilding — per run cell (`[[matrix.run]] env = {..}`), per test (`#[proxima::test(env("K"="v"))]`), per-test variants (`env_matrix(K = ["a","b"])` -> reported `name[K=a]`). Runtime features = conflaguration settings, each already an env var, so a runtime matrix is a matrix over setting values. Process-per-test gives per-test env isolation by construction (cargo test cannot: one process; why edition 2024 made set_var unsafe and why the rules require temp_env::with_vars). `process` groups share one env: members with differing env = error naming them. Env overlay per (binary, variant) built once; per-test overlay into the slot's buffer (0 allocs per launch). Record: env pairs are variable-length vs the fixed 328-byte SchedRecord (proxima-test/src/sched.rs:55-73) — prefer a second record kind in the same section over growing the fixed record | runtime-config correctness across settings without rebuilds; no test-to-test env leakage | a 2-value env_matrix on 1 test yields 2 result lines with labels; a test mutating its env never changes a sibling's observed env (counted across 100 runs); process group with mismatched env -> error |
| MATRIX axis (owner 2026-10-11: "how do we enable multi feature testing in parallel", prompted by omega/src/msl/mod.rs:188-192 gating route_compaction_key to metal+macos while emit_and_classify.rs:1817 calls it unconditionally — a metal+linux combination nothing built). New axis, admitted: a run fans out over cells = (feature set, target triple, host). Layer 1 check matrix: `cargo check --all-targets` per cell, feature sets enumerated by cargo val from `cargo metadata` (each feature, default, all, no-default; powerset only on request) — catches the cfg/feature class without native hosts. Layer 2 test matrix: declared cells only, built+run natively (Linux cells on versailles, macOS local). Parallelism: one target dir per cell (cargo locks a target dir; ~19 GB per Proxima cell observed on versailles); stream tests per cell as its binaries appear; result lines labeled with the cell; fail-fast and no-tests per cell. Config: [matrix] check=[...], targets=[...], [[matrix.test]] features=[...] host="...". Owner 2026-10-11: the matrix IS a Proxima pipe fanout — each cell (features x target x host) is a pipe; the run fans out over cells with `FanOut`/`PipeExt::fanout` (proxima-primitives/src/pipe/fanout.rs, alloc tier — fine on the Prime host) and merges cell result streams back with `FanIn` (pipe/fan_in.rs, no alloc), each result line labelled with its cell; the cell list is data from `cargo metadata` + [matrix] config, so a new cell is config, not code. No new type. Owner 2026-10-11: cargo val does NOT use or invoke cargo-hack. Enumerate feature sets itself from `cargo metadata` (each feature, default, all, no-default, declared combos) and run plain `cargo check`/`cargo test --features ...` per cell in its own target dir; never edit a manifest (cargo-hack's --no-dev-deps rewrote 43 Cargo.toml files in a live checkout) | catches feature x target correctness bugs before main; uses idle hosts | omega mismatch reproduced by the check matrix's metal+linux cell (1 error) on the pre-fix tree and 0 after; N cells listed == cells run |
| AXES = ARCHITECTURE (owner 2026-10-11: "if you have the axes that looks like an fsm sans io"): each axis is a stage — selection (filter pipe), order (history-keyed ordering over a bounded window), concurrency/placement (admission FSM), environment (transform pipe), edge (per-OS pipes), stop policy (FSM over outcomes), observation (observe pipe). The run is their composition; an option = a variant of one stage's config = one conflaguration setting. Not expressible as such a variant => not admitted. The composition is also the fluent builder: `RunConfig::builder().select(..).order(..).schedule(..).stop(..).observe(..).build()`, mirrored 1:1 by config sections [select] [order] [schedule] [stop] [observe]; CLI flags set builder values; parity test builder == TOML == env. Files: one per stage (select.rs, order lives in schedule.rs or history.rs, schedule.rs, child_env.rs, edge_*.rs, report.rs). | | |
| OPTION ADMISSION (owner 2026-10-11: "we do not want to add willy nilly — what's the space and what should we consider"). Axes: selection, order, concurrency/placement, stop policy, observation, environment, debug. An option is admitted only if it (1) is a value on an existing axis (a new axis needs its own case), (2) adds no new state except the one accepted per-test history store, (3) moves the bill (fix-loop or long-suite wall time, or trust: counts, flake reporting), (4) reuses the spelling users already type (cargo test / libtest / nextest), (5) is one conflaguration setting with builder parity (CLI is a view), (6) is deterministic and honest (seeds printed, flakes reported, nothing killed). Verdict on the list below: ADMIT history store (=> --lf/--ff/--sw/--durations/history order), tags + -m (one record field), summary + --durations; LATER on request: retries-with-flaky, seeded shuffle, -k, partition (with distribution); DECLINE for now: debugger rerun (new per-OS edge for an interactive use), JUnit until a CI needs it. | | |
| OPTION ADMISSION row, built 2026-10-11: ignore mode (`-- --ignored`, `-- --include-ignored`, `CARGO_VAL_IGNORED`). Axis: selection. (1) value on the selection axis, (2) no new state: the ignored set is per binary and lives for that binary's listing, (3) trust: 22 `#[ignore]`d proxima-model-interop tests were counted as PASS, (4) libtest spelling, (5) `IgnoreSources` builder == `from_settings`, flag beats setting, (6) deterministic. Discovery: `--list --ignored` prints exactly the ignored names, `name: test`, sorted; set held in `IgnoredSet` (O(1) membership, fixed storage). Classification by the libtest `test result:` line (`LibtestVerdict`), not exit code: 0 passed + 1 ignored = IGNORED; 0/0/0 = error naming the test; no result line (custom harness) falls back to the exit code | the old `... ignored` name-suffix match missed `ignored, <reason>`; classification by result line has no such format dependency | `tests/ignore_modes.rs` fixture: default 3/1/1/1/0, `--ignored` 3/1/0/0/2, `--include-ignored` 3/2/1/0/0 (discovered/passed/failed/ignored/filtered) |
| OPTION ADMISSION row, built 2026-10-11: summary lines (observation axis). One `TargetTable` row per test binary, `<binary>: discovered N, passed P, failed F, ignored I, filtered X`, then `total:` with wall time (monotonic clock). `filtered` is printed because the balance check `passed+failed+ignored+filtered == discovered` needs it; a binary that does not balance is an error naming it. Executed = passed+failed; zero executed follows `--no-tests` (ignored-only selections are red). NO_COLOR: summary lines carry no color | trust: counts | `tests/ignore_modes.rs` exact lines; `src/summary.rs` unit tests |
| BIN GATE, built 2026-10-11 (scheduling, not an option; `CARGO_VAL_BIN_GATE` on or off exists only so the regression can toggle it). Cargo 1.99 compiles an integration-test unit with no ordering edge to its package's bin unit, so cargo val listed and launched the test before `target/debug/<bin>` existed (c92 strace: test exec at 650.78, bin uplift at 653.41; `tests/same_name_targets.rs` NotFound 3 of 3 cold). Rule: kind `test` or `bench` executables are parked (`ParkedArtifacts`, 64 places, FIFO) until every `bin` target of the package emitted its non-test compiler-artifact (`CargoSourceStep::BinBuilt`); bin counts per package come from one `cargo metadata --no-deps --format-version 1` read by a byte-fed scanner (`MetadataScan`, fixed storage); metadata unavailable (spawn, exit, scan fault) means wait for a successful `build-finished` (`CargoSourceStep::BuildFinished { success }`); `success:false`, or cargo EOF with no `build-finished`, drops every still-parked executable unlaunched (c112: both bin units of the c109 failure died by SIGTERM, cargo sent `build-finished` success false, and the gate had launched the test with no bin). Lib and bin unit-test executables never wait, so lib-only packages stream as before | CARGO_BIN_EXE_<name> is set only for integration tests and benches | `tests/fixtures/bin_gate` (large bin, tiny integration test) cold target dir: gate on 3/3 PASS, gate off 3/3 FAIL |
| OPTION ADMISSION row, built 2026-10-11: `--target-dir` (isolated, shared, or a path) and `CARGO_VAL_TARGET_DIR` (environment axis; owner: "there's no reason at all that we should be waiting for clippy for 23 min ... ever"). Observed: `cargo val` in ~/repos/mine/resume sat 18+ min, both cargo children blocked in flock on `/tmp/cargo_target/debug/.cargo-lock` held by another session's `cargo clippy --workspace`; `CARGO_TARGET_DIR` is global so every repo shares one lock, and cargo val's own `--no-run` and `--doc` children serialize on it. Rule: default `isolated` builds in `<cargo's target_directory>/cargo-val/<fnv1a64(workspace_root) hex16>/{test,doc}`, passing `--target-dir=` to each child (`src/isolated_target.rs` `BuildDirectories`); target_directory and workspace_root come from the one `cargo metadata --no-deps` call (`MetadataFacts`), read BEFORE the children start in isolated mode (the other modes keep the overlapped read); `shared` = cargo's directory unchanged; a path = `<path>/{test,doc}`; metadata unusable = cargo's own directory with a stderr notice. Test environment paths come from the artifact paths cargo reports, so they follow the new directories with no change. Lock visibility: a child whose stderr carries `waiting for file lock on <which>` gets one `cargo-val: waiting for <which> held by another cargo process (<dir>)` line per lock kind (`LockWatch`); the package cache lock is named global and not isolable. Cost: dependencies compile once more per workspace than plain `cargo test`; the doc build compiles the library a second time. Precedence flag, then `CARGO_VAL_TARGET_DIR`, then the build default (`CARGO_VAL_BUILD_TARGET_DIR`, default isolated) in `TargetDirSources`; builder == `from_settings` fixture | isolated moves the build lock off every other cargo process and between the two own children; the package-cache lock (CARGO_HOME) stays shared | `tests/isolated_target.rs`: a real `cargo build` of a fixture whose build.rs blocks reading a fifo holds the build lock in the shared target dir; isolated `cargo val` on a second fixture in that dir completes with passed > 0 while the holder is still building; `--target-dir shared` prints the wait line, then completes once the holder is released. Measured 2026-10-11, cargo-val's own repo, `cargo val --tests --no-fail-fast`, debug binary, 257 tests (257 names identical to `cargo test --tests`, 0 differing lines), load_1m 13-26 (box not quiet): shared warm 5.43 s, 5.40 s, 5.42 s; isolated 9.57 s (first run, includes rebuilding the changed test binaries) then 5.93 s and 5.99 s. `cargo metadata --no-deps --locked` on this repo takes 22-26 ms (5 runs) and now runs before the children start in isolated mode; the warm 0.5 s gap is not traced to it (the suite starts many nested cargo val runs, each paying it). Cold isolated, fresh target dir, fail-fast default, load 56, before the rebase: 3422.6 s wall, run stopped early (59 discovered) by a failing test; the time sits in first-exec stalls of every newly linked binary (processes at 0.00 s CPU for minutes), not in cargo val. Cold shared: not measured. Disk of the isolated subtree for the test build: 896 MiB after the runs. |  |
| OPTION ADMISSION row, built 2026-10-11: filterset `-E` / `--filterset` / `CARGO_VAL_FILTERSET` (selection axis). nextest subset: `test() binary() package() all() none()`; `and & or | + not ! -`; precedence not > and,- > or; `~contains` (default for `test()`), `=exact`, `#glob` (default for `binary()` / `package()`, as in nextest). Parser = byte-fed FSM over fixed storage (`src/filterset.rs`), evaluation = pure fn over (package, binary, test) with `memchr::memmem::Finder` built once per run (`Filterset::prepare`); applied at discovery so excluded tests are never launched and count as `filtered`. DELIBERATE DIVERGENCES from nextest: (a) `/regex/` is refused at parse time with a caret naming contains, `=exact`, `#glob`: a regex engine needs `std`/a heap and this binary is `no_std` with a null allocator (regex-lite measured: `compile_error` without `std`; with `std`, duplicate `panic_impl` lang item against `src/main.rs`); (b) glob `{a,b}` alternation is refused; (c) a second `-E` is refused (nextest ORs them); (d) `binary()` matches cargo's target name, `package()` the package name. Malformed input fails before any build | nextest reference (nexte.st filtersets) defaults per predicate | partition `not(e)` and subset `a and b` over the 737-name real interop corpus; malformed table with offsets; `tests/ignore_modes.rs` |
| pytest-inspired options (owner Q 2026-10-11), ranked, NOT built: (1) `--lf`/`--ff`/`--sw` (last-failed only, failed-first, stepwise resume) on the history store; (2) tags: `#[proxima::test(tags = ["gpu","slow"])]` in the binary record + `-m "not gpu"` boolean selector (replaces hand exclusion of model/GPU tests); (3) `--durations=N`; (4) `-k` boolean name expression (subset of nextest -E); (5) `-r` short summary + `--tb=short|long|line` failure-output modes (today truncates at 4 KiB); (6) retries with flaky reporting (pytest-rerunfailures / nextest retries); (7) shuffle with printed, replayable seed; (8) `--debugger lldb|gdb` rerun of one failing test (pytest --pdb analogue, per-OS edge); (9) `--collect-only` == `--list`. Session-scoped fixtures == the `process` flag (built); xdist loadscope/loadfile == process groups + distributed plan | fastest fix loop on large suites; declarative exclusion of heavy tests | (1) after one failure, `--lf` runs exactly 1 test; (2) `-m "not gpu"` on a fixture with 2 gpu-tagged of 10 runs 8; (3) `--durations=3` lists 3 names with times matching the run records |
| GAP AUDIT 2026-10-11 (installed binary 09:45, probes in ~/.cache/cargo-val/gap-audit/probe/p1-p28.log). Ranked: |  |  |
| P0 bug: zero tests run exits 0 (`-- --ignored`, `--lib`, `--examples`, `--benches` all ran 0 and exited 0; main.rs:157; a harness=false binary listing no ": test" lines also yields 0 launches and exit 0, unix_driver.rs:818). Adopt nextest's `--no-tests=fail|warn|pass`, default fail, plus the summary line | N==0 is never green | each of the 4 zero-run probes exits nonzero by default; `--no-tests=pass` restores 0 |
| P0 bug: short `-F feats` and `-m PATH` are not in the value-taking list, so the value becomes the test filter (cli.rs:117-124,133) | correct cargo arg parsing | `cargo val -F x` builds with feature x and runs all tests |
| P1: failure output header names no test (unix_driver.rs:223-237), capture truncates at 4096 bytes (`CAPTURE_CAPACITY`, :64), passing output is discarded; `--nocapture`/`--no-capture`/`--show-output` rejected | usable failure reports; libtest flags | failure block names the test; output > 4 KiB not truncated (or truncation stated with byte count); --show-output prints passing output |
| P1: libtest selection flags rejected after `--`: `--skip`, `--include-ignored`, `--exact`, `--list`, `--format`, `--shuffle(-seed)`, `--exclude-should-panic`; and only ONE positional filter (second = invalid arguments, cli.rs:137) while libtest accepts many | drop-in for `cargo test -- …` | each flag's selected set equals `cargo test -- <flag>`'s on a fixture |
| P1: `cargo val --doc` errors ("can't skip running doc tests with --no-run") — covered by doc tests step 1 | | |
| P2: user `--no-run` / `--message-format` forwarded alongside cargo val's own (unix_driver.rs:862-864) | | duplicates rejected or merged deterministically |
| recommend one integration-test binary per crate | fewer link steps; process-per-test keeps isolation | link share from `cargo build --timings` on proxima |
| Windows: target dir on Dev Drive | Defender first-run scan of new test exes (macOS analogue: syspolicyd stall, 316 s observed) | first-run latency per new binary, Dev Drive vs NTFS |

**Windows resource constraints (owner ask 2026-10-11: "reduce the resource usage
on windows and minimize"; binding for the Windows slices; none measured yet):**

| constraint | resource it bounds | measurement on a native Windows host |
|---|---|---|
| job assigned at creation via `PROC_THREAD_ATTRIBUTE_JOB_LIST` (no suspend/assign/resume); one job per run with KILL_ON_JOB_CLOSE, DIE_ON_UNHANDLED_EXCEPTION (no WerFault.exe / dialog), BELOW_NORMAL priority | syscalls per child; crash-reporter processes; rustc CPU share | child create latency; WerFault count on a panicking-abort fixture = 0 |
| `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` with only the 3 stdio handles; CREATE_NO_WINDOW; absolute exe path; close child thread handle and child pipe ends immediately | inherited and leaked handles | cargo val handle count flat across a 4327-test run |
| command line + UTF-16 environment block built once per binary, reused per test | allocation and CPU per launch | allocations per launched test = 0 |
| one merged stdout+stderr pipe per child by default (separate streams as an option) | pipe handles, kernel pipe quota, overlapped reads, completions — all halved | handles and completions per test |
| small kernel pipe buffer, ~64 KB overlapped reads into the slot's pinned buffer | nonpaged pool per pipe; completions per test | nonpaged pool peak; completions per test |
| one thread, one IOCP, batched `GetQueuedCompletionStatusEx`; no thread pool, no per-child threads | threads and context switches | thread count = 1 + 0 per child |
| fixed slot buffers sized by --jobs | memory O(jobs), not O(tests) | peak commit at 17 vs 4327 tests |
| optional history-driven batching of many tiny tests per process (default off; trades isolation) | process creations | wall and CPU vs per-test processes on many-tiny-test suites |
| recommend target dir on a Dev Drive | Defender first-run scan per new test exe | first-run latency per new binary |

**Owner rule (2026-10-11), binding on every card above:** platform
differences (Windows / Linux / macOS) live only at the edges, wrapped in pipes
with identical In/Out on every OS, and are orchestrated by one platform-neutral
sans-IO FSM. No `cfg(target_os)` / `cfg(windows)` / `cfg(unix)` in orchestration
or stage modules (acceptance: production grep count = 0). The Windows job /
handle-list / overlapped-slot code and the Unix spawn/readiness code are the
per-OS implementations behind those edge pipes.

**Owner rule (2026-10-11): organization is navigation.** A human sees where to
make a change from the file list alone. Flat `src/`, one file per business
concern named for it, platform edges as `edge_<concern>_<os>.rs`, every tunable
in conflaguration in one `config.rs`. Target layout: `main.rs`, `config.rs`,
`run.rs`, `discover_cargo.rs`, `discover_listing.rs`, `select.rs`,
`outcome.rs`, `report.rs`, `history.rs`, `edge_spawn_{unix,windows}.rs`,
`edge_stream_{unix,windows}.rs`, `edge_exit_{macos,linux,windows}.rs`,
`edge_args_{unix,windows}.rs`. Applied incrementally as slices touch files.

Move map (current file -> target; a slice that touches a file moves it):

| current | target | concern |
|---|---|---|
| `cli.rs` | `cli.rs` | command-line parsing only |
| `test_concurrency.rs`, `build.rs` defaults, runtime `Settings` in `host/unix_driver.rs:234-264` | `config.rs` | every setting + precedence + builder (conflaguration) |
| `artifact_stream.rs`, `cargo_message.rs`, `cargo_source.rs` | `discover_cargo.rs` | cargo JSON -> test binaries, build-script outputs |
| `libtest_listing.rs`, `listing_source.rs` | `discover_listing.rs` | libtest `--list` -> test names |
| `host/unix_rustc.rs` | `discover_toolchain.rs` | rustc libdir query |
| `selection.rs`, `test_intent.rs` | `select.rs` | which tests run: filters, exact/skip, ignored |
| `session_slots.rs` | `schedule.rs` | when/how many: slots, groups/serial, test-threads, fail-fast, history order |
| `test_environment.rs` | `child_env.rs` | per-binary environment and cwd (platform-neutral data) |
| `host_event.rs`, `child_completion.rs`, `runner.rs`, `host/unix_driver.rs` loop | `run.rs` | the platform-neutral orchestration FSM (on Prime) |
| `outcomes.rs` | `outcome.rs` | results ledger, counts |
| `reporter.rs` | `report.rs` | lines, failure output, summary, color |
| `unix_arguments.rs` / `windows_arguments.rs` | `edge_args_unix.rs` / `edge_args_windows.rs` | argv/env encoding per OS |
| `host/unix_spawn.rs`, `host/unix_pipe_pair.rs`, `host/unix_env.rs` | `edge_spawn_unix.rs` | spawn edge (posix_spawn, CLOEXEC_DEFAULT) |
| `host/windows_spawn.rs`, `host/windows_handles.rs` (job, handle list), `host/windows_pipe_pair.rs` | `edge_spawn_windows.rs` | spawn edge (CreateProcessW, job, handle list) |
| `host/windows_pipe.rs` (overlapped slot) | `edge_stream_windows.rs` | completion-gated stream edge |
| `host/macos_pipe.rs`, `host/linux_pipe.rs` | `edge_stream_unix.rs` (readiness via Prime) | readiness-gated stream edge |
| `host/macos_exit.rs`, `host/linux_exit.rs` | `edge_exit_macos.rs`, `edge_exit_linux.rs` (kqueue proc / pidfd via Prime) | exit gate edges |
| (new) | `edge_exit_windows.rs` | job-object exit gate |
| (new) | `history.rs` | stored per-test timings |
| `test_alloc.rs`, `test_dir.rs` (cfg(test)) | `test_support.rs` | test-only helpers |
| `lib.rs`, `main.rs` | `lib.rs`, `main.rs` | crate root, entry |

Result: 22 files flat in `src/`, no subdirectory; `host/` disappears.

Owner direction: do not take cores away from rustc; finishing the build is the
priority, tests fill what is idle.

### Changelog
| Date | Change | Δ vs prior | CoV / runs | Host loadout |
|---|---|---|---|---|
| 2026-10-11 | log opened; old c54b rows imported; harness `tools/val-bench` built | – | – | – |
| 2026-10-11 | child environment parity (dylib path, cwd, CARGO_* per binary) — correctness, Proxima 4327/4327 | enables real-suite runs | – | loaded |
| 2026-10-11 | c66: rustc target-libdir query started before cargo, collected before the loop (`unix_rustc.rs:27-81`, `unix_driver.rs:1001,1035-1040,1087`); A/B 0e4c8d5a -> cb4c7e1b, small fixture, 6 alternating blocks, n=30 each | first result 76.9 -> 57.5 ms (-19.4); wall 95.1 -> 75.9 ms (-19.2); ranges disjoint (before min 68.7 > after max 62.6); cargo test reference unchanged (32.2/32.6 first, 68.7/68.9 wall); Proxima still 4327/0 | 3 blocks per side | load 4.9–5.6, not quiet |
