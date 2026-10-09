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
before discovery/build completion, if a child lifecycle uses blocking wait/stdio,
busy polling, per-child threads, or `block_on`, or if timeout/cancellation can
terminate another test or leave an orphan child, this runner does not meet its
stated execution model.

## requirements

| id | requirement | testable in isolation |
|---|---|---|
| R1 | Build and run the runner natively on Windows, macOS, and Linux using Proxima process support | yes |
| R2 | Consume Cargo JSON compiler-artifact events as a stream and start handling an executable before Cargo exits | yes |
| R3 | Discover names line by line per executable and dispatch a test before discovery reaches EOF; never construct a whole-suite test vector | yes |
| R4 | Run each selected test as an exact process through Proxima's async child-session API; retain its environment, working directory, and exit classification | yes |
| R5 | Bound live child processes with Proxima admission/backpressure primitives; do not pre-enqueue the complete suite | yes |
| R6 | Collect each child's stdout, stderr, exit status, timeout, cancellation, and cleanup without cross-test interference | yes |
| R7 | Load typed runtime settings through a no_std-capable `conflaguration` path, including worker/admission capacity and color policy; read host environment bytes through a Proxima platform adapter | yes |
| R8 | Emit color by default and suppress ANSI when `NO_COLOR` is present | yes |
| R9 | Preserve explicit counts for discovered, filtered, ignored, passed, failed, and unaccounted tests | yes |
| R10 | Record per-child allocation and output payloads on Windows, macOS, and Linux before deciding whether arena storage is warranted | yes |
| R11 | Compare against cargo-nextest with per-test traces and host-labeled throughput, latency, CPU, RSS, errors, and variance | yes |
| R12 | Keep the runner and Proxima child lifecycle poll-driven: no blocking wait/read/write, busy-poll fallback, per-child worker thread, or `block_on` bridge | yes |
| R13 | Keep `std` absent from the shipped cargo-val binary and its production dependency graph, including Proxima process sessions; host operations use Proxima's async/no_std-capable substrate | yes |
| R14 | Document the native cargo-val invocation and link each example to its matching fixture and acceptance command | yes |

## architecture

```text
Cargo JSON stdout
  -> Proxima byte/stream source
  -> compiler-artifact filter
  -> per-executable libtest listing stream
  -> exact-test intents
  -> lossless async admission (await a Proxima semaphore before pulling an intent)
  -> Proxima async child-session source
  -> child stdout/stderr/exit lifecycle
  -> Proxima fan-in
  -> terminal reporter
```

The runner holds at most the configured number of live child processes and the
currently observed artifact/test record. It does not build a Vec containing all
test names. Artifact and discovery streams feed admission directly; the runner
awaits an async semaphore before pulling another intent. It does not use
`Demand` for admission because disarmed Demand consumes and drops its input.
Process sessions retain pending native operations and buffers across polling;
no worker thread, busy-poll fallback, blocking wait/stdio, or executor bridge
is used. Native process creation is a bounded OS submission call; the child
lifecycle after submission is entirely readiness-driven. This requires a new
reactor-driven Proxima process-session capability for child stdio and exit
notification on Linux, macOS, and Windows. Rayon is not the test executor.

The design reuses `Pipe`, async semaphore admission, `FanIn`, Proxima runtime
facilities, `conflaguration`, and Proxima process-session primitives. The
configuration boundary must be available without `std`: Proxima reads the
selected environment bytes using its platform adapter, and the typed
conflaguration layer parses and validates those bytes. The current
conflaguration environment loader calls `std::env`; its no_std typed mapping
path is therefore a prerequisite, not an API this spec assumes already exists.
Build scripts, Cargo, rustc, and host test harnesses are outside the shipped
binary dependency graph. It does not use
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
| fully async and no_std | add a Proxima-owned reactor-driven child session; remove blocking process wait/read/write, per-child worker threads, and `block_on`; keep `std` out of the shipped cargo-val binary and its production dependency graph | an async signature around blocking workers does not satisfy the execution contract |
| runtime configuration without std | add a no_std conflaguration mapping/validation seam and obtain only required environment bytes through Proxima's platform adapter | the current conflaguration env loader calls `std::env`; runtime configuration cannot use that loader in a no_std binary |
| child admission | await a Proxima semaphore before consuming another discovered test intent | `Demand` drops inputs while disarmed; a bounded queue alone does not supply async lossless backpressure |
| arena | do not add one until per-child traces identify repeated allocations and existing Proxima storage cannot own them | an arena without allocation-site evidence adds a new lifetime and cleanup model without establishing the source of duplication |
| output color | color enabled by default, including redirected output; `NO_COLOR` presence disables ANSI | the requested default is explicit; the environment override is observable and testable |

