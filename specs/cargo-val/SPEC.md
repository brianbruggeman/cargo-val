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
| R1 | Build and run the runner natively on Windows, macOS, and Linux with a narrow cargo-val host adapter composed with Proxima Pipe and poll-I/O primitives | yes |
| R2 | Consume Cargo JSON compiler-artifact events as a stream and start handling an executable before Cargo exits | yes |
| R3 | Discover names line by line per executable and dispatch a test before discovery reaches EOF; never construct a whole-suite test vector | yes |
| R4 | Run each selected test as an exact process through the cargo-val host adapter; inherit the parent environment, overlay per-test values for that child only, preserve the working directory, and classify its exit | yes |
| R5 | Keep live child processes in coordinator-owned fixed session slots; do not pre-enqueue the complete suite or use a semaphore | yes |
| R6 | Collect each child's stdout, stderr, exit status, timeout, cancellation, and cleanup without cross-test interference | yes |
| R7 | Load typed runtime settings through a no_std-capable `conflaguration` path, including maximum live child slots and color policy; read host environment bytes through the narrow cargo-val host adapter | yes |
| R8 | Emit color by default and suppress ANSI when `NO_COLOR` is present | yes |
| R9 | Preserve explicit counts for discovered, filtered, ignored, passed, failed, and unaccounted tests | yes |
| R10 | Record per-child allocation and output payloads on Windows, macOS, and Linux before deciding whether arena storage is warranted | yes |
| R11 | On the same multi-test fixture, demonstrate more than 3% lower median end-to-end wall clock than `cargo test` and no slower wall clock than cargo-nextest, with per-test traces and host-labeled throughput, latency, CPU, RSS, errors, and variance | yes |
| R12 | Keep the runner and Proxima child lifecycle poll-driven: no blocking wait/read/write, busy-poll fallback, per-child worker thread, or `block_on` bridge | yes |
| R13 | Keep `std` absent from the shipped cargo-val binary and its complete production dependency graph, including the host adapter and Proxima primitives it uses | yes |
| R14 | Document the native cargo-val invocation and link each example to its matching fixture and acceptance command | yes |
| R15 | Keep mutable runner state under one async coordinator owner; use no semaphore, mutex, permit pool, or waiter queue, and pass internal payloads as borrowed slices | yes |
| R16 | Eliminate repeated per-test heap allocations after startup with caller-owned fixed storage; trace allocation sites and name any unavoidable OS representation copy at its boundary | yes |

## architecture

```text
                    FSM × sans-IO
Cargo stdout bytes -> newline FrameCodec -> Cargo JSON record FSM
                                      |
                                      v
                           Pipe: artifact -> list
                                      |
                    FSM × sans-IO      |
libtest bytes -> newline FrameCodec -> test-name FSM -> Filter/selector
                                      |
                     Pipe: pull one intent only when a session slot is free
                                      |
                    FSM × sans-IO      |
test intent -> cargo-val host-session FSM -> OS readiness events
                                      |
                         FanIn: child events
                                      |
                    FSM × sans-IO      |
child bytes/status -> outcome FSM -> reporter Pipe -> terminal
```

The dataflow is FSM × sans-IO | Pipe | FSM × sans-IO. The Cargo and libtest
protocol FSMs own partial-record state, name recognition, and typed outcomes
without syscalls. Internal payloads remain borrowed slices. Reuse Proxima
framing and Pipe primitives only where their production path preserves the
no-allocation requirement; an allocating shared encode contract is not admitted
to the shipped runtime path. Each state transition that passes data downstream
is a typed Proxima `Pipe` (`In -> Result<Out, Err>`), composed with existing
`AndThen`, selection/filter, and source/sink primitives. Errors and terminal
outcomes travel in `Err` or typed output records; there is no parallel `Option`
termination protocol.

The host boundary is the other FSM. The pinned Proxima revision has no
no_std poll-driven child session, so cargo-val owns the narrow OS edge for
process creation, readiness, stdio, exit observation, timeout, cancellation,
and reaping. It emits typed events through Proxima's no_std `Pipe` and poll-I/O
contracts into the sans-IO outcome FSM; it is not a second generic process
framework. A bounded OS launch call is allowed. After launch, no blocking
child wait/read/write, busy-poll fallback, per-child helper thread, or
`block_on` bridge is allowed. Linux, macOS, and Windows each require native
event and lifecycle evidence before support is claimed. Proxima's named
`SourcePipe` is std/alloc-coupled to `Signal`; `DrainFanIn` lends bytes but has
no waker. Neither is represented here as a ready child-event source.

