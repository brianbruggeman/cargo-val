# cargo-val -- implementation cards

Status: prospective after C0. Each admitted card changes one small behavior,
carries one count-bearing acceptance check, and is one coherent conventional
commit. F0, P0, and Q1-Q6 are the first admission probes. Every proposed card
has a separately specified literal oracle and incumbent/negative control in
the oracle ledger below. Platform implementation waits for its Q probe. If a
card is larger than one Luna session under 20 minutes, split it before work.
The prospective counts are targets, never evidence.
Every commit updates this file with its actual command, counts,
artifact, and hash. The card-specific check and affected crate tests must pass
at that commit. This document does not authorize a commit or push.

The shipped cargo-val binary and **every normal runtime dependency** must
exclude `std`. A `#![no_std]` marker on the top crate alone is insufficient.
Cargo, build scripts, and test harnesses run on the host and are audited
separately. A raw OS create-process call is bounded launch submission;
readiness, stdio, exit, cancellation, and reaping are poll-driven thereafter.
No blocking wait/read/write, `block_on`, per-child helper threads, or busy-poll
fallback is admitted. Reuse Proxima Pipe, AsyncRead/AsyncWrite, reactor,
semaphore, and fan-in surfaces; do not duplicate them in cargo-val.

## opened source that determines the split

Read Proxima `main` at `5207d66e` with
`git -C ../proxima show 5207d66e:<path>` (the shared `../proxima` working
tree is on another branch) and conflaguration 2.0.0 at
`/Users/brianbruggeman/repos/mine/conflaguration`. These pointers describe
existing code; they do not assert that the proposed API exists.

| id | source lines | consequence |
|---|---|---|
| E1 | `/Users/brianbruggeman/repos/mine/conflaguration/src/lib.rs:398,446,476`; `/Users/brianbruggeman/repos/mine/conflaguration/crates/conflaguration-derive/src/lib.rs:395,423` | `Settings` and derive use `std::env` and generated `::std`; typed borrowed-pair input is absent |
| E2 | `../proxima/proxima-process/src/lib.rs:17`; `../proxima/proxima-process/Cargo.toml:13,26,31` | host process API and normal dependencies currently enable `std` |
| E3 | `../proxima/proxima-process/src/command.rs:1335,1363,1407`; `../proxima/proxima-process/src/fd_pipe.rs:160,163,195,212` | current pipes use blocking I/O/wait, child threads, and `block_on` |
| E4 | `../proxima/proxima-process/src/spawn_windows.rs:1,4,38,70` | Windows Child wraps `std::process` |
| E5 | `../proxima/prime/src/lib.rs:9,19`; `../proxima/prime/src/os/readiness.rs:17,25,72,87,136` | host readiness is `std`-gated; Windows constructor accepts sockets, and off-worker path busy-polls |
| E6 | `../proxima/proxima-core/src/io.rs:57,76`; `../proxima/proxima-primitives/src/pipe/primitives.rs:99` | no_std poll I/O and typed Pipe already exist |
| E7 | `../proxima/proxima-primitives/src/sync/semaphore.rs:34,78`; `../proxima/proxima-primitives/src/pipe/demand.rs:124` | async permit admission exists; disarmed Demand consumes input |
| E8 | `../proxima/proxima-primitives/src/pipe/fan_in.rs:1,253`; `../proxima/proxima-primitives/src/pipe/scatter_gather.rs:1,78` | FanIn merges ready sources; ScatterGather collects all results |
| E9 | `src/main.rs:1,5,12`; `Cargo.toml:1,6` | C0 binary imports `std` |

All tests named below are prospective. Their fixtures must emit the listed
counts from independently specified input bytes. Build checks also require
one target artifact and zero compiler errors. Native cards run on their host;
cross-compilation alone cannot establish child lifecycle behavior.
In the tables, a bare `test_binary filter` is command shorthand for
`cargo test -p <table-owner> --test test_binary -- filter`. The table owners
are `conflaguration`, `proxima-process` (except `child_reactor`, whose owner is
`prime`), and `cargo-val`, respectively. The first fully spelled command in
each table shows the expansion; a native-host qualifier is part of the check.

## conflaguration prerequisites

F0 lands in cargo-val as a source-backed gap register; F1-F9, if admitted,
land in `conflaguration`. Its no_std typed source accepts borrowed
platform environment pairs. Keep key naming, defaults, validation, and
`NO_COLOR` presence semantics. Do not create a second settings parser in
cargo-val. The existing `std` env/file convenience API stays outside the
shipped feature graph.

