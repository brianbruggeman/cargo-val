# cargo-val

status: draft
owner: brian
created: 2026-10-09

## problem

Rust teams that need per-test process isolation across Windows, macOS, and Linux
must build or wait for every test executable and discover the complete suite
before execution; `cargo val` will consume Cargo artifacts and libtest discovery
incrementally, dispatch exact tests through Proxima process and pipe primitives,
and preserve per-test outcomes on all three native hosts.

## refutation condition

If a native fixture on any supported host cannot start its first discovered test
before discovery/build completion, or if a test's timeout/cancellation can
terminate another test or leave an orphan child, this runner does not meet its
stated execution model.

## requirements

| id | requirement | testable in isolation |
|---|---|---|
| R1 | Build and run the runner natively on Windows, macOS, and Linux using Proxima process support | yes |
| R2 | Consume Cargo JSON compiler-artifact events as a stream and start handling an executable before Cargo exits | yes |
| R3 | Discover names line by line per executable and dispatch a test before discovery reaches EOF; never construct a whole-suite test vector | yes |
| R4 | Run each selected test as an exact process through `proxima_process::Command`; retain its environment, working directory, and exit classification | yes |
| R5 | Bound live child processes with Proxima admission/backpressure primitives; do not pre-enqueue the complete suite | yes |
| R6 | Collect each child's stdout, stderr, exit status, timeout, cancellation, and cleanup without cross-test interference | yes |
| R7 | Load runner settings through `conflaguration`, including worker/admission capacity and color policy | yes |
| R8 | Emit color by default and suppress ANSI when `NO_COLOR` is present | yes |
| R9 | Preserve explicit counts for discovered, filtered, ignored, passed, failed, and unaccounted tests | yes |
| R10 | Record per-child allocation and output payloads on Windows, macOS, and Linux before deciding whether arena storage is warranted | yes |
| R11 | Compare against cargo-nextest with per-test traces and host-labeled throughput, latency, CPU, RSS, errors, and variance | yes |

## architecture

```text
Cargo JSON stdout
  -> Proxima byte/stream source
  -> compiler-artifact filter
  -> per-executable libtest listing stream
  -> exact-test intents
  -> bounded Proxima demand/admission
  -> proxima_process::Command child processes
  -> child stdout/stderr/exit lifecycle
  -> Proxima fan-in
  -> terminal reporter
```

The runner holds at most the configured number of live child processes and the
currently observed artifact/test record. It does not build a Vec containing all
test names. Artifact and discovery streams feed admission directly; bounded
capacity expresses backpressure. Process creation stays process-based. Rayon is
not the test executor: Proxima's background Rayon pool handles CPU jobs, while
`proxima_process::Command` owns OS process launching.

The design reuses `Pipe`, bounded demand/admission, `FanIn`, Proxima runtime
facilities, `conflaguration`, and `proxima_process::Command`. It does not use
`ScatterGather` for suite execution because that primitive waits for all inputs
and collects every result in a `Vec`.

## decisions

| decision | chosen | why not the alternative |
|---|---|---|
| project and command name | package `cargo-val`, executable `cargo-val`, invoked as `cargo val` | crates.io already has `cargo-validate` and its executable `cargo-validate`; the hyphen/underscore spelling does not create a distinct Cargo package |
| discovery | stream Cargo artifacts, then stream names from each ready test executable | a global list phase requires retaining all test names and prevents early dispatch |
| concurrency | bounded live-child admission composed from Proxima primitives | an unbounded whole-suite work queue duplicates test records and hides backpressure |
| execution | one exact test per OS child process | process boundaries preserve test isolation and independently enforce timeout/termination |
| Proxima dependency source | consume the Proxima workspace packages from a pinned Proxima Git revision | the checked `proxima-process`, `proxima-primitives`, and `proxima-runtime` crates are absent from crates.io; a local path would make clean clones fail |
| Windows support | first add a native Windows backend to `proxima-process`, land it on Proxima `main`, then pin that revision in cargo-val | cargo-val must not create a competing process abstraction |
| arena | do not add one until per-child traces identify repeated allocations and existing Proxima storage cannot own them | an arena without allocation-site evidence adds a new lifetime and cleanup model without establishing the source of duplication |
| output color | color enabled by default, including redirected output; `NO_COLOR` presence disables ANSI | the requested default is explicit; the environment override is observable and testable |

## acceptance criteria

Commands below are the acceptance contract for the implementation cards. Each
named fixture test reports its internal record counts as well as the test
result. Commands are prospective until their card lands.