One coordinator owns Prime's no_std `LocalExecutor`, its task Wakers, the fixed
child slots, and the OS event port. A pending child read or exit interest stores
a Waker in its slot; a ready epoll/kqueue/IOCP record wakes that slot, then the
coordinator calls `LocalExecutor::tick()` to poll only ready futures. Prime's
`RemoteWake` callback signals the same port (`eventfd`, `EVFILT_USER`, or an
IOCP control packet) after a cross-thread wake enters Prime's ready set. The
driver drains ready work before entering the one kernel event wait; a control
wake after the drain prevents a lost wake. The kernel wait parks the event
loop, not a child-specific worker. `LocalExecutor::block_on` is excluded because
its source loops without a reactor when no task is ready. C36a proves the
pending-before-event state transition before native host wiring.

Discovery is pull-driven. Each selected executable is listed as a byte stream;
one `TestIntent` is read, filtered/accounted, admitted, and launched before the
next name is pulled. The process reactor yields ready child IDs as one event
source; it must not require scanning a suite-sized vector on every wake. The
runner retains only the current Cargo record, current test-name frame, bounded
child slots, and bounded per-child buffers. A fixed-capacity Proxima `FanIn`
may merge readiness sources when its scan is over the configured fixed arity;
the event-loop design must not use `ScatterGather`, which waits for all inputs
and collects every result in a `Vec`.

Tier boundaries are deliberate. The record, selection, and outcome FSMs must
compile at Proxima's no_std + no-alloc floor using borrowed slices and
caller-owned buffers. The shipped runtime remains `no_std`; conflaguration's
no_std + alloc tier handles runtime environment overrides because the no-alloc
tier has build-time constants only. Any allocation used during configuration
must be traced separately from the steady-state per-test path. The no-alloc
floor uses conflaguration-generated constants seeded from the same defaults.
Settings default to color enabled; `NO_COLOR` presence disables ANSI. cargo-val
does not parse configuration independently.

The async coordinator is the sole owner of mutable runner state and the fixed
set of child-session slots. It pulls one test name only when a slot is free,
places the resulting child session directly in that slot, and resumes discovery
when a child event retires a slot. There is no semaphore, mutex, permit pool,
waiter queue, or discovered-name queue. The session slots are lifecycle state,
not a second work queue; this single-owner model is the lock-free coordination
design. Internal payloads are lent by reference, with no intermediate copy or
allocation. The production runner uses caller-owned fixed storage and must
record any required OS representation copy at the exact boundary that requires
it. Native per-child allocation traces must show whether repeated allocations
remain and identify each source before a zero-allocation steady-state claim.

Locking and allocation are measured at the host boundary. The current core
`Signal` is std-gated and its waiter registry uses a mutex. Proxima's no_std
`Notify` is alloc-gated and stores a boxed event listener when polled. The
pinned Proxima process API has no no_std poll-driven child session. The host
adapter therefore uses the already available Proxima `Pipe` and poll-I/O
contracts, plus fixed session slots and caller-owned buffers, while calling
only the OS operations required to produce readiness and child events. It
does not add a semaphore, permit, waiter or test-name queue, or a new generic
reactor. Cancellation and timeout are per-session state transitions; their
native cleanup and allocation sites must be measured before an arena decision.
Rayon is not the process executor.

Configuration is conflaguration-based. `CargoValSettings` is the single
serializable, validated settings value and fluent builder target. The initial
runtime fields are:

| field | source key | default | meaning |
|---|---|---|---|
| `color` | `CARGO_VAL_COLOR` | `true` | emit ANSI by default; presence of `NO_COLOR`, including an empty value, forces `false` |
| `jobs` | `CARGO_VAL_JOBS` | generated platform default | maximum simultaneously admitted child sessions |