| card / AC / commit | acceptance command and exact count | depends / evidence |
|---|---|---|
| F0 / AC1 `docs: inventory no_std settings api gaps` | `python3 specs/cargo-val/validate_conflag_gap.py` -> 5 source-backed gap rows and 1 concrete borrowed-pair API signature, 0 unbound runtime dependencies; observed 5 / 1 / 0 | E1 plus `/Users/brianbruggeman/repos/mine/conflaguration/Cargo.toml:12-16` and `src/lib.rs:276-388,512-565`; artifact `oracles/f0.jsonl` SHA-256 `74207a43a1a48dd97d19545925aa8a6ffa24e0caf3a228ab3a1cafd8a47cf007`; current `Settings::from_env` source is the compatibility baseline |
| F1 / AC9 `feat: add a no_std settings leaf crate` | `cargo check -p conflaguration-core --no-default-features --lib --message-format=json` -> 1 library artifact, 0 errors; shared harness -> 1 core-only incumbent accepted, 1 `std::env` fixture rejected | F0's admitted API seam; E1; empty typed source surface, no root crate flip |
| F2 / AC10 `feat: look up borrowed setting keys` | `cargo test -p conflaguration-core --test borrowed_settings one_key` -> 1 present, 1 absent, 0 owned copies | F1; E1 |
| F3 / AC11 `feat: distinguish empty setting from absence` | `borrowed_settings present_empty_absent` -> 3 distinct states, 0 collapsed | F2; E1 |
| F4 / AC12 `feat: parse one scalar from borrowed pairs` | `borrowed_settings one_scalar` -> 2 parsed values, 1 invalid rejected | F3; E1 |
| F5 / AC13 `feat: retain validation errors in alloc` | `borrowed_settings validation_failure` -> 1 typed validation failure, 0 panics | F4; `conflaguration/src/lib.rs:298-388` |
| F6 / AC14 `feat: derive two fields from borrowed pairs` | `cargo test -p conflaguration-core --test borrowed_settings derive_two_fields` -> 2 fields resolved, 1 invalid rejected | F5; E1 derive uses generated `::std` |
| F7 / AC15 `refactor: gate conflaguration display behind std` | `cargo test -p conflaguration --test std_compat display_unchanged` -> 1 old display payload unchanged, 0 formatting diffs | F6; `conflaguration/src/lib.rs:512-565` |
| F8 / AC16 `refactor: gate environment helpers behind std` | `cargo test -p conflaguration --test std_compat env_unchanged` -> 1 old env source path unchanged, 0 value diffs | F7; E1; `conflaguration/src/lib.rs:390-489` |
| F8a / AC17 `refactor: gate file helpers behind std` | `cargo test -p conflaguration --test std_compat file_unchanged` -> 1 old file source path unchanged, 0 precedence diffs | F8; E1; `conflaguration/src/lib.rs:250-277` |
| F9 / AC18 `feat: reexport typed core from no_std facade` | `cargo check -p conflaguration --no-default-features --features derive --lib --message-format=json` -> 1 library artifact, 0 errors; normal feature tree -> 0 `std` edges | F8a; E1; end-state named crate remains `conflaguration` |

## Proxima host and process prerequisites

P0 lands in cargo-val as a source-backed gap register; H/R/P1-P21, if
admitted, land in Proxima. Platform bytes and handles belong to its host layer,
readiness to Prime, and the child session to `proxima-process`. P1's existing
target gate at `6cb786ea` remains useful; the subsequent synchronous Windows
tests are only independent child-behavior oracles.

