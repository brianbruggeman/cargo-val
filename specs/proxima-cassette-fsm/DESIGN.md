# Proxima cassette invalidation: sans-IO FSM driven through pipes

- date: 2026-10-11
- target checkout: /Users/brianbruggeman/repos/slot-0/proxima-prime-windows-io at HEAD 8417f133
- status: design, not implemented. Owner rule: cassette invalidation is a sans-IO FSM driven through pipes.
- author: proxima-architect (read-only pass); recorded verbatim from its report.
- owner decision 2026-10-11: a missing cassette BUILDS a new one; it never fails by default. Default mode = `record-missing`. Failing on a missing cassette or a miss (`strict`) is an opt-in conflaguration toggle (`PROXIMA_CASSETTE_MODE=strict`, config file, or builder), e.g. for CI. This supersedes the architect's strict default everywhere below.
- open owner decision: remove the legacy `PROXIMA_CASSETTE` alias.
- owner decision 2026-10-11: cassette LOCATION is a conflaguration setting. Default store = cargo target dir (a cassette is an optimization / cache); a developer may choose to keep it next to the test (a committed fixture). See "Cassette location" below.

## Cassette location (owner decision; edge concern, not FSM)

- Owner 2026-10-11: "custom folder, next to test, custom folder can include test hierarchy or flat as a toggle".
- Config: `store: CassetteStore { Target, Source, Custom }` (`PROXIMA_CASSETTE_STORE=target|source|custom`, default `target`); `custom_dir: Option<PathBuf>` (`PROXIMA_CASSETTE_DIR`), required when `store = custom` (Validate error otherwise; a relative path resolves against the workspace root); `layout: CassetteLayout { Hierarchy, Flat }` (`PROXIMA_CASSETTE_LAYOUT=hierarchy|flat`, default `hierarchy`), which applies to the custom folder. Same `CassetteConfig`, builder/config parity.
- Paths, where `<mod>` is the test's module path without the crate name (from `module_path!()` emitted by `#[proxima::test]` into `CassetteSpec`, a new field — coordinate with the in-flight proxima-macros scheduling edit; S6 depends on it):
  - `Target`: `<target-dir>/cassettes/<crate>/<mod as dirs>/<name>[__<case>].jsonl`
  - `Source` (next to the test): `<CARGO_MANIFEST_DIR>/tests/cassettes/<name>[__<case>].jsonl` (today's scheme, proxima-test/src/harness.rs:44-54)
  - `Custom` + `Hierarchy`: `<custom_dir>/<crate>/<mod as dirs>/<name>[__<case>].jsonl` (mirrors the test hierarchy)
  - `Custom` + `Flat`: `<custom_dir>/<crate>__<mod with :: as __>__<name>[__<case>].jsonl` (one directory, no subdirectories; crate and module prefix keep names unique)
- Target-dir discovery at test runtime (the harness edge): `CARGO_TARGET_DIR` if set; else derived from `std::env::current_exe()` = `<target>/<profile>/deps/<bin>` (walk up past `deps` and the profile dir). `CARGO_TARGET_TMPDIR` is not used: cargo provides it only to integration tests and benches, not unit tests.
- Lookup rule: `store` decides only where NEW recordings are written. Reads probe, in order: source (committed next to the test), custom folder (if `custom_dir` is configured, whatever the store, using the configured layout), target cache.
- Precedence when several exist (owner delegated the choice 2026-10-11): source > custom > target, i.e. most deliberate and durable first. Source is committed and reviewed, versioned with the code, and what CI sees; custom was configured on purpose; target is a disposable machine-local cache (`cargo clean` deletes it). The configured `store` is deliberately NOT read first: with the default `store = target`, store-first reads would let a stale local cache shadow a committed fixture.
- Shadowing report: when more than one copy exists, the edge compares their content digests (whole-file, computed at probe time). Identical copies: silent. Differing copies: one `proxima::telemetry::warn!` naming the copy used and every shadowed path. Tests: {2 copies identical, 2 differing, 3 differing} → warn count 0, 1, 1 with the right paths. A committed fixture is never shadowed by a stale local cache. The edge turns the first hit into `Presence::Present` (with that path) or `Presence::Absent`; the FSM never sees paths.
- Rebuild/backup writes go to the location the cassette was READ from (a committed fixture is rebuilt in place under `source`; under `target` a committed fixture is never modified — the rebuilt copy is written to the target cache and the source one is left untouched, reported as shadowed-by-rebuild in the warn event).
- Trade-off recorded: with the `target` default a fresh CI checkout has no cassettes, so record-missing calls the live upstreams. Hermetic CI = commit cassettes with `store = source` and set `mode = strict`.
- Tests (added to S6): parity fixture covers `store`/`custom_dir`/`layout`; `store = custom` without `custom_dir` is a Validate error; resolution table — {source, custom, target} presence (8 combinations) × {store target, store source, store custom+hierarchy, store custom+flat} → read path and write path asserted; hierarchy mirrors a nested module path (`a::b::c` → `a/b/c/`); flat names for two crates and two modules with the same cassette name do not collide; target-dir derivation from a `current_exe`-shaped path for debug/release/custom profile and with `CARGO_TARGET_DIR` set.

## Revisions after review (2026-10-11, binding on S2+)

- S1 landed: keying.rs new items + tests/keying_alloc.rs; 12/12 keying tests, 0 allocs over 100,000 `request_key`, crate 113/113. The new key/digest newtypes live in keying.rs; fsm.rs imports them.
- R1 collision honesty: the claim "never served silently" was not enforced (lookup takes only `KeyDigest`, `IndexEntry` stores no request identity). Fix: `IndexEntry` and the stamped `cassette-entry` carry a second, independent `RequestDigest` (full normalized request, different fold seed); a lookup hit whose `RequestDigest` differs is `Refusal::KeyCollision { key }`, never served. One field, no new type beyond the digest newtype.
- R2 type consolidation: `Expectation` and `CassetteHeader` are one type (`CassetteHeader`, used both as the expectation and the observed header). `Rebuild { cause, fresh, ready }` is one struct `Rebuild` reused by `Opened`, `HeaderOutcome`, `LoadOutcome` variants. `LineNo` stays only if a call site would otherwise confuse it with another `u32`; else plain `u32`. Each remaining newtype must have a call site where a bare integer would be wrong (`Millis` vs `UnixMillis` qualifies).
- R3 no runtime state flags: `RecordLedger`'s `SessionState { Unopened, Open }` becomes typestate — `RecordLedger<Unopened>` whose first admit returns `(RecordLedger<Open>, AppendDecision::Append(Now(stamp)))`; `OpenSegment` disappears.
- R4 lock-free record path: the ledger has a single owner (the recorder pipe's driving task / a single-consumer pipe), not `AsyncMutex`; record-path requests hand exchanges to it without serializing the request path.
- R5 revised Load table, 15 rows (design's 12 + 3). The added rows, each a test in fsm.rs: (a) `Entry` whose key is already indexed but whose `RequestDigest` differs → `Refuse(KeyCollision { key })` (R1 enforced at load; `load_key_collision_with_different_request_digest_refuses`); (b) `End`, RecordMissing, no `Segment` seen → `Ready(Record(Missing, session = 0))` (`load_end_with_no_segment_records_into_session_zero`); (c) `End`, RecordMissing, newest = `u32::MAX` → `Refuse(Unreadable(SegmentsExhausted { newest }))`, since newest+1 would overflow (`load_end_after_the_last_segment_id_refuses_exhausted`). Revised S2 count: setup 8 + header 11 + load 15 + duplicates 7 = 41; S3 cumulative becomes 61 (41 + 7 + 6 + 4 + 3).
- R6 serving rows, 25 not 20 (S3 cumulative becomes 66 = 41 + 25). Five rows beyond the original 7 + 6 + 4 + 3, each a test in fsm.rs, none removable: (a) `lookup_key_hit_with_a_different_request_digest_refuses_key_collision`, (b) `route_record_missing_key_collision_refuses_instead_of_serving`, (c) `route_verify_key_collision_refuses_instead_of_comparing` enforce R1 on the serving side (a hit whose `RequestDigest` differs is `Refusal::KeyCollision`, never served and never compared); the original table counted R1 only at load. (d) `ledger_unopened_skip_hands_back_the_unopened_ledger` and (e) `ledger_unopened_refusal_hands_back_the_unopened_ledger` enforce R3 typestate: `RecordLedger<Unopened>::admit` returns `Admission::{Opened, Skipped(ledger), Refused{refusal, ledger}}`, so a skip or refusal on the first request returns the still-unopened ledger instead of consuming it. Serving breakdown: lookup 8 (7 + a) + route 8 (6 + b, c) + verify 3 + ledger 6 (4 + d, e) = 25. S3 AC becomes `fsm::` = 66 passed.

## The number that hurts first

`tests/cassettes/weather.jsonl` as it stands has no `cassette-meta` line and no key stamp (lines 1-4). The rule says a version mismatch means rebuild, never a guessed replay. Under that rule this file cannot be replayed. In strict mode (the proposed default) `weather_cassette_round_trips` (tests/e2e/proxima_test_smoke.rs:350-367) gets `Refuse(Version(Unversioned))`. `sugar.jsonl` is in the same position. The slice that turns the loader on therefore also re-records both fixtures (S6).

## The shape

- The FSM is a set of state structs. Each owns only its legal data. Each transition consumes `self` and returns an outcome enum; the variant of that enum is the decision. No top-level enum where a state can receive the wrong event, so no runtime "am I in the right state" checks.
- The FSM is tier-3 (no_std, no alloc, all `Copy`). Anything that grows with the cassette lives in caller-owned storage: `IndexBuilder<R>`, then `CassetteIndex<R>`, plus a `RecordLedger` for the recording session (tier-1, `alloc`).
- The edges do all the IO. The harness reads env, config and the clock. The loader turns bytes into events. The recorder and verifier are observe pipes (`Exchange -> Exchange`).
- The per-request decision path does not allocate: key fold, binary search, age arithmetic.

### Contested decision: where the key comes from at load

Chosen: the recorder writes the key digest into the cassette and the loader reads it back; the loader never recomputes it from the recorded request.

Why recomputing was rejected: `RequestHeader.query` and `.headers` are `BTreeMap<String,String>` (proxima-recording/src/event.rs:103-110). The tee writes `header_map(&request.query)` (src/test_support.rs:463-473,515), so `?tag=a&tag=b` reaches the file as a single pair. Today's load-time key (`match_key_from_recording`, keying.rs:92-100) is computed from that lossy copy, which makes it a guess. Cost: hand-editing a request line no longer changes its key; changing a request means re-recording.

Raw (non-cassette) recordings replayed by `ReplayUpstream` for proxima-intercept and examples keep load-time derivation, only as an explicit loader input (`KeySource::Derived`), never as a fallback inside the FSM.

### Composition finding at this rev

`PipeExt::and_then`/`filter` are defined only on root `Pipe` (proxima-primitives/src/pipe/ext.rs:45,87). Every cassette stage must be a `SendPipe` because `into_handle` requires `SendPipe<Request,Response,ProximaError>` (handler.rs:113-118). So stages compose through `AndThen::new` (primitives.rs:189-200; its `SendPipe` impl needs both stages `SendPipe`). No `SendPipeExt` exists. `filter` is not used: its rejection aborts the chain with `Err`, which would turn `SkipIdentical` into a failure.

## 1. The FSM — `proxima-recording/src/replay/fsm.rs` (no_std, no alloc)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)] pub struct KeyDigest(u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct ResponseDigest(u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)] pub struct SegmentId(u32);
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct SlotId(u32);            // fields private: minted only by CassetteIndex
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)] pub struct UnixMillis(u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)] pub struct Millis(u64);
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct FormatVersion(u32);     // == RECORDING_FORMAT_VERSION (event.rs:11)
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct KeySchemaVersion(u16);  // KEY_SCHEMA_VERSION = 1
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct KeySpecDigest(u64);     // digest of the canonical header allowlist
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct UpstreamDigest(u64);    // digest of Option<upstream identity>
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct LineNo(u32);

