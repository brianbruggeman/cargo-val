# cargo-val

status: audited
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
named fixture test checks output against hand-authored expected records that
are independent of cargo-val's runner. The same semantic fixture is run through
cargo-nextest and its canonical result records must pass the same comparator.
For runner-specific behavior with no nextest equivalent (early dispatch,
`NO_COLOR`, allocation traces), cargo-nextest remains the semantic outcome
control while a separate literal fixture oracle checks that behavior. Commands
are prospective until their card lands.

| id | discharges | command | expected | independent oracle and incumbent control |
|---|---|---|---|---|
| ACP1 | R1 | `cargo test -p proxima-process --test windows_target_gate -- unix_only_negative_control` | 1 Windows target fixture accepted, 1 Unix-only control rejected | compare the Windows `std::process::Command` reference fixture to its literal child record; cargo-nextest runs that same reference fixture and must pass the same comparator, while the Unix-only manifest remains the negative compile control |
| ACP2 | R1 | `cargo test -p proxima-process --test windows_spawn -- spawn_wait_inherited_stdio` on Windows | 1 child spawned, 1 child reaped, 0 orphan children | compare child pid lifecycle and inherited bytes to `std::process::Command`; run the same fixture under cargo-nextest and require the expected exit record |
| ACP3 | R1, R6 | `cargo test -p proxima-process --test windows_stdio -- stdout_bytes` on Windows | 1 stdout payload, 0 byte mismatches, 0 orphan children | compare bytes to a checked-in literal payload and `std::process::Command`; cargo-nextest control must report the fixture pass |
| ACP4 | R1, R6 | `cargo test -p proxima-process --test windows_stdio -- stdin_stderr_bytes` on Windows | 2 byte payloads, 0 byte mismatches, 0 orphan children | compare bytes to checked-in stdin/stderr vectors and `std::process::Command`; cargo-nextest control must report the fixture pass |
| ACP5 | R1, R4 | `cargo test -p proxima-process --test windows_command -- env_and_cwd` on Windows | 3 cases passed, 0 mismatches | compare environment and cwd to independent `std::process::Command` output; cargo-nextest control must pass the same fixture |
| ACP6 | R1 | `cargo test -p proxima-process --test process_contract` in native Windows, macOS, and Linux jobs | each host: 7 process contract cases passed, 0 orphan children | compare each case to the existing Unix std-process behavior or literal Windows vectors; cargo-nextest control must pass the same seven fixture cases on each native host |
| AC0 | R1 | `cargo test -p cargo-val --test cli_smoke -- help_banner` | 1 fixture passed, 1 `cargo val --help` banner observed | compare exact banner bytes to a checked-in expected string; Cargo's own external-subcommand resolution is the control and must resolve `cargo-val` as `cargo val` |
| AC0a | R1, R4, R5 | `cargo test -p cargo-val --test dependency_source -- pinned_proxima_revision` | 3 Proxima packages resolve from the same pinned Git revision, 1 mismatched-revision fixture rejected | compare package source IDs to the pinned revision in `Cargo.lock`; the test invokes `cargo tree` for both fixtures, then cargo-nextest runs the valid fixture and must pass the same source-id comparator |
| AC1 | R2 | `cargo test -p cargo-val --test artifact_stream -- two_artifacts_before_build_exit` | 2 executable artifact records in input order, 1 test executable handled before Cargo exit, 0 non-test artifacts launched | compare against two hand-authored Cargo JSON event lines; run the same fixture suite with cargo-nextest and require both canonical test result records to match the literal oracle |
| AC2 | R3 | `cargo test -p cargo-val --test discovery_stream -- line_parser_counts` | 3 names parsed, 0 malformed names accepted | compare to three hand-authored libtest listing lines; cargo-nextest's listing of the same fixture must pass the same canonical name comparator |
| AC3 | R3 | `cargo test -p cargo-val --test discovery_stream -- dispatch_before_eof` | 1 test dispatched before listing EOF, 3 names discovered, 0 whole-suite materializations | a barrier-controlled stream fixture is the independent ordering oracle; cargo-nextest runs the same fixture and must pass the shared three-name/result comparator |
| AC4 | R4 | `cargo test -p cargo-val --test exact_process -- selected_tests_once` | 3 selected names, 3 exact child invocations, 0 duplicate invocations | compare invocation names and marker-file records to a hand-authored exact-selection vector; cargo-nextest must pass the same selected fixture and canonical outcome comparator |
| AC5 | R5 | `cargo test -p cargo-val --test admission -- seven_tests_two_slots` | 7 started, 7 completed, peak live children 2, 0 dropped intents | compare start/finish records to a fixed seven-item fixture and a two-permit state trace; cargo-nextest must produce the same seven final outcome records |
| AC6 | R6 | `cargo test -p cargo-val --test child_lifecycle -- pass_failure_abort` | 3 outcomes classified, 0 cross-test terminations, 0 orphan children | compare exit classes to three hand-authored child response vectors; cargo-nextest must pass the same semantic fixtures and outcome comparator |
| AC7a | R6 | `cargo test -p cargo-val --test child_output -- stdout_stderr_payloads` | 2 payloads retained byte-for-byte, 0 mismatches | compare bytes to checked-in stdout/stderr vectors; cargo-nextest must pass the same payload fixture and byte comparator |
| AC7 | R7 | `cargo test -p cargo-val --test settings -- config_env_cli_precedence` | 4 precedence cases passed, 0 invalid capacities accepted | compare the four resolved settings to a literal precedence table; cargo-nextest runs the same settings fixture and must pass the same expected-value checks |
| AC8 | R8 | `cargo test -p cargo-val --test color_policy -- no_color_matrix` | 4 cases: 2 ANSI outputs with `NO_COLOR` absent (TTY and pipe), 2 ANSI-free outputs when `NO_COLOR` is present (TTY and pipe) | compare ANSI presence to four fixed byte-level expectations; cargo-nextest output is run through the same presence checker and must pass its color-policy control cases |
| AC9 | R9 | `cargo test -p cargo-val --test accounting -- mixed_suite` | 5 discovered = 1 filtered + 1 ignored + 2 passed + 1 failed, 0 unaccounted | compare to a hand-counted five-record outcome table; cargo-nextest's output for the same fixture must pass the identical accounting comparator |
| AC10 | R10 | `cargo test -p cargo-val --test allocation_trace -- child_payload_record` on Windows, macOS, and Linux | each host retains 1 raw per-child allocation record and 1 output payload; no arena claim is made by this measurement | compare output payload to a fixed byte vector and allocation records to host profiler raw events; cargo-nextest is run on the same child and its output payload must pass the same byte comparator |
| AC11 | R11 | `cargo test -p cargo-val --test comparison_record -- matched_host_outcomes` | each native host: 1 paired cargo-val/cargo-nextest record with the same fixture's raw test outcomes | compare both output records to the hand-authored fixture oracle and require both to pass the same canonical outcome comparator |
| AC11a | R11 | `cargo test -p cargo-val --test comparison_record -- latency_throughput_samples` | each native host: 1 sample record contains throughput, single-request latency, percentile latency, raw samples, and CoV | compare sample fields to the independent measurement schema and require the cargo-nextest control record to pass the same schema checker |
| AC11b | R11 | `cargo test -p cargo-val --test comparison_record -- host_resource_samples` | each native host: 1 sample record contains CPU, RSS, error rate, corpus, profile, baseline, and host identity | compare fields to the independent measurement schema and require the cargo-nextest control record to pass the same schema checker |
| AC12 | R6 | `cargo test -p cargo-val --test cancellation -- cancel_two_live_children` | 2 children reaped, 0 launches after cancellation, 0 orphans | compare process ids and exit records to a barrier-controlled fixture; cargo-nextest must pass the same two-child final-outcome comparator |
| AC12a | R6 | `cargo test -p cargo-val --test timeout -- terminate_and_reap_child` | 1 timed-out child classified, 1 child reaped, 0 orphan children | compare elapsed-time decision and exit class to a fixed-clock timeout fixture; cargo-nextest must pass the same child final-outcome control |
| AC13 | R10 | `cargo test -p cargo-val --test allocation_trace -- repeated_process_alloc_sites_and_arena_payload_parity` on Windows | either 1 conditional arena parity case passed with byte-identical payload, or 1 negative trace recorded and 0 arena changes admitted | compare bytes to the fixed payload fixture and repeated allocation sites to retained profiler events; cargo-nextest control must pass the same payload fixture |
| AC14 | R1, R4, R6 | `cargo test -p cargo-val --test platform_acceptance` in native Windows, macOS, and Linux jobs | each host: 1 acceptance record covering 5 fixture categories, 0 unaccounted outcomes, 0 orphan children | compare to the same checked-in platform fixture records; cargo-nextest control must pass all five semantic categories on every native host |
| AC15 | R11 | `cargo test -p cargo-val --test comparison_record -- malformed_control` | 1 malformed record rejected, 0 valid host records rejected | compare candidate and cargo-nextest raw records to a schema with hand-authored expected fields; the malformed fixture must be rejected by the same validator |
| AC16 | R1-R9 | `cargo test -p cargo-val --test docs_contract -- command_examples` | 3 platform examples, 1 command spelling, 0 undocumented platform caveats | compare docs examples to checked-in executable fixture invocations; cargo-nextest runs the same examples and its canonical outcomes must pass the shared result comparator |

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