| card / AC / commit | acceptance command and exact count | depends / evidence |
|---|---|---|
| H1 / AC19 `feat: expose linux environment pairs` | native Linux `cargo test -p proxima-process --test host_env linux_pairs` -> 2 borrowed pairs, 0 retained copies | F9; E1,E2 |
| H2 / AC20 `feat: expose macos environment pairs` | native macOS `host_env macos_pairs` -> 2 borrowed pairs, 0 retained copies | H1 contract; E1,E2 |
| H3 / AC21 `feat: expose windows environment units` | native Windows `host_env windows_wide_pairs` -> 2 borrowed wide pairs, 1 empty value distinguished from absence | H1 contract; E1,E4 |
| P0 / AC2 `docs: inventory no_std child reactor seams` | `python3 specs/cargo-val/validate_process_gap.py` -> 4 source-backed gaps (launch, pipe readiness, exit readiness, no_std feature graph), 1 typed session signature, 0 blocking calls admitted | F0; E2-E6; independently reopen each exact `5207d66e` source hunk through `git show`, count four missing seams, and retain the existing synchronous child fixture as the process outcome control; implementation admission gate |
| Q1 / AC3 `test: probe linux kernel child wake source` | native Linux `child_reactor linux_exit_probe` -> 1 Pending, 1 kernel wake, 1 exit observation; 0 helper threads, otherwise 1 explicit unsupported-source record | P0; probe one host source before implementing PR2 or P15 |
| Q2 / AC4 `test: probe macos kernel child wake source` | native macOS `child_reactor macos_exit_probe` -> same 4 fields or 1 explicit unsupported-source record | P0; probe before PR3 or P16 |
| Q3 / AC5 `test: probe windows pipe and child handle completion` | native Windows `child_reactor windows_handle_probe` -> 1 pipe completion and 1 child completion, 0 helper threads; otherwise 1 explicit unsupported-handle record | P0; probe before PR4-PR5 or P17 |
| Q4 / AC6 `test: probe linux no_std child launch` | native Linux `child_spawn linux_launch_probe` -> 1 raw child handle, 1 status 0; otherwise 1 explicit unsupported-launch record | P0; fixed literal program only, before P6 |
| Q5 / AC7 `test: probe macos no_std child launch` | native macOS `child_spawn macos_launch_probe` -> same 2 counts or 1 unsupported-launch record | P0; fixed literal program only, before P7 |
| Q6 / AC8 `test: probe windows no_std child launch` | native Windows `child_spawn windows_launch_probe` -> same 2 counts or 1 unsupported-launch record | P0; fixed literal program only, before P8 |
| PR1 / AC22 `feat: retain no_std reactor registration state` | `cargo test -p prime --test child_reactor registration_lifecycle` -> 1 register, 1 wake, 1 retire, 0 stale wakes | P0's admitted reactor seam; E5; core owner only |
| PR2 / AC23 `feat: wake linux pipe readiness` | native Linux `child_reactor linux_pipe_ready` -> 1 Pending, 1 kernel wake, 1 drained pipe | PR1,Q1; E5 |
| PR3 / AC24 `feat: wake macos pipe readiness` | native macOS `child_reactor macos_pipe_ready` -> same 3 counts | PR1,Q2; E5 |
| PR4 / AC25 `feat: register windows pipe completions` | native Windows `child_reactor windows_pipe_completion` -> 1 completion, 1 wake, 0 stale wakes | PR1,Q3; E4,E5 |
| PR5 / AC26 `feat: wake windows process completion` | native Windows `child_reactor windows_exit_completion` -> 1 completion, 1 wake, 0 per-child waiter threads | PR1,Q3; E4,E5 |
| P1 / AC27 `fix: gate unix process backend by target` | `cargo test -p proxima-process --test windows_target_gate -- unix_only_negative_control` -> 1 Windows fixture accepted, 1 Unix control rejected | recorded `6cb786ea`; E2; native target support gate |
| P2 / AC28 `docs: bind async child session contract` | `cargo test -p proxima-process --test async_session_contract` -> 4 lifecycle sequences, 0 blocking operations admitted | P0,P1; E2-E6 |
| P3 / AC29 `feat: retain one terminal state across polls` | `async_session_state terminal_once` -> 3 exit/cancel/error cases, 1 terminal event each | P2; E6 |
| P4 / AC30 `feat: poll child output through proxima io` | `async_session_state output_poll` -> 1 Pending, 1 Ready payload, 0 duplicates | P3; E6 |
| P5 / AC31 `feat: poll child input through proxima io` | `async_session_state input_poll` -> 1 partial write, 1 wake, 1 completion | P3; E6 |
| P6 / AC32 `feat: submit linux child creation without std` | native Linux `child_spawn linux_submit` -> 1 child handle, 1 status 0, 0 `std` edges | P3,Q4; raw launch submission |
| P6a / AC33 `feat: pass one linux child environment pair` | native Linux `child_spawn linux_env` -> 1 literal env value in child, 0 leaked parent-only keys | P6,H1; E2 |
| P6b / AC34 `feat: set linux child working directory` | native Linux `child_spawn linux_cwd` -> 1 literal cwd observed by child, 0 parent cwd changes | P6a; E2 |
| P7 / AC35 `feat: submit macos child creation without std` | native macOS `child_spawn macos_submit` -> 1 child handle, 1 status 0, 0 `std` edges | P3,Q5; raw launch submission |
| P7a / AC36 `feat: pass one macos child environment pair` | native macOS `child_spawn macos_env` -> 1 literal env value in child, 0 leaked parent-only keys | P7,H2; E2 |
| P7b / AC37 `feat: set macos child working directory` | native macOS `child_spawn macos_cwd` -> 1 literal cwd observed by child, 0 parent cwd changes | P7a; E2 |
| P8 / AC38 `feat: submit windows child creation without std` | native Windows `child_spawn windows_submit` -> 1 child handle, 1 status 0, 0 `std` edges | P3,Q6; E4; raw launch submission |
| P8a / AC39 `feat: pass one windows child environment pair` | native Windows `child_spawn windows_env` -> 1 literal env value in child, 0 leaked parent-only keys | P8,H3; E4 |
| P8b / AC40 `feat: set windows child working directory` | native Windows `child_spawn windows_cwd` -> 1 literal cwd observed by child, 0 parent cwd changes | P8a; E4 |
| P9 / AC41 `feat: drain linux child stdout on readiness` | native Linux `async_stdio linux_stdout` -> 2 chunks, 1 EOF, 0 helper threads | P4,P6b,PR2; E3,E5 |
| P10 / AC42 `feat: drain macos child stdout on readiness` | native macOS `async_stdio macos_stdout` -> same counts | P4,P7b,PR3; E3,E5 |
| P11 / AC43 `feat: drain windows child stdout on completion` | native Windows `async_stdio windows_stdout` -> same counts | P4,P8b,PR4; E4,E5 |
| P12 / AC44 `feat: write linux child stdin on readiness` | native Linux `async_stdio linux_stdin` -> 2 partial writes, 1 EOF, 0 helper threads | P5,P6,PR2; E3 |
| P13 / AC45 `feat: write macos child stdin on readiness` | native macOS `async_stdio macos_stdin` -> same counts | P5,P7,PR3; E3 |
| P14 / AC46 `feat: write windows child stdin on completion` | native Windows `async_stdio windows_stdin` -> same counts | P5,P8,PR4; E4 |
| P15 / AC47 `feat: wake linux child exit without wait thread` | native Linux `async_exit linux_exit` -> 1 wake, 1 reap, 0 wait threads | P6,PR2; E3; kernel completion source |
| P16 / AC48 `feat: wake macos child exit without wait thread` | native macOS `async_exit macos_exit` -> same counts | P7,PR3; E3; kernel completion source |
| P17 / AC49 `feat: wake windows child exit without wait thread` | native Windows `async_exit windows_exit` -> same counts | P8,PR5; E4 |
| P18a / AC50 `feat: cancel and reap one linux child session` | native Linux `async_session_state linux_cancel_reap` -> 1 cancelled, 1 reaped, 0 unrelated signals | P9,P12,P15; E6 |
| P18b / AC51 `feat: cancel and reap one macos child session` | native macOS `async_session_state macos_cancel_reap` -> same 3 counts | P10,P13,P16; E6 |
| P18c / AC52 `feat: cancel and reap one windows child session` | native Windows `async_session_state windows_cancel_reap` -> same 3 counts | P11,P14,P17; E6 |
| P19a / AC53 `feat: expose linux child session as typed pipe` | native Linux `async_process_pipe linux_child_payload` -> 1 request, 1 output payload, 1 exit, 0 helper threads | P18a; E6 |
| P19b / AC54 `feat: expose macos child session as typed pipe` | native macOS `async_process_pipe macos_child_payload` -> same 4 counts | P18b,P19a contract; E6 |
| P19c / AC55 `feat: expose windows child session as typed pipe` | native Windows `async_process_pipe windows_child_payload` -> same 4 counts | P18c,P19a contract; E6 |
| P20 / AC56 `build: close no_std process feature graph` | `cargo check -p proxima-process --no-default-features --features async-process --lib --message-format=json` -> 1 library artifact, 0 errors; normal tree -> 0 `std` edges | P19a-P19c; E2 |
| P21 / AC57 `test: reject blocking process substitutions` | `async_contract_audit` -> 4 negative controls rejected: blocking wait, read/write, `block_on`, child thread; 0 admitted | P20; E3-E5 |