#[non_exhaustive] #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "kebab-case")]
pub enum CassetteMode { Strict, RecordMissing, Verify, Rerecord }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "kebab-case")]
pub enum Disposition { Truncate, Backup }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "kebab-case")]
pub enum DuplicatePolicy { RejectDivergent, LastWins, FirstWins }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "kebab-case")]
pub enum OnStale { Fail, Warn }
pub enum Staleness { Unbounded, MaxAge { max_age: Millis, on_stale: OnStale } }

pub struct Expectation { pub format: FormatVersion, pub key_schema: KeySchemaVersion, pub key_spec: KeySpecDigest, pub upstream: UpstreamDigest }
pub struct Policy { pub mode: CassetteMode, pub duplicates: DuplicatePolicy, pub staleness: Staleness,
                    pub replace: Disposition, pub expect: Expectation, pub now: UnixMillis }   // `now` is read by the edge, once

// events
pub enum Presence { Absent, Present }
pub struct CassetteHeader { pub format: FormatVersion, pub key_schema: KeySchemaVersion, pub key_spec: KeySpecDigest, pub upstream: UpstreamDigest }
pub enum RecordedAt { At(UnixMillis), Unknown }
pub struct SegmentStamp { pub segment: SegmentId, pub recorded_at: RecordedAt }
pub struct LoadedEntry { pub key: KeyDigest, pub segment: SegmentId, pub response: ResponseDigest }
pub enum LineFault { Malformed { line: LineNo }, FormatVersion { line: LineNo, found: FormatVersion },
                     SegmentOrder { line: LineNo, segment: SegmentId }, UnknownSegment { line: LineNo, segment: SegmentId },
                     DanglingInteraction { line: LineNo } }