| id | discharges | command | expected |
|---|---|---|---|
| AC1 | R1 | `cargo test -p cargo-val --test platform_process -- native_spawn_wait_stdio` on Windows, macOS, and Linux | each host: 1 case passed, 0 spawn errors, 0 orphan children |
| AC2 | R2 | `cargo test -p cargo-val --test artifact_stream -- two_artifacts_before_build_exit` | 2 executable artifact records in input order, 1 test executable handled before Cargo exit, 0 non-test artifacts launched |
| AC3 | R3 | `cargo test -p cargo-val --test discovery_stream -- dispatch_before_eof` | 1 test dispatched before listing EOF, 3 names discovered, 0 whole-suite materializations |
| AC4 | R4 | `cargo test -p cargo-val --test exact_process -- selected_tests_once` | 3 selected names, 3 exact child invocations, 0 duplicate invocations |
| AC5 | R5 | `cargo test -p cargo-val --test admission -- seven_tests_two_slots` | 7 started, 7 completed, peak live children 2, 0 dropped intents |
| AC6 | R6 | `cargo test -p cargo-val --test child_lifecycle -- pass_assertion_abort_timeout_cancel` | 5 outcomes classified, 0 cross-test terminations, 0 orphan children |
| AC7 | R7 | `cargo test -p cargo-val --test settings -- config_env_cli_precedence` | 4 precedence cases passed, 0 invalid capacities accepted |
| AC8 | R8 | `cargo test -p cargo-val --test color_policy -- no_color_matrix` | 4 cases: 2 ANSI outputs with `NO_COLOR` absent (TTY and pipe), 2 ANSI-free outputs when `NO_COLOR` is present (TTY and pipe) |
| AC9 | R9 | `cargo test -p cargo-val --test accounting -- mixed_suite` | 5 discovered = 1 filtered + 1 ignored + 2 passed + 1 failed, 0 unaccounted |
| AC10 | R10 | `cargo test -p cargo-val --test allocation_trace -- child_payload_record` on Windows, macOS, and Linux | each host retains 1 raw per-child allocation record and 1 output payload; no arena claim is made by this measurement |
| AC11 | R11 | `cargo test -p cargo-val --test comparison_record -- complete_host_record` | 1 complete comparison record per host contains throughput, single-request and percentile latency, CPU, RSS, errors, CoV, corpus, profile, baseline, and raw test records |

## out of scope

- Reproducing every cargo-nextest filter expression, reporter, archive, retry,
  setup-script, test-group, and JUnit feature in the first usable release.
- Shared-process tests or a custom harness protocol.
- Claiming zero allocations, a performance win, or an arena benefit before
  platform traces and matched comparisons establish it.
- Modifying cargo-nextest or Cargo itself.

## risks

| risk | likelihood | what it costs | what we do about it |
|---|---|---|---|
| Cargo's artifact stream and a test binary's listing stream do not expose records early enough to beat full discovery | medium | the requested early-start behavior cannot be delivered by this interface | make the first-dispatch-before-EOF fixture a gate before investing in the runner |
| Windows process creation, job cleanup, or stdio differs from Unix behavior | high | platform coverage and isolation are incomplete | land and test the Proxima Windows backend before runner integration |
| bounded capacity is confused with a queue of all undiscovered work | medium | memory grows with suite size and delays execution | represent only currently observed intents and active-child permits |
| Windows allocation traces do not support reuse via an arena | medium | arena work would add overhead without eliminating repeated allocation | keep arena work conditional on allocation-site traces |
| `cargo val` package command collides with another installed binary | low | users invoke an unexpected subcommand | package/executable name is distinct from existing `cargo-validate`; check command resolution in install fixtures |

## context

- Proxima's typed pipe surface: `proxima/proxima-primitives/src/pipe/primitives.rs`.
- Proxima's finite-source fair merge: `proxima/proxima-primitives/src/pipe/fan_in.rs`.
- Bounded backpressure and demand semantics: `proxima/proxima-primitives/src/pipe/bounded.rs`, `demand.rs`.
- Whole-response materialization shape to avoid: `proxima/proxima-primitives/src/pipe/scatter_gather.rs`.
- Existing Proxima process surface and Windows gaps: `proxima/proxima-process/build.rs`, `src/command.rs`, `src/spawn.rs`, `src/fork_server.rs`.
- Proxima preallocated inbox example: `proxima/prime/src/core/inbox.rs`.
- Proxima settings conventions: `proxima/prime/src/config.rs`; `conflaguration` is already a workspace dependency.
- Registry dependency check, 2026-10-09: crates.io API lookups for `proxima-process`, `proxima-primitives`, and `proxima-runtime` returned 404; pin a Git revision until those packages are published.
- cargo-nextest's rationale and runner model: [How nextest works](https://www.nexte.st/docs/design/how-it-works/), [runner loop](https://www.nexte.st/docs/design/architecture/runner-loop/), [Why process-per-test](https://www.nexte.st/docs/design/why-process-per-test/).
- crates.io name check, 2026-10-09: [the `cargo-val` API lookup](https://crates.io/api/v1/crates/cargo-val) returned 404, [`cargo-validate`](https://crates.io/api/v1/crates/cargo-validate) returned 200 with executable `cargo-validate`, and [the normalized `cargo_val` lookup](https://crates.io/api/v1/crates/cargo_val) returned 404. A package `cargo-val` is therefore the selected candidate, subject to a final registry check before publication.