## cargo-val cards

C0 at `57cb50f` is a help-only `std` scaffold. C0a-C0d replace its shipped
entry. `NO_COLOR` comes from H1-H3 and F3-F5, never `std::env`. A host test
harness may use `std`; normal executable dependencies may not.

| card / AC / commit | acceptance command and exact count | depends / evidence |
|---|---|---|
| C0 / AC58 `chore: scaffold cargo-val fixture workspace` | `cargo run -- --help` -> 1 help banner, 1 exit status 0 | `57cb50f`; E9; baseline smoke only |
| C0a / AC59 `build: make cargo-val core no_std` | `cargo check -p cargo-val --lib --no-default-features --message-format=json` -> 1 artifact, 0 errors, 0 normal `std` edges | P20,F9; E9 |
| C0b / AC60 `feat: enter cargo-val on linux without std` | native Linux `native_entry linux_help` -> 1 banner, 1 exit status 0 | C0a; E9 |
| C0c / AC61 `feat: enter cargo-val on macos without std` | native macOS `native_entry macos_help` -> same 2 counts | C0b contract; E9 |
| C0d / AC62 `feat: enter cargo-val on windows without std` | native Windows `native_entry windows_help` -> same 2 counts | C0b contract; E9 |
| C1 / AC63 `build: pin no_std dependencies` | `dependency_source pinned_revisions` -> 4 identities match pins; `cargo tree -p cargo-val --target all --no-default-features -e normal,features --prefix depth | python3 specs/cargo-val/assert_no_std_graph.py` -> 0 normal `std` edges across 3 target families | P21,F9,C0d; scan the real graph, not a fixture |
| C2 / AC64 `feat: decode one cargo artifact record` | `artifact_stream one_artifact` -> 1 executable emitted, 1 non-test artifact ignored | C1; E6 |
| C3 / AC65 `feat: emit cargo artifact before eof` | `artifact_stream two_before_eof` -> 2 ordered executables, first before EOF, 0 suite vectors | C2; E6 |
| C4 / AC66 `feat: parse one libtest listing line` | `discovery_stream one_line` -> 1 name emitted, 1 non-test line ignored | C3; E6 |
| C5 / AC67 `feat: dispatch first test before listing eof` | `discovery_stream dispatch_before_eof` -> 1 dispatch before EOF, 3 names later, 0 suite vectors | C4; E6 |
| C6 / AC68 `feat: await proxima permit before pulling intent` | `async_admission two_permits` -> 2 permits held, third intent unread, 0 dropped | C5; E7 |
| C7 / AC69 `feat: resume discovery after permit return` | `async_admission seven_two_slots` -> 7 intents completed once, peak 2 sessions, 0 lost | C6; E7 |
| C8 / AC70 `feat: run one exact test through child session` | `exact_process one_selected_test` -> 1 exact argv, 1 env value, 1 working-directory value, 1 child exit record | C1,C7; P19a-P19c; PR4 |
| C9 / AC71 `feat: isolate three exact test children` | `exact_process selected_tests_once` -> 3 names, 3 children, 0 duplicates | C8; P19a-P19c |
| C10 / AC72 `feat: stream stdout and stderr payloads` | `child_output stdout_stderr_payloads` -> 2 byte-identical streams, 0 blocking adapters | C8; P9-P14 |
| C11 / AC73 `feat: classify child exit separately` | `child_lifecycle pass_failure_abort` -> 1 pass, 1 assertion failure, 1 abnormal abort, 0 cross-test signals | C10; P15-P17; R6 |
| C12 / AC74 `feat: cancel and reap one active child` | `child_lifecycle cancel_one_child` -> 1 cancelled, 1 reaped, 0 orphan | C11; P18a-P18c |
| C13 / AC75 `feat: time out and reap one active child` | `child_lifecycle timeout_one_child` -> 1 timed out, 1 reaped, 0 orphan | C12; P18a-P18c |
| C14 / AC76 `feat: resolve runner settings from platform pairs` | `settings two_fields_from_pairs` -> 2 typed fields from a real platform environment pair iterator, 1 invalid capacity rejected, 0 direct `std::env` reads | C1,H1-H3,F9; E1; R7 |
| C14a / AC77 `feat: apply environment override to runner settings` | `settings env_override` -> 1 configured default, 1 environment override, 1 final value matching override | C14; F9,H1-H3; R7 |
| C15 / AC78 `feat: honor no_color presence in reporter` | `color_policy no_color_matrix` -> 2 colored when absent, 2 ANSI-free when present | C14; E1; each TTY/pipe |
| C16 / AC79 `feat: account every discovered outcome` | `accounting mixed_suite` -> 5 discovered = 1 filtered + 1 ignored + 2 passed + 1 failed; 0 unaccounted | C9-C15,C14a |
| C17 / AC80 `perf: retain per-child allocation trace` | native `allocation_trace child_payload_record` -> 1 raw allocation-site record, 1 exact payload per host | C10; instrumentation only |
| C18 / AC81 `perf: identify repeated windows allocation site` | Windows `allocation_trace repeated_site` -> 1 source-identified repeated site or 1 negative trace with 0 qualifying sites, 1 byte-identical payload control | C17; evidence gate for reuse |
| C18a / AC82 `perf: reuse the identified allocation owner` (conditional) | Windows `allocation_trace site_toggle` -> 1 byte-identical off/on pair, 1 site-specific before/after allocation count pair, 0 unrelated site changes | C18 positive trace; modify existing Proxima owner only |
| C19a / AC83 `ci: exercise linux lifecycle fixtures` | native Linux `platform_acceptance linux` -> 1 spawn/env/cwd, 1 stdout/stderr, 1 pass/fail/abort, 1 timeout, 1 cancellation; 0 orphans, 0 unaccounted | C16; five named categories |
| C19b / AC84 `ci: exercise macos lifecycle fixtures` | native macOS `platform_acceptance macos` -> same five category counts, 0 orphans, 0 unaccounted | C19a contract; C16 |
| C19c / AC85 `ci: exercise windows lifecycle fixtures` | native Windows `platform_acceptance windows` -> same five category counts, 0 orphans, 0 unaccounted | C19a contract; C16 |
| C20 / AC86 `test: reject malformed comparison records` | `comparison_record malformed_control` -> 1 malformed rejected, 0 valid rejected | C19a-C19c |
| C21 / AC87 `perf: retain paired nextest outcome payloads` | `comparison_record matched_outcomes` -> 1 paired raw record per host, 0 unexplained semantic mismatches | C19a-C19c,C20 |
| C22 / AC88 `perf: retain latency and throughput samples` | `comparison_record latency_throughput` -> 1 raw set with latency, throughput, CoV, conditions per host | C21 |
| C23 / AC89 `perf: retain cpu rss and error samples` | `comparison_record host_resources` -> 1 raw set with CPU, RSS, errors, conditions per host | C21 |
| C24 / AC90 `docs: show native cargo val commands` | `python3 specs/cargo-val/validate_command_examples.py` -> 3 native examples, 1 command spelling, 0 missing fixture links | C19a-C19c; documents R1 native acceptance invocations |