pub enum HeaderEvent { Header(CassetteHeader), Headerless, Fault(LineFault) }
pub enum BodyEvent<R> { Segment(SegmentStamp), Entry(LoadedEntry, R), Fault(LineFault), End }

// refusals (typed, Copy)
pub enum Mismatch { Unversioned, Format { expected: FormatVersion, found: FormatVersion },
                    KeySchema { expected: KeySchemaVersion, found: KeySchemaVersion },
                    KeySpec { expected: KeySpecDigest, found: KeySpecDigest }, Upstream { expected: UpstreamDigest, found: UpstreamDigest } }
pub enum Age { Known(Millis), Unknown }
pub enum Refusal { MissingCassette, Version(Mismatch), Unreadable(LineFault), Miss { key: KeyDigest },
                   Stale { key: KeyDigest, age: Age, max_age: Millis },
                   DivergentDuplicate { key: KeyDigest, segment: SegmentId, prior: ResponseDigest, incoming: ResponseDigest } }

// states — each owns only its legal data
pub struct AwaitingPresence { policy: Policy }
pub struct ExpectingHeader  { policy: Policy }
pub struct Loading          { policy: Policy, newest: SegmentId }
pub struct ReplayServing    { staleness: Staleness, now: UnixMillis }                       // strict + raw replay
pub enum   RecordScope      { Missing, All }
pub struct RecordServing    { scope: RecordScope, staleness: Staleness, now: UnixMillis, session: SegmentId, duplicates: DuplicatePolicy }
pub struct VerifyServing    { staleness: Staleness, now: UnixMillis }
pub enum   Ready            { Replay(ReplayServing), Record(RecordServing), Verify(VerifyServing) }
pub enum   FreshStart       { Create, Replace(Disposition) }

// outcomes: the variant IS the decision the edge executes
#[must_use] pub enum Opened        { ReadCassette(ExpectingHeader), StartFresh { fresh: FreshStart, ready: RecordServing }, Refuse(Refusal) }
#[must_use] pub enum HeaderOutcome { Accepted(Loading), Rebuild { cause: Mismatch, fresh: FreshStart, ready: RecordServing }, Refuse(Refusal) }
#[must_use] pub enum LoadOutcome   { Continue(Loading), Ready(Ready), Rebuild { cause: Mismatch, fresh: FreshStart, ready: RecordServing }, Refuse(Refusal) }