The build-time conflaguration codegen owns the defaults, including the
platform-specific `jobs` value. The runtime settings default is seeded from
those generated constants; there is no second handwritten default table.
Resolution order is generated defaults, borrowed environment pairs supplied
by the host adapter, then explicit CLI overrides, followed by `Validate`. `NO_COLOR`
presence is represented as an input pair and resolved by conflaguration's
presence-aware typed mapping; cargo-val does not parse environment strings or
implement a parallel settings parser. The no_std + no-alloc FSM build consumes
generated constants only. The no_std + alloc CLI uses conflaguration's typed
borrowed-pair API. The current conflaguration crate lacks that API, so F1-F12
are prerequisites rather than assumed APIs. Build scripts, Cargo, rustc, and
host test harnesses are outside the shipped binary dependency graph.

## decisions

| decision | chosen | why not the alternative |
|---|---|---|
| project and command name | package `cargo-val`, executable `cargo-val`, invoked as `cargo val` | crates.io already has `cargo-validate` and its executable `cargo-validate`; the hyphen/underscore spelling does not create a distinct Cargo package |
| discovery | stream Cargo artifacts, then stream names from each ready test executable | a global list phase requires retaining all test names and prevents early dispatch |
| concurrency | store active child sessions directly in a fixed coordinator-owned slot table | a semaphore or a separate queue duplicates lifecycle state; a whole-suite queue prevents incremental dispatch |
| execution | one exact test per OS child process | process boundaries preserve test isolation and independently enforce timeout/termination |
| Proxima dependency source | consume the Proxima workspace packages from a pinned Proxima Git revision | the checked `proxima-process`, `proxima-primitives`, and `proxima-runtime` crates are absent from crates.io; a local path would make clean clones fail |
| Windows support | use overlapped named pipes for child stdio and a fixed-slot exit notification into one IOCP; prove callback lifetime and reaping natively | anonymous `CreatePipe` cannot perform overlapped I/O; an OS callback is not a dedicated per-child helper thread |
| fully async and no_std | implement only the missing host edge in cargo-val, reusing Proxima `Pipe`, poll-I/O, and sans-IO forms; remove blocking child wait/read/write, per-child helper threads, and `block_on` | the pinned Proxima process and Prime OS reactor APIs are std-gated, and an async signature around blocking workers does not satisfy the contract |
| runtime admission | coordinator-owned fixed child-session slots, with discovery pulled only when a slot is free | capacity is represented by the actual child lifecycle records; a separate gate, permit pool, or queue would duplicate that state |
| no_std host driving | drive Prime's existing no_std `LocalExecutor` from the fixed-slot cargo-val platform event source | current `PrimeRuntime` and OS reactor are std-gated |
| cancellation | carry cancellation in the owning child slot and wake from the platform event source | current `Signal` is std-gated and mutex-backed; current no_std `Notify` is alloc-gated and boxes a listener |
| runtime configuration without std | use a no_std conflaguration mapping/validation seam on borrowed pairs from the cargo-val host adapter | the current conflaguration env loader calls `std::env` and has no borrowed-pair entry point, so runtime settings remain a separate API dependency |
| child admission | do not pull the next test name until a coordinator-owned session slot is free | no intermediate intent needs to be retained or retried |
| child environment | inherit the parent environment by default; overlay per-test values for one child; never mutate the parent or leak one test's overrides into a sibling | test binaries need Cargo's environment and PATH; process APIs that default to an empty environment would silently break that behavior |
| arena | do not add one until per-child traces identify repeated allocations and existing Proxima storage cannot own them | an arena without allocation-site evidence adds a new lifetime and cleanup model without establishing the source of duplication |
| output color | color enabled by default, including redirected output; `NO_COLOR` presence disables ANSI | the requested default is explicit; the environment override is observable and testable |

## acceptance criteria

The former ACP1-ACP16 synchronous acceptance table is retired. Those rows
used synchronous `std::process` behavior as the process oracle, so they cannot
discharge R12 or R13. The current Proxima `Command` is not accepted as the
execution implementation either: its source audit is recorded in
`TASKS.md` E2-E5. F0, P0, and Q1-Q6 are historical source and native-host
capability probes. The active C35+ host-adapter cards in `TASKS.md` supersede
the former Proxima-repository implementation route. Each implementation row
remains a candidate until its native fixture, independent literal oracle,
control, and bounded packet exist.

