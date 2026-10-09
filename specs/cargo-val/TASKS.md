# cargo-val -- slices

Each card is one behavior change, one conventional commit, and one validation
command. Luna execution scope is capped at 20 minutes per card; split any card
that cannot meet that bound before implementation. Every commit must have its
own green acceptance command, update this ledger in the same commit, land on
`main`, and be pushed after validation.

For Proxima prerequisite cards, the commit and validation occur in the Proxima
repository; cargo-val integration cards depend on the corresponding Proxima
commit being present on `main`.

| # | card / coherent commit | discharges | validation command | expected | done | note |
|---|---|---|---|---|---|---|
| P1 | Proxima: `fix: gate unix process backend by target` | R1 | `cargo check -p proxima-process --target x86_64-pc-windows-msvc --no-default-features --message-format=json` | 1 library artifact, 0 compiler errors | [ ] | separate target-gating from backend implementation |
| P2 | Proxima: `feat: add windows process spawn and wait` | R1 | `cargo test -p proxima-process --test windows_spawn -- spawn_wait_inherited_stdio` on Windows | 1 child spawned, 1 child reaped, 0 orphan children | [ ] | preserve the existing `Command` surface |
| P3 | Proxima: `feat: pipe windows child stdout` | R1, R6 | `cargo test -p proxima-process --test windows_stdio -- stdout_bytes` on Windows | 1 stdout payload, 0 byte mismatches, 0 orphan children | [ ] | use the existing descriptor and stdio model |
| P4 | Proxima: `feat: pipe windows child stdin and stderr` | R1, R6 | `cargo test -p proxima-process --test windows_stdio -- stdin_stderr_bytes` on Windows | 2 byte payloads, 0 byte mismatches, 0 orphan children | [ ] | one byte-exact fixture |
| P5 | Proxima: `fix: preserve windows command environment and cwd` | R1, R4 | `cargo test -p proxima-process --test windows_command -- env_and_cwd` on Windows | 3 cases passed, 0 mismatches | [ ] | match Unix `CommandDescriptor` behavior |
| P6 | Proxima: `ci: check process backend on three hosts` | R1 | `cargo test -p proxima-process --test process_contract` in native Windows, macOS, and Linux jobs | each host: 7 process contract cases passed, 0 orphan children | [ ] | native execution required; cross-check is not runtime evidence |
| C0 | `chore: scaffold cargo-val fixture workspace` | R1 | `cargo test -p cargo-val --test cli_smoke -- help_banner` | 1 fixture passed, 1 `cargo val --help` banner observed | [ ] | Rust 2024; package `cargo-val`, binary `cargo-val` |
| C0a | `build: pin Proxima workspace dependencies` | R1, R4, R5 | `cargo tree -p cargo-val -e normal --prefix none` | 1 `proxima-process`, 1 `proxima-primitives`, 1 `proxima-runtime`, all from the same pinned Git revision | [ ] | no developer-local path dependencies; blocked on P6 landing on Proxima `main` |
| C1 | `feat: parse cargo artifacts as a stream` | R2 | `cargo test -p cargo-val --test artifact_stream -- two_artifacts_before_build_exit` | 2 artifact records ordered, 1 handled before producer exit, 0 non-test launches | [ ] | bounded JSON-line parsing |
| C2 | `feat: parse one libtest listing incrementally` | R3 | `cargo test -p cargo-val --test discovery_stream -- line_parser_counts` | 3 names parsed, 0 malformed names accepted | [ ] | parser fixture only |
| C3 | `feat: dispatch discovery before listing eof` | R3 | `cargo test -p cargo-val --test discovery_stream -- dispatch_before_eof` | 1 test dispatched before EOF, 0 full-suite materializations | [ ] | first-launch ordering fixture |
| C4 | `feat: run an exact test through proxima process` | R1, R4 | `cargo test -p cargo-val --test exact_process -- selected_tests_once` | 3 names selected, 3 child invocations, 0 duplicate invocations | [ ] | no custom process abstraction |
| C5 | `feat: bound live tests with proxima demand` | R5 | `cargo test -p cargo-val --test admission -- seven_tests_two_slots` | 7 completed, peak live children 2, 0 dropped intents | [ ] | no complete-suite pending queue |
| C6 | `feat: classify child exit outcomes` | R6 | `cargo test -p cargo-val --test child_lifecycle -- pass_failure_abort` | 3 outcomes classified, 0 cross-test terminations | [ ] | process lifecycle state only |
| C7 | `feat: collect child output as bytes` | R6 | `cargo test -p cargo-val --test child_output -- stdout_stderr_payloads` | 2 payloads retained byte-for-byte, 0 mismatches | [ ] | compose Proxima streams |
| C8 | `feat: load runner settings with conflaguration` | R7 | `cargo test -p cargo-val --test settings -- config_env_cli_precedence` | 4 precedence cases, 0 invalid capacities | [ ] | settings and builder parity |
| C9 | `feat: color output unless no_color is present` | R8 | `cargo test -p cargo-val --test color_policy -- no_color_matrix` | 4 cases: 2 ANSI outputs with `NO_COLOR` absent, 2 ANSI-free outputs when present; each pair covers TTY and pipe | [ ] | honor `NO_COLOR` presence |
| C10 | `feat: stop launching and reap children on cancel` | R6 | `cargo test -p cargo-val --test cancellation -- cancel_two_live_children` | 2 children reaped, 0 launches after cancellation, 0 orphans | [ ] | timeout fixture remains in C6 acceptance |
| C11 | `feat: account every discovered test outcome` | R9 | `cargo test -p cargo-val --test accounting -- mixed_suite` | 5 discovered = 1 filtered + 1 ignored + 2 passed + 1 failed; 0 unaccounted | [ ] | accounting behavior only |
| C12 | `perf: retain per-child allocation records` | R10 | `cargo test -p cargo-val --test allocation_trace -- child_payload_record` on each native host | each host: 1 raw per-child allocation record, 1 output payload | [ ] | measurement only; no performance claim |
| C13 | CONDITIONAL: `perf: reuse proven windows allocation owner` | R10 | `cargo test -p cargo-val --test allocation_trace -- repeated_process_alloc_sites` followed by `cargo test -p cargo-val --test allocation_trace -- arena_payload_parity` on Windows | if repeated sites are traced and no existing Proxima owner fits: site-count change recorded and 1 payload remains byte-identical; otherwise strike this slice with the negative trace | [ ] | admit only from C12 evidence; do not assume an arena |
| C14 | `ci: run cargo-val fixtures on windows mac linux` | R1, R4, R6 | `cargo test -p cargo-val --test platform_acceptance` in native Windows, macOS, and Linux jobs | each host: 1 acceptance record covering 5 fixture categories, 0 unaccounted outcomes, 0 orphan children | [ ] | no cross-compilation substituted for runtime |
| C15 | `perf: record matched nextest comparison cells` | R11 | `cargo test -p cargo-val --test comparison_record -- complete_host_record` | 3 complete host records; malformed control rejected 1 time | [ ] | raw per-test traces + throughput, single-request and percentile latency, CPU, RSS, errors, CoV, corpus, profile, baseline |
| C16 | `docs: describe cargo-val command and supported behavior` | R1-R9 | `cargo test -p cargo-val --test docs_contract -- command_examples` | 3 platform examples, 1 command spelling, 0 undocumented platform caveats | [ ] | document `cargo val` and `NO_COLOR` |

## resume

Last landed slice: none
Next action: land P1 in Proxima so the Windows dependency edge is tested before
the cargo-val process adapter is implemented
Open question, if any: none; C3's dispatch-before-EOF fixture is the explicit
gate that falsifies the incremental discovery requirement

## struck

-