## independent oracle and incumbent control ledger

For each card ID below, the worker writes the named literal fixture **before**
implementation. The expected bytes/events are entered by hand from the
specified external contract or source hunk, never copied from candidate output.
`oracles/<id>.jsonl` is relative to `specs/cargo-val/` for cargo-val cards;
the same basename is copied into the owning repository's test fixtures for
upstream cards. A source probe may report its explicit unsupported record;
that is a falsification, not a passing implementation outcome. Existing
`std` process commands appear only in separate test/control programs and do
not enter the shipped runtime graph.

| card | independent literal oracle | incumbent or negative control |
|---|---|---|
| F0 | `oracles/f0.jsonl`: five `file:line` gaps, one borrowed-pair signature | reopen each pinned source hunk; current `Settings::from_env` behavior remains baseline |
| F1 | `oracles/f1.jsonl`: one no_std library symbol list | run the leaf's `core`-only API fixture and the deliberate `std::env` fixture through the same compile-check harness; require the incumbent fixture accepted and the std fixture rejected |
| F2 | `oracles/f2.jsonl`: `COLOR=always` present, `MISSING` absent | current conflaguration env lookup over the same two keys |
| F3 | `oracles/f3.jsonl`: absent, empty, nonempty values | current `std::env::var_os` control distinguishes all three states |
| F4 | `oracles/f4.jsonl`: `WORKERS=2`, `WORKERS=7`, `WORKERS=x` | current Settings scalar resolver receives the same UTF-8 values |
| F5 | `oracles/f5.jsonl`: invalid capacity maps to field `workers` | current Validate reports that field from the same value |
| F6 | `oracles/f6.jsonl`: `WORKERS=2`, `COLOR=always`, bad `WORKERS=x` | existing Settings derive resolves equivalent process env in test control |
| F7 | `oracles/f7.jsonl`: exact old display bytes | run old display fixture against pinned conflaguration revision |
| F8 | `oracles/f8.jsonl`: exact env override value | run old env fixture against pinned conflaguration revision |
| F8a | `oracles/f8a.jsonl`: exact file base value and overlay order | run old file fixture against pinned conflaguration revision |
| F9 | `oracles/f9.jsonl`: one core export, zero std symbols | run the existing `proxima-primitives` no_std artifact through the same export/feature-graph checker and require acceptance; compile a deliberate `std::env` import in no_std mode through that checker and require rejection |
| H1 | `oracles/h1.jsonl`: two literal Linux environment entries | compare borrowed bytes to libc `environ` in test control |
| H2 | `oracles/h2.jsonl`: two literal macOS environment entries | compare borrowed bytes to platform `environ` in test control |
| H3 | `oracles/h3.jsonl`: two literal wide Windows entries, including empty | compare units to Win32 environment block in test control |
| P0 | `oracles/p0.jsonl`: four missing source seams and one session signature | reread pinned Proxima hunks; old synchronous child fixture supplies output control |
| Q1 | `oracles/q1.jsonl`: Pending before release, one wake and one exit | child held behind an independent release pipe; closed source must not wake twice |
| Q2 | `oracles/q2.jsonl`: Pending before release, one wake and one exit | same barrier oracle on native macOS; closed source must not wake twice |
| Q3 | `oracles/q3.jsonl`: one pipe and one process completion after release | one non-overlapped negative handle must emit unsupported, not a fake completion |
| Q4 | `oracles/q4.jsonl`: fixed Linux child program, one handle and exit 0 | libc launch in the test harness supplies the child result control; missing no_std symbol emits unsupported |
| Q5 | `oracles/q5.jsonl`: fixed macOS child program, one handle and exit 0 | libc launch in the test harness supplies the child result control; missing no_std symbol emits unsupported |
| Q6 | `oracles/q6.jsonl`: fixed Windows child program, one handle and exit 0 | Win32 CreateProcess test control supplies the child result; missing no_std import emits unsupported |
| PR1 | `oracles/r1.jsonl`: register/wake/retire event order | run Prime's existing readiness registration fixture through the same lifecycle checker and require one register/wake/retire; stale-generation injection must deliver zero wakes |
| PR2 | `oracles/r2.jsonl`: pending/drain/wake sequence | run Prime's existing Linux socket-readiness fixture through the same readiness comparator and require one payload wake; barrier-before-release emits zero bytes and stale registration gets zero wakes |
| PR3 | `oracles/r3.jsonl`: pending/drain/wake sequence | run Prime's existing macOS socket-readiness fixture through the same readiness comparator and require one payload wake; barrier control emits zero premature bytes |
| PR4 | `oracles/r4.jsonl`: completion key and generation | run an existing Windows socket completion through the same completion comparator and require one wake; cancelled operation delivers zero later wakes |
| PR5 | `oracles/r5.jsonl`: child exit code and completion key | run an existing Windows completion registration through the same completion comparator and require one event; cancelled registration delivers zero later wakes |
| P1 | `oracles/p1.jsonl`: Windows accepted and Unix rejected target records | run existing Windows fixture and Unix target control through the same target-gate checker; Windows is accepted and Unix-only fixture is rejected |
| P2 | `oracles/p2.jsonl`: open, running, cancelled, reaped transitions | run the existing synchronous child command through the same lifecycle comparator and require open/running/reaped; reject an illegal second terminal transition |
| P3 | `oracles/p3.jsonl`: exit, cancel, error each terminate once | run the existing synchronous child status through the same terminal-event comparator and require one exit event; polling a terminated session twice emits zero extra events |
| P4 | `oracles/p4.jsonl`: pending then literal `out` payload | run the existing Proxima async socket-read fixture through the same poll comparator and require the literal payload; wake before source release emits zero bytes |
| P5 | `oracles/p5.jsonl`: first partial write, wake, final bytes | run the existing Proxima async socket-write fixture through the same poll comparator and require exact bytes; held sink remains pending without spinning |
| P6 | `oracles/p6.jsonl`: fixed Linux argv and status 0 | pinned synchronous Command runs same child in test control |
| P6a | `oracles/p6a.jsonl`: exact Linux child environment value | pinned synchronous Command sends same env pair in test control |
| P6b | `oracles/p6b.jsonl`: exact Linux child cwd and parent cwd | pinned synchronous Command runs same cwd child in test control |
| P7 | `oracles/p7.jsonl`: fixed macOS argv and status 0 | pinned synchronous Command runs same child in test control |
| P7a | `oracles/p7a.jsonl`: exact macOS child environment value | pinned synchronous Command sends same env pair in test control |
| P7b | `oracles/p7b.jsonl`: exact macOS child cwd and parent cwd | pinned synchronous Command runs same cwd child in test control |
| P8 | `oracles/p8.jsonl`: fixed Windows argv and status 0 | pinned synchronous Command runs same child in test control |
| P8a | `oracles/p8a.jsonl`: exact Windows child environment value | pinned synchronous Command sends same env pair in test control |
| P8b | `oracles/p8b.jsonl`: exact Windows child cwd and parent cwd | pinned synchronous Command runs same cwd child in test control |
| P9 | `oracles/p9.jsonl`: two literal stdout chunks and EOF | pinned synchronous child fixture yields the concatenated bytes |
| P10 | `oracles/p10.jsonl`: same payload with macOS chunk boundaries | pinned synchronous child fixture yields the concatenated bytes |
| P11 | `oracles/p11.jsonl`: same payload with Windows completion order | pinned synchronous child fixture yields the concatenated bytes |
| P12 | `oracles/p12.jsonl`: exact stdin bytes and child-observed EOF | pinned synchronous child fixture receives the same bytes |
| P13 | `oracles/p13.jsonl`: exact stdin bytes and child-observed EOF | same synchronous control on native macOS |
| P14 | `oracles/p14.jsonl`: exact stdin bytes and child-observed EOF | same synchronous control on native Windows |
| P15 | `oracles/p15.jsonl`: child exit code and one reap record | pinned synchronous wait reports same exit code; held child must not wake early |
| P16 | `oracles/p16.jsonl`: child exit code and one reap record | same control on native macOS |
| P17 | `oracles/p17.jsonl`: child exit code and one reap record | same control on native Windows |
| P18a | `oracles/p18a.jsonl`: Linux cancelled PID, reaped PID, live sibling | sibling output/exit identical under cancelled and uncancelled arms |
| P18b | `oracles/p18b.jsonl`: macOS cancelled PID, reaped PID, live sibling | same control on native macOS |
| P18c | `oracles/p18c.jsonl`: Windows cancelled PID, reaped PID, live sibling | same control on native Windows |
| P19a | `oracles/p19a.jsonl`: one Linux request/output/exit triplet | direct no_std session call yields identical payload without Pipe wrapper |
| P19b | `oracles/p19b.jsonl`: one macOS request/output/exit triplet | direct no_std session call yields identical payload |
| P19c | `oracles/p19c.jsonl`: one Windows request/output/exit triplet | direct no_std session call yields identical payload |
| P20 | `oracles/p20.jsonl`: expected normal dependency identities | run the pinned `proxima-primitives` no_std dependency graph through the same graph checker and require acceptance; enabling one `std` feature must make it reject |
| P21 | `oracles/p21.jsonl`: four forbidden call patterns | run the audited Proxima process-session source through the same call-pattern checker and require its allowed baseline accepted; inject each forbidden pattern and require all four rejected |
| C0 | `oracles/c0.jsonl`: exact checked-in help banner bytes | Cargo external-subcommand invocation reaches the same binary banner |
| C0a | `oracles/c0a.jsonl`: one no_std library artifact | run the pinned `proxima-primitives` no_std graph through the same graph checker and require acceptance; a deliberate normal `std` dependency must be rejected |
| C0b | `oracles/c0b.jsonl`: exact Linux help bytes and status 0 | old C0 help banner bytes are the output control |
| C0c | `oracles/c0c.jsonl`: exact macOS help bytes and status 0 | old C0 help banner bytes are the output control |
| C0d | `oracles/c0d.jsonl`: exact Windows help bytes and status 0 | old C0 help banner bytes are the output control |
| C1 | `oracles/c1.jsonl`: four pinned package IDs plus zero std features | run the pinned baseline graph through the same graph checker and require four identities accepted; inject one normal `std` edge and one mismatched pin and require each rejected |
| C2 | `oracles/c2.jsonl`: one Cargo compiler-artifact JSON line, one non-test line | Cargo's own JSON emitter supplies the field-shape control |
| C3 | `oracles/c3.jsonl`: two artifacts separated by a held-open producer barrier | run Cargo's own JSON emitter through the same streaming comparator and require its first artifact before EOF; held-open producer preserves first emission and reversed lines reject |
| C4 | `oracles/c4.jsonl`: one literal libtest name line and one header line | libtest `--list` on pinned fixture binary is the name control |
| C5 | `oracles/c5.jsonl`: first-name dispatch before a gated listing EOF | barrier release after observed dispatch; cargo-nextest runs same three-name outcome fixture |
| C6 | `oracles/c6.jsonl`: two permits and third intent still unread | run the existing Proxima semaphore permit path through the same admission comparator and require two admitted identities; no-permit control performs zero source reads |
| C7 | `oracles/c7.jsonl`: seven identity records and capacity two | cargo-nextest executes same seven fixture names; overflow arm loses zero identities |
| C8 | `oracles/c8.jsonl`: exact argv, env, cwd, exit tuple | direct Proxima child session runs same literal test fixture; nextest outcome agrees |
| C9 | `oracles/c9.jsonl`: three exact names and per-child marker IDs | cargo-nextest runs same selected names; nonselected marker remains absent |
| C10 | `oracles/c10.jsonl`: exact stdout and stderr byte arrays | direct Proxima session and nextest control yield same bytes |
| C11 | `oracles/c11.jsonl`: one pass, one assertion failure, one abort code | direct child exit records and nextest semantic outcome classify same three fixtures |
| C12 | `oracles/c12.jsonl`: cancelled PID and later reaped PID | direct Proxima session cancels same barrier child; sibling PID stays live |
| C13 | `oracles/c13.jsonl`: fake-clock deadline and one child PID | deadline-disabled arm completes same child; nextest outcome control retains fixture identity |
| C14 | `oracles/c14.jsonl`: two platform pairs, two typed values, one invalid capacity | conflaguration borrowed-pair API resolves same literal pairs directly |
| C14a | `oracles/c14a.jsonl`: default 2, env 3, result 3 | conflaguration direct call over the same platform pair yields 3 |
| C15 | `oracles/c15.jsonl`: four exact ANSI byte-presence records | direct conflaguration presence query supplies NO_COLOR state; nextest runs same output fixture |
| C16 | `oracles/c16.jsonl`: five hand-counted name/outcome records | cargo-nextest runs same five fixtures and passes the shared outcome comparator |
| C17 | `oracles/c17.jsonl`: one output payload and one child identity | run the pinned cargo-nextest child under the same host profiler and allocation-record comparator; require one raw allocation record for the matching PID and identical literal payload |
| C18 | `oracles/c18.jsonl`: one raw repeated allocation site or negative-site record | run the pinned cargo-nextest child under the same profiler and site comparator; require a matching PID-tagged allocation record or an explicit zero-site record, with byte-identical payload |
| C18a | `oracles/c18a.jsonl`: site ID, off/on count fields, exact payload bytes | off/on toggle changes only the traced site and retains bytes; unrelated-site change voids the comparison |
| C19a | `oracles/c19a.jsonl`: five named Linux classes and expected IDs | cargo-nextest runs same five classes on native Linux |
| C19b | `oracles/c19b.jsonl`: five named macOS classes and expected IDs | cargo-nextest runs same five classes on native macOS |
| C19c | `oracles/c19c.jsonl`: five named Windows classes and expected IDs | cargo-nextest runs same five classes on native Windows |
| C20 | `oracles/c20.jsonl`: one missing-required-field record | schema validator accepts one complete literal record and rejects malformed one |
| C21 | `oracles/c21.jsonl`: exact selected test IDs and outcomes | cargo-nextest and cargo-val each compared separately against literal records |
| C22 | `oracles/c22.jsonl`: sample field names, units, host/profile/corpus labels | raw timer samples independently recompute the reported latency/throughput/CoV; nextest arm uses same schema |
| C23 | `oracles/c23.jsonl`: CPU/RSS/error event field names and labels | raw host monitor samples independently recompute summaries; nextest arm uses same schema |
| C24 | `oracles/c24.jsonl`: three literal command invocations and fixture links | run the existing README command example through the same docs checker and require acceptance; check the three new examples and require each command and fixture link to name its matching native acceptance case |