The top-level gate is: on native Windows, macOS, and Linux, cargo-val discovers
and executes exact test children while the producer remains active, with no
`std` in cargo-val or its production host-adapter graph, no blocking
wait/read/write, no child helper threads, no lost test intents, and every child
reaped after completion, timeout, or cancellation. Its falsifier is any
supported-host trace showing one forbidden call, a missing/duplicate intent,
or an unreaped child.

The current admission ACs are the source-audit and native-capability probes in
`TASKS.md`:

| id | discharges | command | expected | independent oracle and control |
|---|---|---|---|---|
| AC1 | design gate for R7, R13 | `python3 specs/cargo-val/validate_conflag_gap.py` | 5 source-backed gaps, 1 borrowed-pair API signature, 0 unclassified direct dependency names | reopen every cited conflaguration source hunk from its checkout and recount; a fixture with the borrowed-pair anchor removed is rejected by the same anchor validator |
| AC2 | design gate for R1, R4, R6, R12, R13 | `python3 specs/cargo-val/validate_process_gap.py` | 6 source-backed gaps, 1 candidate child-session signature, 1 missing-anchor negative control rejected | reopen each exact Proxima `main` hunk via `git show 5207d66e:<path>`; a fixture with the blocking-wait source anchor removed is rejected by the same anchor validator |
| AC3 | R12, R13 feasibility probe | Linux: `cargo test -p prime --test child_reactor -- linux_exit_probe` | 1 Pending, 1 kernel wake, 1 exit observation, 0 helper threads; unsupported source falsifies feasibility and does not pass | held-child barrier plus release pipe; libc wait control emits expected exit record and passes the same comparator, while pre-release poll emits no event |
| AC4 | R12, R13 feasibility probe | macOS: `cargo test -p prime --test child_reactor -- macos_exit_probe` | same 4 counts as AC3; unsupported source falsifies feasibility and does not pass | held-child barrier plus release pipe; libc wait control emits expected exit record and passes the same comparator, while pre-release poll emits no event |
| AC5 | R12, R13 feasibility | Windows: `cargo test -p prime --test child_reactor -- windows_handle_probe` | 1 pipe completion and 1 child completion, 0 helper threads; unsupported handles falsify feasibility and do not pass | native `std::process` pipe/child control emits the expected payload and passes the same comparator; non-overlapped negative handle must be classified unsupported |
| AC6 | R12, R13 launch feasibility | Linux: `cargo test -p proxima-process --test child_spawn -- linux_launch_probe` | 1 child handle, 1 status 0; unsupported launch falsifies feasibility and does not pass | libc launch fixture emits the literal child record and passes the same status comparator |
| AC7 | R12, R13 launch feasibility | macOS: `cargo test -p proxima-process --test child_spawn -- macos_launch_probe` | 1 child handle, 1 status 0; unsupported launch falsifies feasibility and does not pass | libc launch fixture emits the literal child record and passes the same status comparator |
| AC8 | R12, R13 launch feasibility | Windows: `cargo test -p proxima-process --test child_spawn -- windows_launch_probe` | 1 child handle, 1 status 0; unsupported launch falsifies feasibility and does not pass | Win32 `CreateProcess` control emits the literal child record and passes the same status comparator |

AC1 and AC2 have executable source-audit scripts and literal oracles. AC3-AC8
and C19a-C19o are historical Proxima-process candidate probes. The active
cargo-val host path is C35-C54 in `TASKS.md`; each native card needs a literal
fixture, independent control, and retained native result. A fixture's own count
is never its oracle.