## acceptance criteria

The former ACP1-ACP16 synchronous acceptance table is retired. Those rows
used synchronous `std::process` behavior as the process oracle, so they cannot
discharge R12 or R13. The current Proxima `Command` is not accepted as the
execution implementation either: its source audit is recorded in
`TASKS.md` E2-E5. F0, P0, and Q1-Q6 are admitted as source and native-host
capability probes. Implementation rows remain candidates until the probes
produce their records and each row has a bounded work packet, an independent
literal oracle, and an incumbent control.

The top-level gate is: on native Windows, macOS, and Linux, cargo-val discovers
and executes exact test children while the producer remains active, with no
`std` in cargo-val or the Proxima process-session implementation, no blocking
wait/read/write, no child helper threads, no lost test intents, and every child
reaped after completion, timeout, or cancellation. Its falsifier is any
supported-host trace showing one forbidden call, a missing/duplicate intent,
or an unreaped child.

The current admission ACs are the source-audit and native-capability probes in
`TASKS.md`:

| id | discharges | command | expected | independent oracle and control |
|---|---|---|---|---|
| AC1 | design gate for R7, R13 | `python3 specs/cargo-val/validate_conflag_gap.py` | 5 source-backed gaps, 1 borrowed-pair API signature, 0 unbound runtime dependencies | reopen every cited conflaguration source hunk from its checkout and recount; current `Settings::from_env` source control must pass the same gap-count parser |
| AC2 | design gate for R1, R4, R6, R12, R13 | `python3 specs/cargo-val/validate_process_gap.py` | 4 source-backed gaps, 1 typed child-session signature, 0 blocking calls admitted | reopen each exact Proxima `main` hunk via `git show 5207d66e:<path>`; existing synchronous child fixture must pass the same output-record comparator |
| AC3 | R12, R13 feasibility | Linux: `cargo test -p prime --test child_reactor -- linux_exit_probe` | 1 Pending, 1 kernel wake, 1 exit observation, 0 helper threads; or 1 explicit unsupported-source record | held-child barrier plus release pipe; libc wait control emits expected exit record and passes the same comparator, while pre-release poll emits no event |
| AC4 | R12, R13 feasibility | macOS: `cargo test -p prime --test child_reactor -- macos_exit_probe` | same 4 counts as AC3, or 1 explicit unsupported-source record | held-child barrier plus release pipe; libc wait control emits expected exit record and passes the same comparator, while pre-release poll emits no event |
| AC5 | R12, R13 feasibility | Windows: `cargo test -p prime --test child_reactor -- windows_handle_probe` | 1 pipe completion and 1 child completion, 0 helper threads; or 1 explicit unsupported-handle record | native `std::process` pipe/child control emits the expected payload and passes the same comparator; non-overlapped negative handle must be classified unsupported |
| AC6 | R12, R13 launch feasibility | Linux: `cargo test -p proxima-process --test child_spawn -- linux_launch_probe` | 1 child handle, 1 status 0; or 1 explicit unsupported-launch record | libc launch fixture emits the literal child record and passes the same status comparator |
| AC7 | R12, R13 launch feasibility | macOS: `cargo test -p proxima-process --test child_spawn -- macos_launch_probe` | 1 child handle, 1 status 0; or 1 explicit unsupported-launch record | libc launch fixture emits the literal child record and passes the same status comparator |
| AC8 | R12, R13 launch feasibility | Windows: `cargo test -p proxima-process --test child_spawn -- windows_launch_probe` | 1 child handle, 1 status 0; or 1 explicit unsupported-launch record | Win32 `CreateProcess` control emits the literal child record and passes the same status comparator |

AC1-AC8 are prospective until their scripts/fixtures and retained outputs
exist. The complete candidate acceptance map is in the requirement-coverage
section of `TASKS.md`; no implementation candidate is admitted until its own
fixture oracle and control are checked. C19a-C19c are the final native
platform gates and name spawn/environment/cwd, stdout/stderr, pass/fail/abort,
timeout, and cancellation as separate fixture categories. A fixture's own
count is never its oracle.

