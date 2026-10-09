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
| P1 | Proxima: `fix: gate unix process backend by target` | ACP1 | `cargo test -p proxima-process --test windows_target_gate -- unix_only_negative_control` | 1 Windows target fixture accepted, 1 Unix-only control rejected | [x] | `proxima` main `6cb786ea`; Windows cross-check passed; native Windows runtime unavailable |
| P2 | Proxima: `feat: add windows process spawn and wait` | ACP2 | `cargo test -p proxima-process --test windows_spawn -- spawn_wait_inherited_stdio` on Windows | 1 child spawned, 1 child reaped, 0 orphan children | [ ] | preserve the existing `Command` surface |
| P3 | Proxima: `feat: pipe windows child stdout` | ACP3 | `cargo test -p proxima-process --test windows_stdio -- stdout_bytes` on Windows | 1 stdout payload, 0 byte mismatches, 0 orphan children | [ ] | use the existing descriptor and stdio model |
| P4 | Proxima: `feat: pipe windows child stdin and stderr` | ACP4 | `cargo test -p proxima-process --test windows_stdio -- stdin_stderr_bytes` on Windows | 2 byte payloads, 0 byte mismatches, 0 orphan children | [ ] | one byte-exact fixture |
| P5 | Proxima: `fix: preserve windows command environment and cwd` | ACP5 | `cargo test -p proxima-process --test windows_command -- env_and_cwd` on Windows | 3 cases passed, 0 mismatches | [ ] | match Unix `CommandDescriptor` behavior |
| P6 | Proxima: `ci: check process backend on three hosts` | ACP6 | `cargo test -p proxima-process --test process_contract` in native Windows, macOS, and Linux jobs | each host: 7 process contract cases passed, 0 orphan children | [ ] | native execution required; cross-check is not runtime evidence |
| C0 | `chore: scaffold cargo-val fixture workspace` | AC0 | `cargo test -p cargo-val --test cli_smoke -- help_banner` | 1 fixture passed, 1 `cargo val --help` banner observed | [x] | Rust 2024; package `cargo-val`, binary `cargo-val` |
| C0a | `build: pin Proxima workspace dependencies` | AC0a | `cargo test -p cargo-val --test dependency_source -- pinned_proxima_revision` | 3 Proxima packages resolve from the same pinned Git revision, 1 mismatched-revision fixture rejected | [ ] | no developer-local path dependencies; depends on P6 on Proxima `main` |
| C1 | `feat: parse cargo artifacts as a stream` | AC1 | `cargo test -p cargo-val --test artifact_stream -- two_artifacts_before_build_exit` | 2 artifact records ordered, 1 handled before producer exit, 0 non-test launches | [ ] | bounded JSON-line parsing |
| C2 | `feat: parse one libtest listing incrementally` | AC2 | `cargo test -p cargo-val --test discovery_stream -- line_parser_counts` | 3 names parsed, 0 malformed names accepted | [ ] | parser fixture only |
| C3 | `feat: dispatch discovery before listing eof` | AC3 | `cargo test -p cargo-val --test discovery_stream -- dispatch_before_eof` | 1 test dispatched before EOF, 3 names discovered, 0 full-suite materializations | [ ] | first-launch ordering fixture |
| C4 | `feat: run an exact test through proxima process` | AC4 | `cargo test -p cargo-val --test exact_process -- selected_tests_once` | 3 names selected, 3 child invocations, 0 duplicate invocations | [ ] | no custom process abstraction |
| C5 | `feat: bound live tests with proxima demand` | AC5 | `cargo test -p cargo-val --test admission -- seven_tests_two_slots` | 7 completed, peak live children 2, 0 dropped intents | [ ] | no complete-suite pending queue |
| C6 | `feat: classify child exit outcomes` | AC6 | `cargo test -p cargo-val --test child_lifecycle -- pass_failure_abort` | 3 outcomes classified, 0 cross-test terminations | [ ] | process lifecycle state only |
| C7 | `feat: collect child output as bytes` | AC7a | `cargo test -p cargo-val --test child_output -- stdout_stderr_payloads` | 2 payloads retained byte-for-byte, 0 mismatches | [ ] | compose Proxima streams |
| C8 | `feat: load runner settings with conflaguration` | AC7 | `cargo test -p cargo-val --test settings -- config_env_cli_precedence` | 4 precedence cases, 0 invalid capacities | [ ] | settings and builder parity |
| C9 | `feat: color output unless no_color is present` | AC8 | `cargo test -p cargo-val --test color_policy -- no_color_matrix` | 4 cases: 2 ANSI outputs with `NO_COLOR` absent, 2 ANSI-free outputs when present; each pair covers TTY and pipe | [ ] | honor `NO_COLOR` presence |
| C10 | `feat: stop launching and reap children on cancel` | AC12 | `cargo test -p cargo-val --test cancellation -- cancel_two_live_children` | 2 children reaped, 0 launches after cancellation, 0 orphans | [ ] | cancellation lifecycle |
| C10a | `feat: terminate and reap timed-out children` | AC12a | `cargo test -p cargo-val --test timeout -- terminate_and_reap_child` | 1 timed-out child classified, 1 child reaped, 0 orphan children | [ ] | fixed-clock timeout fixture |
| C11 | `feat: account every discovered test outcome` | AC9 | `cargo test -p cargo-val --test accounting -- mixed_suite` | 5 discovered = 1 filtered + 1 ignored + 2 passed + 1 failed; 0 unaccounted | [ ] | accounting behavior only |
| C12 | `perf: retain per-child allocation records` | AC10 | `cargo test -p cargo-val --test allocation_trace -- child_payload_record` on each native host | each host: 1 raw per-child allocation record, 1 output payload | [ ] | measurement only; no performance claim |
| C13 | CONDITIONAL: `perf: reuse proven windows allocation owner` | AC13 | `cargo test -p cargo-val --test allocation_trace -- repeated_process_alloc_sites_and_arena_payload_parity` on Windows | 1 conditional arena parity case passed or 1 negative trace recorded and 0 arena changes admitted | [ ] | admit only from AC10 evidence; do not assume an arena |
| C14 | `ci: run cargo-val fixtures on windows mac linux` | AC14 | `cargo test -p cargo-val --test platform_acceptance` in native Windows, macOS, and Linux jobs | each host: 1 acceptance record covering 5 fixture categories, 0 unaccounted outcomes, 0 orphan children | [ ] | no cross-compilation substituted for runtime |
| C15 | `test: validate comparison record schema` | AC15 | `cargo test -p cargo-val --test comparison_record -- malformed_control` | 1 malformed record rejected, 0 valid host records rejected | [ ] | schema and degenerate control only |
| C16 | `perf: record matched test outcome cells` | AC11 | `cargo test -p cargo-val --test comparison_record -- matched_host_outcomes` | each native host: 1 paired cargo-val/cargo-nextest record with identical fixture outcome vectors | [ ] | raw per-test traces and pair identity |
| C17 | `perf: record host latency and throughput samples` | AC11a | `cargo test -p cargo-val --test comparison_record -- latency_throughput_samples` | each native host: 1 record contains throughput, single-request and percentile latency, raw samples, and CoV | [ ] | timing fields only |
| C18 | `perf: record host resource and error samples` | AC11b | `cargo test -p cargo-val --test comparison_record -- host_resource_samples` | each native host: 1 record contains CPU, RSS, error rate, corpus, profile, baseline, and host identity | [ ] | resource fields only |
| C19 | `docs: describe cargo-val command and supported behavior` | AC16 | `cargo test -p cargo-val --test docs_contract -- command_examples` | 3 platform examples, 1 command spelling, 0 undocumented platform caveats | [ ] | document `cargo val` and `NO_COLOR` |

## resume

Last landed slices: P1 on Proxima main (`6cb786ea`), C0 on cargo-val main
Next action: P2 adds Windows spawn and wait to `proxima-process`; C0a waits for P6
Open question, if any: none; C3's dispatch-before-EOF fixture is the explicit
gate that falsifies the incremental discovery requirement

## struck

-