C2 / AC64 consumes one complete, borrowed Cargo JSON line per call. Its literal
input `oracles/c2.jsonl` has three `compiler-artifact` records, each with a
string `executable` and `target.test=true`: a `cargo-val` binary test harness
with `target.kind=["bin"]` and `profile.test=true`, a normal `cargo-val`
binary with `target.kind=["bin"]` and `profile.test=false`, and a `cli_smoke`
test artifact with `target.kind=["test"]` and `profile.test=true`. Selection
uses `profile.test` plus a non-null, nonempty executable; it does not use
`target.test` or hard-code `target.kind=["test"]`. Cargo records with a null
executable are valid and ignored. The output oracle is `oracles/c2.expected.jsonl`.
`python3 specs/cargo-val/validate_c2_oracle.py` asserts 2 literal test
executables, 1 ignored non-test artifact, 2 test plus 1 non-test artifact in
the retained all-targets Cargo JSON, the same 2 plus 1 from a live Cargo
emitter, and 2 rejected selectors (`target.test`-only and `kind=test`-only).
The validator compares the full retained
`evidence/c2/cargo-all-targets.jsonl` artifact shapes with the literal input
and live `RUSTC_WRAPPER= cargo test --no-run --bin cargo-val --test cli_smoke --message-format=json`
output. `oracles/c2-escaped-fields.jsonl` tests escaped JSON spellings for the
`reason`, `profile.test`, and `compiler-artifact` values. The independent
expected record is `oracles/c2-escaped-fields.expected.jsonl`.
`oracles/c2-windows-escaped.jsonl` adds one hand-authored Windows
artifact whose JSON `executable` contains escaped backslashes and whose
unknown `future` field contains nested objects and an array. The independent
Python JSON decoder and `oracles/c2-windows-escaped.expected.jsonl` bind its
decoded 49-byte executable to `C:\work\cargo-val\target\debug\deps\win_smoke.exe`.
The parser must borrow an unescaped path directly from the input, decode the
escaped path into a caller-owned scratch slice, and return a view into that
slice. The integration test checks the returned pointer lies inside the input
for the unescaped case and inside scratch, outside input, for the escaped case;
it counts 0 parser
allocations around each call, including unknown-field skipping.

C2 accepts caller-supplied `max_record_bytes` and `max_json_depth`, with a
parser recursion ceiling of 16 levels; the fixture sets them to its observed
629 bytes and 4 JSON container levels. A valid
630-byte record (one trailing space), a separate valid depth-5 record in
`oracles/c2-over-depth.jsonl`, and a 48-byte scratch slice for the 49-byte
decoded path must each produce one distinct bounded error and zero emitted
executables. These are test limits, not a second runtime settings parser;
conflaguration remains the settings source. Proxima's
`DelimiterCodec::parse_frame` returns a borrowed slice but its complete-frame
branch does not check `FrameLimits::max_frame_bytes`, and `FrameCodec` requires
`Vec<u8>` for encoding. Proxima's JSON codec is `std`-gated in
`proxima-codec/Cargo.toml:30-46` and `src/lib.rs:213-246`; using it here would
violate the no_std/noalloc record path. C2 therefore uses a small Cargo-record
FSM with caller-owned scratch and explicit limits. The parser does not
authenticate records; its input contract is a complete line from the Cargo
child's `--message-format=json` stdout, not arbitrary user JSON. End-to-end
stream buffering remains a separate C3 framing obligation.

The retained C2 implementation run executes 1 integration test: 2 original
executable records, 1 escaped Windows path, and 1 escaped-key selector emit;
1 production and 1 null-executable record are ignored. It observes borrowed
and scratch-backed views, 0 allocations across 10 measured parser calls, and
1 deliberate allocation detected by the counter. One nested unknown field is
skipped. The parser rejects truncation, root/nested trailing commas, duplicate
selection fields, invalid UTF-8, byte overflow, depth overflow, an invalid
depth limit, and short scratch. The test and oracle outputs are in
`evidence/c2/`; they cover the library parser only, not the shipped binary.

The AC1-AC115 identifiers name historical candidates; they do not imply that all
oracle files or results exist. AC1, AC2, and C2/AC64 have retained source,
oracle, and implementation outputs in their evidence directories.
C3/AC65 has a retained literal oracle, focused macOS framing/Pipe test,
allocation-counter control, and no_std library check in `evidence/c3/`; the
test feeds chunks directly and does not exercise a live Cargo stdout source,
so it does not discharge end-to-end R2.
AC3 has one retained native Linux result.
PR0/AC91 and PR0a/AC111-PR0b/AC112 are historical semaphore records, withdrawn
from the cargo-val design and acceptance map. C27/AC113 has a retained
capacity-gated listing Pipe test and release build evidence in
`evidence/c27/`; the allocation record measures one Pipe call and does not
prove an allocation-free dependency graph. The historical process checks remain
prospective. C6/C27 have retained artifacts; active C30-C54 cards are
prospective until their literal input, runnable command, independent control,
and retained result are present. A
requirement is discharged only after its listed AC records and independent
control have been reviewed.