The AC1-AC90 contract is complete in `TASKS.md`: each AC is the exact
count-bearing command, expected record, independent oracle, and control in its
card/oracle pair. The table below binds every requirement to those ACs. ACs
outside AC1-AC8 remain prospective implementation criteria; they define the
required evidence but do not claim that evidence has been produced. A
requirement is discharged only when its listed AC records and independent
control exist and have been reviewed.

| requirement | acceptance criteria |
|---|---|
| R1 | AC3-AC8, AC27, AC58, AC60-AC62, AC83-AC85, AC90 |
| R2 | AC64-AC65 |
| R3 | AC66-AC67 |
| R4 | AC32-AC40, AC47-AC55, AC70-AC71, AC83-AC85 |
| R5 | AC68-AC69 |
| R6 | AC41-AC55, AC72-AC75, AC83-AC85 |
| R7 | AC1, AC9-AC21, AC76-AC77 |
| R8 | AC11-AC13, AC78 |
| R9 | AC79, AC83-AC85 |
| R10 | AC80-AC82 |
| R11 | AC86-AC89 |
| R12 | AC2-AC8, AC22-AC57, AC68-AC75, AC83-AC85 |
| R13 | AC1-AC2, AC9-AC18, AC32, AC35, AC38, AC56-AC63 |
| R14 | AC90 |

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
| Proxima lacks child-exit waker registration and Windows pipe-handle readiness | high | fully async process lifecycle cannot be implemented by composing the current Command surface | add and validate reactor-backed child session ownership before cargo-val process integration |
| bounded capacity is confused with a queue of all undiscovered work | medium | memory grows with suite size and delays execution | represent only currently observed intents and active-child permits |
| Windows allocation traces do not support reuse via an arena | medium | arena work would add overhead without eliminating repeated allocation | keep arena work conditional on allocation-site traces |
| `cargo val` package command collides with another installed binary | low | users invoke an unexpected subcommand | package/executable name is distinct from existing `cargo-validate`; check command resolution in install fixtures |

## context

- Proxima source ref inspected: `main` at `5207d66e9995ffcb4d3b2d2258c02e6fabc4cc6b` in `/Users/brianbruggeman/repos/slot-0/proxima-windows`.
- Pipe algebra: `proxima-primitives/src/pipe/primitives.rs:91-113`; finite-source merge: `proxima-primitives/src/pipe/fan_in.rs:253-282`; disarmed Demand behavior: `proxima-primitives/src/pipe/demand.rs:124-132`; async permit acquisition: `proxima-primitives/src/sync/semaphore.rs:78-92`; materializing `ScatterGather`: `proxima-primitives/src/pipe/scatter_gather.rs:78-89`.
- Process behavior: `proxima-process/src/command.rs:1330-1433` uses worker threads, blocking stdio, `block_on`, and joins; `proxima-process/src/fd_pipe.rs:158-215` uses blocking file operations, worker threads, an unbounded channel, and blocking `waitpid`; `proxima-process/src/spawn_windows.rs:1-74` wraps `std::process`; `proxima-process/src/lib.rs:17-43` gates host spawn and pipe modules on the `std` feature.
- Readiness boundary: `prime/src/os/readiness.rs:1-3,23-30,59-87` uses `std` and reports off-worker busy-poll; `prime/src/os/reactor_windows.rs:1-16,70-101` stores `OwnedSocket` and accepts `AsSocket`, not child pipe or process handles.
- Configuration evidence: conflaguration 2.0.0 `src/lib.rs:398,446,476`, `src/builder.rs:180-236`, and `crates/conflaguration-derive/src/lib.rs:395,423`; the current env resolver and derive output use `std`.
- Registry dependency check, 2026-10-09: crates.io API lookups for `proxima-process`, `proxima-primitives`, and `proxima-runtime` returned 404; pin a Git revision until those packages are published.
- cargo-nextest's rationale and runner model: [How nextest works](https://www.nexte.st/docs/design/how-it-works/), [runner loop](https://www.nexte.st/docs/design/architecture/runner-loop/), [Why process-per-test](https://www.nexte.st/docs/design/why-process-per-test/).
- crates.io name check, 2026-10-09: [the `cargo-val` API lookup](https://crates.io/api/v1/crates/cargo-val) returned 404, [`cargo-validate`](https://crates.io/api/v1/crates/cargo-validate) returned 200 with executable `cargo-validate`, and [the normalized `cargo_val` lookup](https://crates.io/api/v1/crates/cargo_val) returned 404. A package `cargo-val` is therefore the selected candidate, subject to a final registry check before publication.