impl AwaitingPresence { #[must_use] pub const fn new(policy: Policy) -> Self; pub fn on_presence(self, presence: Presence) -> Opened; }
impl ExpectingHeader  { pub fn on_header(self, event: HeaderEvent) -> HeaderOutcome; }
impl Loading          { pub fn on_event<R>(self, event: BodyEvent<R>, builder: &mut IndexBuilder<R>) -> LoadOutcome; }

// per-request reads: no transition, so &self; 0 allocs
pub enum Freshness { Fresh, Expired { age: Age, max_age: Millis } }
pub struct Hit { pub slot: SlotId, pub freshness: Freshness }
pub enum Prior { Unrecorded, Recorded { slot: SlotId, segment: SegmentId, response: ResponseDigest, freshness: Freshness } }
pub enum LiveRoute { Serve(SlotId), CallLive(Prior) }
pub enum VerifyFinding { Matches { key: KeyDigest, freshness: Freshness }, Unrecorded { key: KeyDigest },
                         Diverged { key: KeyDigest, recorded: ResponseDigest, live: ResponseDigest, freshness: Freshness } }
impl ReplayServing { #[must_use] pub fn lookup<R>(&self, index: &CassetteIndex<R>, key: KeyDigest) -> Result<Hit, Refusal>; }
impl RecordServing { #[must_use] pub fn route<R>(&self, index: &CassetteIndex<R>, key: KeyDigest) -> LiveRoute;
                     #[must_use] pub fn open_ledger(&self) -> RecordLedger; }
impl VerifyServing { #[must_use] pub fn route<R>(&self, index: &CassetteIndex<R>, key: KeyDigest) -> Prior;
                     #[must_use] pub fn verify(&self, key: KeyDigest, prior: Prior, live: ResponseDigest) -> VerifyFinding; }

// one duplicate rule, used both at load and at record
pub enum Resolution { Insert, Supersede, Replace, KeepPrior, Refuse(Refusal) }
impl DuplicatePolicy { #[must_use] pub fn resolve(self, key: KeyDigest, prior: Option<(SegmentId, ResponseDigest)>,
                                                  incoming: (SegmentId, ResponseDigest)) -> Resolution; }
```

Caller-owned storage, `replay/index.rs` (alloc):

```rust
pub struct IndexEntry<R> { pub key: KeyDigest, pub segment: SegmentId, pub response: ResponseDigest, pub payload: R }
pub struct IndexBuilder<R> { entries: BTreeMap<KeyDigest, IndexEntry<R>>, segments: Vec<SegmentStamp> }   // load path only
pub struct CassetteIndex<R> { entries: Box<[IndexEntry<R>]>, segments: Box<[SegmentStamp]> }           // sorted by key; frozen
impl<R> IndexBuilder<R> { pub fn new() -> Self; #[must_use] pub fn seal(self) -> CassetteIndex<R>; }
impl<R> CassetteIndex<R> { #[must_use] pub fn empty() -> Self; #[must_use] pub fn get(&self, slot: SlotId) -> &IndexEntry<R>; pub fn len(&self) -> usize; }

pub enum OpenSegment { Already, Now(SegmentStamp) }
pub enum AppendDecision { Append(OpenSegment), SkipIdentical, Refuse(Refusal) }
pub enum SessionState { Unopened, Open }
pub struct RecordLedger { session: SegmentId, state: SessionState, appended: BTreeMap<KeyDigest, ResponseDigest>, duplicates: DuplicatePolicy, now: UnixMillis }
impl RecordLedger { pub fn admit(&mut self, key: KeyDigest, prior: Prior, response: ResponseDigest) -> AppendDecision; }
```

Keying, `replay/keying.rs` (alloc, sans-IO):

```rust
pub const KEY_SCHEMA_VERSION: KeySchemaVersion = KeySchemaVersion(1);
pub struct HeaderAllowlist(Vec<String>);   // lowercase, sorted, deduped by construction
impl HeaderAllowlist { pub fn new<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Self, ProximaError>;
                       #[must_use] pub fn digest(&self) -> KeySpecDigest; }
#[must_use] pub fn request_key(request: &Request<Bytes>, allowlist: &HeaderAllowlist) -> KeyDigest;   // 0 allocs for <=16 query pairs
#[must_use] pub fn response_digest(status: u16, body: &[&[u8]]) -> ResponseDigest;                    // same fold as test_support.rs:570
#[must_use] pub fn render_request_line(request: &Request<Bytes>) -> String;                           // cold path only (miss text, labels)
```

- Key fold (v1): FNV-1a-64 reusing `fnv1a64_fold` (keying.rs:44-50). Input: `"pxk1"`, `METHOD 0x00`, `path 0x00`, each query pair in `(name,value)` byte order as `name 0x01 value 0x02`, then `0x00`; then each allowlisted header in allowlist order, values in request order, as `lname 0x01 value 0x02`; then `0x00` and the body digest as 8 LE bytes.
- Separators: today's `a=b&c` text key (keying.rs:102-131) is ambiguous when values contain `&` or `=`; separator bytes remove that.
- Collisions become a load-time `DivergentDuplicate` or a dedupe; never served silently.
- `HeaderAllowlist` passes the capability test: `["Accept","accept"]` and `["accept"]` give the same `KeySpecDigest`; a bare `&[String]` depends on caller order and case.

### Transition tables (58 rows; each row is one test in S2/S3)

Setup — `AwaitingPresence::on_presence` (8 rows)

| mode | Absent | Present |
|---|---|---|
| Strict | `Refuse(MissingCassette)` | `ReadCassette` |
| Verify | `Refuse(MissingCassette)` | `ReadCassette` |
| RecordMissing | `StartFresh{Create, Record(Missing, session 0)}` | `ReadCassette` |
| Rerecord | `StartFresh{Create, Record(All, session 0)}` | `StartFresh{Replace(policy.replace), Record(All, 0)}` (file never read) |

Header — `ExpectingHeader::on_header` (11 rows). Fields compared in order format, key_schema, key_spec, upstream; first difference reported.

| event | Strict | Verify | RecordMissing |
|---|---|---|---|
| Header, all fields equal | `Accepted(Loading{newest: none})` | same | same |
| Header, a field differs (m) | `Refuse(Version(m))` | `Refuse(Version(m))` | `Rebuild{m, Replace(replace)}` |
| Headerless | `Refuse(Version(Unversioned))` | same | `Rebuild{Unversioned}` |
| `Fault(FormatVersion{found})` | `Refuse(Version(Format))` | same | `Rebuild{Format}` |
| `Fault(other)` | `Refuse(Unreadable)` | same | same (corruption never auto-destroys) |

Load — `Loading::on_event` (12 rows; 15 with the R5 additions)
- `Segment(s)` with s > newest → `Continue`; s not greater → `Refuse(Unreadable(SegmentOrder))`
- `Entry` naming an undeclared segment → `Refuse(Unreadable(UnknownSegment))`
- `Entry` → `DuplicatePolicy::resolve`: Insert/Supersede/Replace/KeepPrior → `Continue`; `Refuse` → `Refuse(DivergentDuplicate)`
- `Fault(FormatVersion)`: Strict and Verify → `Refuse(Version(Format))`; RecordMissing → `Rebuild` (3 rows)
- `Fault(other)` → `Refuse(Unreadable)`
- `End`: Strict → `Ready(Replay)`; Verify → `Ready(Verify)`; RecordMissing → `Ready(Record(Missing, session = newest+1))` (3 rows)

Duplicates — `DuplicatePolicy::resolve` (7 rows; same function at load and record)
- no prior → Insert
- incoming segment newer than prior → Supersede (refresh)
- incoming segment older → KeepPrior
- same segment, identical digest → KeepPrior (every policy)
- same segment, divergent: RejectDivergent → Refuse; LastWins → Replace; FirstWins → KeepPrior

Replay lookup — `ReplayServing::lookup` (7 rows)
- miss → `Err(Miss)`
- hit, Unbounded → `Hit{Fresh}`
- hit, age ≤ max → `Hit{Fresh}`
- hit, age > max: Fail → `Err(Stale{Known})`; Warn → `Hit{Expired}`
- hit, recorded_at Unknown: Fail → `Err(Stale{Unknown})`; Warn → `Hit{Expired{Unknown}}`

Live route (6 rows)
- Record(Missing): fresh hit → `Serve`; expired hit → `CallLive(Recorded)`; miss → `CallLive(Unrecorded)`
- Verify: hit → `Recorded`; miss → `Unrecorded`
- Record(All) → `CallLive(Unrecorded)` (index empty by construction)

Ledger — `RecordLedger::admit` (4 rows): session's first Insert/Supersede/Replace → `Append(Now(stamp))`; later ones → `Append(Already)`; KeepPrior → `SkipIdentical`; Refuse → `Refuse`.

Verify — `VerifyServing::verify` (3 rows): Unrecorded → `Unrecorded`; equal digest → `Matches`; different → `Diverged`.

Payload stamps (replace meta.rs:12-71), each a `ProtocolEvent::Custom` line old readers ignore:
- `cassette-header {key_schema, header_allowlist:[..], upstream: str|null}` (format = that line's envelope `v`)
- `cassette-segment {segment, recorded_at_ms, recorder}`
- `cassette-entry {key: "<hex16>", segment}`, written under the interaction's own `id`

Recorder version is provenance only and never invalidates (otherwise every proxima release orphans every cassette). Age is per segment, because record-missing appends would make a cassette-level `recorded_at_ms` (meta.rs:20) wrong for new entries.

## 2. Pipes vs FSM methods

| item | form | In / Out / Err | caller capability |
|---|---|---|---|
| `ReplayUpstream` (reshaped, mod.rs:70-77) | transform `SendPipe` | `Request<Bytes>` / `Response<Bytes>` / `ProximaError` | holds `ReplayServing` + `Arc<CassetteIndex<RecordedResponse>>` + `HeaderAllowlist`; strict cassettes and raw recordings; public surface unchanged |
| `CassetteLive<O>` (new, replaces `RecordingTee` test_support.rs:453-609) | transform `SendPipe` | same | serves hits from the index, calls `inner` on `CallLive`, hands the exchange to `O`; record-missing and verify do not exist today |
| `Recorder` (new) | observe `SendPipe` | `Exchange` / `Exchange` / `ProximaError` | runs `ledger.admit` and executes the `AppendDecision`; ledger behind `proxima_primitives::sync::AsyncMutex` (P21 level 2; replaces the blocking `Mutex` at test_support.rs:460,572) |
| `Verifier` (new) | observe `SendPipe` | `Exchange` / `Exchange` / `ProximaError` | runs `serving.verify`, emits `proxima::telemetry::warn!` per finding (the stale report); never writes the cassette |

`O: SendPipe<In = Exchange, Out = Exchange, Err = ProximaError>` is the one composition seam: `AndThen::new(recorder, caller_observe)` plugs in.

```rust
// before (test_support.rs:179-185): record-only, no hit serving, no verify
into_handle(RecordingTee { inner: into_handle(inner), sink, match_spec, duplicates, seen: Arc::new(Mutex::new(HashMap::new())) })
// after: record-missing serves hits, misses go live and are appended
into_handle(CassetteLive::record(serving, index, allowlist, into_handle(inner), Recorder::new(sink, serving.open_ledger())))
into_handle(CassetteLive::verify(serving, index, allowlist, into_handle(inner), Verifier::new()))
into_handle(CassetteLive::record(serving, index, allowlist, into_handle(inner), AndThen::new(Recorder::new(sink, ledger), audit_tap)))

// strict, before (test_support.rs:200): ReplayUpstream::from_jsonl_with(path, "cassette", runtime, match_spec)
// strict, after:
let (ready, index) = load_cassette(source, KeySource::Stamped, expecting).await?;   // Ready::Replay
into_handle(ReplayUpstream::from_index(label, serving, index, allowlist))
```

Not pipes, and why:
- FSM states/transitions: sans-IO decisions that consume `self`; `Pipe::call(&self, In)` cannot consume its state.
- `IndexBuilder`, `CassetteIndex`, `RecordLedger`: caller-owned storage, not algebra.
- `request_key` / `response_digest`: gate 2 — `request_key(&request, &allowlist)` and `KeyRequest(allowlist).call(request)` give identical power; a swappable keying pipe would let a caller change keys without changing the stamped `KeySpecDigest`.
- `load_cassette(source, KeySource, ExpectingHeader) -> Result<(Ready | Rebuild…, CassetteIndex<RecordedResponse>), Refusal>`: passes gate 1 but fails gate 2 (`CassetteLoad.call((s, e)).await` == `load_cassette(s, k, e).await`); maps `RecordingEvent`s to `HeaderEvent`/`BodyEvent` with the existing per-`InteractionId` assembly loop (mod.rs:202-265, moved).
- harness `cassette_pipe*` fns: the edge — env, config, clock (`now_unix_ms`, test_support.rs:254), `fs::exists` probe in; outcome variants executed (truncate/backup, header write, open sink).
- payload types (`Policy`, events, `Exchange`, stamps): data.

Information destroyed at the erased-handle boundary: `Refusal` stays typed through the chain but becomes `ProximaError` at `into_handle` (forced: handler.rs:113-118). `Miss` → `ReplayMiss { fingerprint: render_request_line(..) + " [key 0x..]" }` (cold path; keeps the readable form `known_keys` asserts use — proxima-intercept capture_replay_round_trip.rs:95 expects `"POST /responses?"`). Every other refusal → `Record(<display with rebuild hint>)`.

## 3. Replaced / deleted, and config parity

- proxima-recording/src/replay/keying.rs: `MatchSpec` :28-35 and `finish_match_key` :62-72 deleted (body always keyed; `match_body`/`request_bodies` go away); `match_key_from_request(_with)` :74-90 and `match_key_from_recording` :92-100 → `request_key` + stored digest; `write_match_key` :102-132 → `render_request_line`.
- meta.rs:12-71: `CassetteMeta`, `CASSETTE_META_KIND` → three stamp payloads.
- mod.rs: `by_match_key: HashMap<String,RecordedResponse>` :73 and per-request `.cloned()` :175 move into the index; `from_jsonl_with`/`from_source_with` :94-136 and `meta()` :159 deleted; silent last-wins at :239 replaced by the shared duplicate rule.
- ws.rs:29,93,165 → `request_key` + label map (S7).
- jsonl/wire.rs:129-134: string version error → typed `ProximaError::UnsupportedRecordingVersion { expected: u32, found: u32 }` in proxima-core (Display keeps "unsupported recording version").
- src/cassette_config.rs: `ModePolicy` :46-79 (incl. `Auto`) → `CassetteMode`; `RerecordPolicy` :84-118 → `Disposition` (`Fail` deleted; strict never writes); `StalenessPolicy` :153-182 → `OnStale`; `RerecordDecision`/`StaleDecision` :503-519 and `CassetteHooks`/`RerecordHook`/`StaleHook` :521-566 deleted (`Arc<dyn Fn>`, P20; uses become per-test config); `DuplicatePolicy` :123-147 moves to fsm.rs, re-exported.
- src/test_support.rs: `cassette_pipe_with_hooks` :103-191 → `cassette_pipe_with`; `replay_cassette` :193-252 → `load_cassette` + `ReplayUpstream::from_index`; `RecordingTee` :453-609 → `CassetteLive` + `Recorder`; swallowed `let _ = remove_file` :124 and `let _ = create_dir_all` :149 errors now propagate.
- proxima-test/src/harness.rs:35-42 `resolve_mode` deleted, plus `Mode` (lib.rs:56-59) and `CassetteCtx.mode` (lib.rs:64); presence becomes an event.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Builder, Deserialize, Serialize, Settings)]
#[settings(prefix = "PROXIMA_CASSETTE")] #[builder(derive(Clone, Debug))]
pub struct CassetteConfig {
    #[setting(default_str = "record-missing", resolve_with = "parse_mode")]    pub mode: CassetteMode,   // owner: missing builds; strict is opt-in
    #[setting(default_str = "backup", resolve_with = "parse_disposition")]     pub replace: Disposition,
    #[setting(default_str = "reject-divergent", resolve_with = "parse_duplicates")] pub duplicates: DuplicatePolicy,
    #[setting(default_str = "", resolve_with = "parse_allowlist")]             pub header_allowlist: Vec<String>,   // PROXIMA_CASSETTE_HEADER_ALLOWLIST=accept,x-api-version
    #[setting(default)]                                                        pub max_age_ms: Option<u64>,         // None = unbounded (replaces the 0 sentinel at :249-254)
    #[setting(default_str = "fail", resolve_with = "parse_on_stale")]          pub on_stale: OnStale,
    #[setting(default)]                                                        pub upstream: Option<String>,        // None = undeclared; recorded and compared as such
}
impl CassetteConfig {
    pub fn resolve_for_dir(dir: &Path) -> Result<Self, ProximaError>;                         // unchanged layering :309-321
    pub fn lower(&self, now: UnixMillis) -> Result<(Policy, HeaderAllowlist), ProximaError>;  // config → FSM input
    pub fn layered() -> CassetteLayerBuilder;   // with_mode / with_replace / with_duplicates / with_header_allowlist / with_max_age / with_on_stale / with_upstream
}
```

- Default `mode = record-missing` (owner decision): a missing cassette is recorded, a miss is recorded live; `strict` is the opt-in toggle for CI. In strict, the missing-cassette refusal names `PROXIMA_CASSETTE_MODE=record-missing` as the way back.
- Consequence for the existing unstamped fixtures: under the default they are rebuilt (backed up to `.bak`, re-recorded) on first run instead of refused; S6's AC becomes "e2e `weather_cassette_round_trips` passes under the default AND under `PROXIMA_CASSETTE_MODE=strict` after the re-record", and the Rebuild of an unversioned cassette needs the upstream reachable at that moment (record-missing calls live).
- Legacy `PROXIMA_CASSETTE` alias (:225) removed (owner to confirm): record/replay/auto don't map 1:1 onto four modes; aliasing `auto` would silently change behavior.
- `Policy` is a lossy lowering (digests); config is the mirror type and `lower` the explicit bridge.
- Parity fixture: env (`temp_env`), TOML (`from_path`), bon builder, `layered().with_*` yield equal `CassetteConfig`s and equal `lower(UnixMillis(T))`.
- No `sized` constants: version constants are spec values (P12 exemption).

## 4. Worked example (S8 walkthrough and test)

Inputs: header allowlist `["accept"]`, upstream `None`, duplicates `reject-divergent`, replace `backup`, on_stale `fail`; `now = UnixMillis(1_791_676_800_000)` (2026-10-11T00:00Z). Policy A `max_age_ms = Some(2_592_000_000)` (30 d); Policy B `Some(604_800_000)` (7 d). R1 `GET /v1/forecast?city=oslo`, no headers, empty body; R2 = R1 + `accept: text/csv`; R3 = R1 in a second run under Policy B. K1/K2 symbolic (S1 pins hex values). `D1 = response_digest(200, [b"{\"temp\":12}"])` (b64 `eyJ0ZW1wIjoxMn0=`, line 3).

A. `weather.jsonl` verbatim: first loader event is `HeaderEvent::Headerless`.

| mode | presence | header outcome | edge executes |
|---|---|---|---|
| strict | `ReadCassette` | `Refuse(Version(Unversioned))` | test fails: "cassette …/weather.jsonl is unversioned; rebuild with PROXIMA_CASSETTE_MODE=rerecord" |
| verify | `ReadCassette` | `Refuse(Version(Unversioned))` | same; verify never writes |
| record-missing | `ReadCassette` | `Rebuild{Unversioned, Replace(Backup)}` → `Record(Missing, session 0)` | rename to `weather.jsonl.bak`, write `cassette-header`; R1 `CallLive(Unrecorded)` → `Append(Now(seg0))`; R2 → `Append(Already)`; repeat R1 → KeepPrior → `SkipIdentical` |
| rerecord | `StartFresh{Replace(Backup)}` (file not read) | none | same writes as record-missing |

B. Stamped file (three lines prepended; S6 re-record shape):

```
{"v":3,"id":"<ulid-h>","ts_ms":0,"proto":"custom","kind":"cassette-header","payload":{"key_schema":1,"header_allowlist":["accept"],"upstream":null}}
{"v":3,"id":"<ulid-s>","ts_ms":0,"proto":"custom","kind":"cassette-segment","payload":{"segment":0,"recorded_at_ms":1790812800000,"recorder":"proxima 0.1.0"}}
{"v":3,"id":"01KT4M602X24W4F7P4AZ5TRYEY","ts_ms":0,"proto":"custom","kind":"cassette-entry","payload":{"key":"<K1>","segment":0}}
```

Load: `Header` (equal) → `Accepted`; `Segment(0, At(1_790_812_800_000))` → `Continue`; `Entry(K1, seg0, D1)` → `Insert` → slot0; `End` → `Ready`. Age of seg0 = 864_000_000 ms (10 d): fresh under A, expired under B.

| mode | R1 (run 1, A) | R2 (run 1, A) | R3 (run 2, B) |
|---|---|---|---|
| strict | `Hit{slot0, Fresh}` → 200 `{"temp":12}` | `Err(Miss{K2})` → `ReplayMiss` | `Err(Stale{K1, Known(864_000_000), 604_800_000})`; with `on_stale=warn`: `Hit{slot0, Expired}` + warn event |
| record-missing | `Serve(slot0)`, inner not called | `CallLive(Unrecorded)` → live D1 → Insert → `Append(Now(seg1 @now))` | `CallLive(Recorded{slot0, seg0, D1, Expired})` → live D1 → Supersede → `Append(Now(seg2))` |
| verify | `Prior::Recorded{slot0,…,Fresh}` → live D1 → `Matches{K1, Fresh}` | `Unrecorded{K2}` | `Matches{K1, Expired{Known(864_000_000), 604_800_000}}`; no writes |
| rerecord | `StartFresh{Replace(Backup)}`; `CallLive(Unrecorded)` → `Append(Now(seg0))` | `Append(Already)` (K2) | backs up again; `Append(Now(seg0))` |

## 5. Slices (each compiles and keeps tests green; every AC names a count)

- S1 keying: add `KeyDigest`, `HeaderAllowlist`, `request_key`, `response_digest`, `KEY_SCHEMA_VERSION`, `render_request_line` beside the old functions. AC: `cargo nextest run -p proxima-recording --features replay -E 'test(keying::)'` = 12 passed / 0 failed (3 existing + 9 new: query order invariance, multi-valued query, header case, header outside allowlist ignored, allowlist canonical digest, separator ambiguity, K1 ≠ K2 pinned, body digest) + alloc-counter test: 0 allocations over 100,000 `request_key` calls.
- S2 FSM setup through load: fsm.rs states/outcomes for setup, header, load, plus index.rs. AC: `-E 'test(fsm::)'` = 41 passed (8 + 11 + 15 + 7, load per R5); `cargo build -p proxima-recording --no-default-features --features alloc,replay` exit 0 and `--emit=metadata` lists `replay::fsm` and `replay::index` (count 2, not 0).
- S3 FSM serving: `ReplayServing`, `RecordServing`, `VerifyServing`, `RecordLedger`. AC: `fsm::` = 66 passed (41 + 8 + 8 + 6 + 3, per R6) + alloc-counter: 0 allocations over 100,000 `lookup` calls on a 1,000-entry index.
- S4 stamps + typed version error: header/segment/entry codecs beside `CassetteMeta`; `ProximaError::UnsupportedRecordingVersion` at wire.rs:129. AC: `-E 'test(meta::)'` = 9 passed (3 round-trip + 6 malformed); existing `jsonl::` count unchanged from the pre-slice run (record it).
- S5 loader edge + `ReplayUpstream` reshape: `load_cassette` with `KeySource::{Stamped, Derived}`; `ReplayUpstream::from_index`; `from_jsonl`/`from_source` keep signatures via `Derived`. AC: the 2 tests in `replay::tests` (mod.rs:408-443) pass; proxima-intercept `capture_replay_round_trip` passes with `known_keys == ["POST /responses?"]`; new `loader::` = 6 passed (headerless, format fault, segment order, unknown segment, dangling interaction, derived parity).
- S6 harness flip (one atomic slice; may exceed 20 min): new `CassetteConfig` + `lower`; `CassetteLive`, `Recorder`, `Verifier` in the proxima crate; `cassette_pipe`/`cassette_pipe_with` rewired; `RecordingTee` and hooks removed; the 14 cassette tests at test_support.rs:689-1080 rewritten (9 kept: record-then-replay, backup, divergent duplicate, identical dedupe, last-wins, stale fail, stale warn, format-version hint, body-keyed POSTs; 5 replaced: rerecord-fail → strict-never-writes, rerecord hook → per-test replace, use-existing → record-missing-serves-hits, stale hook → per-test on-stale warn, provenance → unversioned-refused-in-strict/rebuilt-in-record-missing); re-record `weather.jsonl` and `sugar.jsonl` with `PROXIMA_CASSETTE_MODE=rerecord`. AC: `-E 'test(test_support::)'` = 14 passed + 4 config-parity tests; e2e `weather_cassette_round_trips` passes under default strict; `grep -c cassette-header tests/cassettes/{weather,sugar}.jsonl` = 1 each.
- S7 deletions: `MatchSpec`, `match_key_*`, `CassetteMeta`, `from_*_with`, `ModePolicy`, `RerecordPolicy`, `StalenessPolicy`, `RerecordDecision`, `StaleDecision`; ws.rs → `request_key`. AC: `rg -c 'MatchSpec|match_key_from|CassetteMeta|CassetteHooks|RerecordDecision|StaleDecision|ModePolicy' --type rust .` = 0; ws round-trip tests = 5 passed.
- S8 proxima-test cleanup + walkthrough: delete `resolve_mode` (harness.rs:35-42), `Mode`, `CassetteCtx.mode`; add `proxima-recording/examples/cassette_fsm_walkthrough.rs` running section 4. Ordering: after the in-flight `proxima::test` scheduling edits to `proxima-test/src/lib.rs` and `proxima-macros/`. AC: walkthrough exits 0 after asserting 28 decisions; `rg -c 'PROXIMA_CASSETTE"' proxima-test` = 0.

## Designs abandoned, and the principle that ruled each out

- Per-request `String` key (alloc, ambiguous text) → `KeyDigest` (P11).
- Recomputing keys from `RequestHeader` at load: lossy BTreeMap, so a guessed replay.
- One FSM-owned index: caller-owned storage keeps the FSM tier-3 (P3).
- One runtime FSM enum: allows wrong (state, event) pairs; per-state structs + outcome enums do not (P11).
- `LiveArm::Absent` placeholder for strict → `Ready::{Replay, Record, Verify}` split.
- Splitting lookup from serve + a decision-override pipe for amnesty: failed gate 2 once hooks became config.
- `KeyRequest` and `CassetteLoad` pipes: gate 2, identical call sites.
- `Arc<dyn Fn>` hooks (P20) and the blocking-`Mutex` seen-map (P21).
- Cassette-level `recorded_at`: wrong for appended entries.
- `filter` for admission: its rejection aborts the chain with `Err`.