## requirement coverage

| SPEC requirement | candidate cards |
|---|---|
| R1 native Windows/macOS/Linux | Q1-Q6, H1-H3, PR2-PR5, P6-P21, C0b-C0d, C19a-C19c; C0 is a help-only shell and does not discharge R1 |
| R2 Cargo artifact streaming | C2-C3 |
| R3 line discovery and early dispatch | C4-C5 |
| R4 exact isolated process, env, cwd, exit | P6-P8b, P15-P19c, C8-C9 |
| R5 bounded admission | C6-C7 |
| R6 output, pass/fail/abort, timeout, cancellation | P9-P19c, C10-C13 |
| R7 typed settings through conflaguration | F0-F9, H1-H3, C14-C14a |
| R8 default color and `NO_COLOR` | F3-F6, C15 |
| R9 complete outcome counts | C16 |
| R10 allocation payload evidence | C17-C18a |
| R11 nextest comparison evidence | C20-C23; C24 documents commands and does not discharge R11 |
| R12 async child lifecycle | P0, Q1-Q3, PR1-PR5, P2-P21, C6-C13 |
| R13 no `std` in shipped graph | F0-F9, P0, P20-P21, C0a-C1 |
| R14 native command documentation | C24 |

## resume

The current binary is C0 and imports `std` (E9). Proxima's current Windows
facade wraps `std::process` (E4); its Pipe still uses child worker threads
and `block_on` (E3). Neither discharges the new runtime requirement. Start
with F0, P0, and Q1-Q6; they bind the missing API seams and platform
feasibility. Do not start F1, PR1, P2, or any downstream implementation card
until the relevant source-backed gate has been reviewed and the card's work is
bounded. Before each platform card, bind its
fixture to literal expected events; retain raw events and account for both
success and failure. Never mark a prospective count as observed.