| requirement | acceptance criteria |
|---|---|
| R1 | C38a-C46c, C47a-C47b, C53a-C53c |
| R2 | C2-C3, C49 |
| R3 | C4-C5, C50-C51a, C53a-C53c |
| R4 | C38a-C46c, C51a-C51b |
| R5 | C6, C27, C35-C36b, C47a-C47b |
| R6 | C40, C42, C45a-C48, C51b, C53a-C53c |
| R7 | C30, F1-F12, C52a-C52d |
| R8 | C30, F10-F12, C52a-C52d |
| R9 | C33-C34, C51b, C53a-C53c |
| R10 | C54a-C54c |
| R11 | C54a-C54c |
| R12 | C35-C48, C53a-C53c |
| R13 | C30g, C30a-C30e, C35-C36b, F1-F12, C52d-C52e |
| R14 | C52d, C53a-C53c |
| R15 | C6, C27, C35-C36b, C45e, C47a-C48o, C49z-C51b, C53a-C53c |
| R16 | C27, C31-C34, C37-C48o, C49z-C51b, C53a-C54c |

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
| Windows process wait and stdio still use blocking `std::process` calls | high | the existing backend cannot drive child lifecycle through the required async event source | use the C43-C48 narrow cargo-val host edge and native controls before runner integration |
| the pinned Proxima process/reactor APIs lack no_std child-exit and pipe readiness | high | the current Command surface cannot provide the required process event source | reuse Proxima Pipe/poll-I/O and prove each host adapter from C38-C48 natively |
| bounded capacity is confused with a queue of all undiscovered work | medium | memory grows with suite size and delays execution | represent only currently observed intents and live sessions in fixed slots |
| Windows allocation traces do not support reuse via an arena | medium | arena work would add overhead without eliminating repeated allocation | keep arena work conditional on allocation-site traces |
| `cargo val` package command collides with another installed binary | low | users invoke an unexpected subcommand | package/executable name is distinct from existing `cargo-validate`; check command resolution in install fixtures |

## context

- Proxima source ref inspected: `main` at `5207d66e9995ffcb4d3b2d2258c02e6fabc4cc6b` in `/Users/brianbruggeman/repos/slot-0/proxima-windows`.
- Pipe algebra: `proxima-primitives/src/pipe/primitives.rs:91-113`; finite-source merge: `proxima-primitives/src/pipe/fan_in.rs:253-282`; materializing `ScatterGather`: `proxima-primitives/src/pipe/scatter_gather.rs:78-89`.
- Process behavior: `proxima-process/src/command.rs:1330-1433` uses worker threads, blocking stdio, `block_on`, and joins; `proxima-process/src/fd_pipe.rs:158-215` uses blocking file operations, worker threads, an unbounded channel, and blocking `waitpid`; `proxima-process/src/spawn_windows.rs:1-74` wraps `std::process`; `proxima-process/src/lib.rs:17-43` gates host spawn and pipe modules on the `std` feature.
- Readiness boundary: `prime/src/os/readiness.rs:1-3,23-30,59-87` uses `std` and reports off-worker busy-poll; `prime/src/os/reactor_windows.rs:1-16,70-101` stores `OwnedSocket` and accepts `AsSocket`, not child pipe or process handles.
- Configuration evidence: conflaguration 2.0.0 `src/lib.rs:398,446,476`, `src/builder.rs:180-236`, and `crates/conflaguration-derive/src/lib.rs:395,423`; the current env resolver and derive output use `std`.
- Registry dependency check, 2026-10-09: crates.io API lookups for `proxima-process`, `proxima-primitives`, and `proxima-runtime` returned 404; pin a Git revision until those packages are published.
- cargo-nextest's rationale and runner model: [How nextest works](https://www.nexte.st/docs/design/how-it-works/), [runner loop](https://www.nexte.st/docs/design/architecture/runner-loop/), [Why process-per-test](https://www.nexte.st/docs/design/why-process-per-test/).
- crates.io name check, 2026-10-09: [the `cargo-val` API lookup](https://crates.io/api/v1/crates/cargo-val) returned 404, [`cargo-validate`](https://crates.io/api/v1/crates/cargo-validate) returned 200 with executable `cargo-validate`, and [the normalized `cargo_val` lookup](https://crates.io/api/v1/crates/cargo_val) returned 404. A package `cargo-val` is therefore the selected candidate, subject to a final registry check before publication.
