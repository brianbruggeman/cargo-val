//! What to do with each cargo build record, independent of how its bytes arrive.

use crate::cargo_message::TestArtifact;
use crate::metadata_scan::BinCounts;
use crate::parked_artifacts::{OwnedArtifact, ParkFault, ParkedArtifacts};

// a test or bench executable may use CARGO_BIN_EXE_<name>, so it waits for its package's bins
/// How a test or bench executable learns that its package's bins exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinGate {
    Off,
    Metadata,
    BuildFinished,
}

impl BinGate {
    #[must_use]
    pub const fn name(self) -> &'static [u8] {
        match self {
            Self::Off => b"off",
            Self::Metadata => b"metadata",
            Self::BuildFinished => b"build-finished",
        }
    }
}

/// What cargo said about the build so far; only a build that succeeded proves every bin exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildState {
    Running,
    Succeeded,
    Failed,
}

/// What an artifact record means right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    Launch,
    Park,
}

/// Why parked artifacts are being released or dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    BinBuilt,
    BuildSucceeded,
    BuildFailed,
    StdoutEof,
}

impl Reason {
    #[must_use]
    pub const fn text(self) -> &'static [u8] {
        match self {
            Self::BinBuilt => b"bin-built",
            Self::BuildSucceeded => b"build-succeeded",
            Self::BuildFailed => b"build-failed",
            Self::StdoutEof => b"stdout-eof",
        }
    }

    const fn settles_build(self) -> bool {
        !matches!(self, Self::BinBuilt)
    }
}

/// Which event ended the build, as the debug line names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildEnd {
    FinishedSuccess,
    FinishedFailure,
    StdoutEof,
}

impl BuildEnd {
    #[must_use]
    pub const fn text(self) -> &'static [u8] {
        match self {
            Self::FinishedSuccess => b" build-finished success=true",
            Self::FinishedFailure => b" build-finished success=false",
            Self::StdoutEof => b" stdout-eof",
        }
    }
}

/// A record of a decision for the driver to render.
#[derive(Clone, Copy)]
pub enum DebugEvent<'held> {
    BuildEnded { end: BuildEnd, parked: usize },
    Dropped { reason: Reason, artifact: &'held OwnedArtifact },
}

/// A parked artifact that is ready to list now.
#[derive(Clone, Copy)]
pub struct Released<'held> {
    pub reason: Reason,
    pub artifact: &'held OwnedArtifact,
}

/// One thing the driver does next, in the order the FSM produced it.
#[derive(Clone, Copy)]
pub enum Output<'held> {
    List(Released<'held>),
    Debug(DebugEvent<'held>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Releasing(Reason),
    Dropping(Reason),
}

/// Parks, releases and drops test artifacts as cargo's build progresses.
pub struct DiscoveryFsm {
    bins: BinCounts,
    gate: BinGate,
    build: BuildState,
    parked: ParkedArtifacts,
    dropped: usize,
    stdout_eof: bool,
    stderr_eof: bool,
    exit_code: Option<i32>,
    announce: Option<(BuildEnd, usize)>,
    ready: OwnedArtifact,
    phase: Phase,
}

fn waits_for_bins(bins: &BinCounts, gate: BinGate, build: BuildState, package_id: &str) -> bool {
    match (build, gate) {
        (BuildState::Succeeded, _) | (_, BinGate::Off) => false,
        (_, BinGate::Metadata) => bins.awaiting(package_id),
        (_, BinGate::BuildFinished) => true,
    }
}

impl DiscoveryFsm {
    #[must_use]
    pub const fn new(bins: BinCounts, gate: BinGate) -> Self {
        Self {
            bins,
            gate,
            build: BuildState::Running,
            parked: ParkedArtifacts::new(),
            dropped: 0,
            stdout_eof: false,
            stderr_eof: false,
            exit_code: None,
            announce: None,
            ready: OwnedArtifact::EMPTY,
            phase: Phase::Idle,
        }
    }

    #[must_use]
    pub const fn gate(&self) -> BinGate {
        self.gate
    }

    #[must_use]
    pub const fn dropped(&self) -> usize {
        self.dropped
    }

    #[must_use]
    pub const fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    #[must_use]
    pub const fn stdout_closed(&self) -> bool {
        self.stdout_eof
    }

    #[must_use]
    pub const fn stderr_closed(&self) -> bool {
        self.stderr_eof
    }

    /// A clean exit cannot stand when cargo's own stream lost artifacts.
    #[must_use]
    pub const fn lost_artifacts_on_clean_exit(&self) -> bool {
        self.dropped > 0 && matches!(self.exit_code, Some(0))
    }

    /// Whether cargo has said everything, exited, and nothing is parked or still running.
    #[must_use]
    pub const fn drained(&self, unaccounted: usize) -> bool {
        self.stdout_eof
            && self.stderr_eof
            && self.exit_code.is_some()
            && unaccounted == 0
            && self.parked.is_empty()
    }

    /// Whether this artifact lists now or waits for its package's bins.
    #[must_use]
    pub fn arrival(&self, artifact: &TestArtifact<'_>) -> Arrival {
        let waits = matches!(artifact.target_kind, "test" | "bench")
            && waits_for_bins(&self.bins, self.gate, self.build, artifact.package_id);
        if waits { Arrival::Park } else { Arrival::Launch }
    }

    /// Holds an artifact whose [`arrival`](Self::arrival) was [`Arrival::Park`].
    ///
    /// # Errors
    ///
    /// Returns [`ParkFault`] when the queue is full or a field passes its capacity.
    pub fn park(&mut self, artifact: &TestArtifact<'_>) -> Result<(), ParkFault> {
        self.parked.park(artifact)
    }

    pub fn bin_built(&mut self, package_id: &str) {
        self.bins.note_built(package_id);
        self.phase = Phase::Releasing(Reason::BinBuilt);
    }

    pub fn build_finished(&mut self, success: bool) {
        let (build, end, reason) = if success {
            (BuildState::Succeeded, BuildEnd::FinishedSuccess, Reason::BuildSucceeded)
        } else {
            (BuildState::Failed, BuildEnd::FinishedFailure, Reason::BuildFailed)
        };
        self.end_build(build, end, reason);
    }

    pub fn stdout_eof(&mut self) {
        self.stdout_eof = true;
        let build = match self.build {
            BuildState::Running => BuildState::Failed,
            settled => settled,
        };
        self.end_build(build, BuildEnd::StdoutEof, Reason::StdoutEof);
    }

    pub const fn stderr_eof(&mut self) {
        self.stderr_eof = true;
    }

    pub const fn exited(&mut self, code: i32) {
        self.exit_code = Some(code);
    }

    fn end_build(&mut self, build: BuildState, end: BuildEnd, reason: Reason) {
        self.announce = Some((end, self.parked.len()));
        self.build = build;
        self.phase = Phase::Releasing(reason);
    }

    /// The next thing to do after the last input; `None` once everything it caused is out.
    pub fn next_output(&mut self) -> Option<Output<'_>> {
        if let Some((end, parked)) = self.announce.take() {
            return Some(Output::Debug(DebugEvent::BuildEnded { end, parked }));
        }
        loop {
            match self.phase {
                Phase::Idle => return None,
                Phase::Releasing(reason) => {
                    let (bins, gate, build) = (&self.bins, self.gate, self.build);
                    let ready = self
                        .parked
                        .take_ready(|package_id| !waits_for_bins(bins, gate, build, package_id));
                    if let Some(artifact) = ready {
                        self.ready = artifact;
                        let artifact = &self.ready;
                        return Some(Output::List(Released { reason, artifact }));
                    }
                    self.phase = if reason.settles_build() {
                        Phase::Dropping(reason)
                    } else {
                        Phase::Idle
                    };
                }
                Phase::Dropping(reason) => {
                    let Some(artifact) = self.parked.take_ready(|_| true) else {
                        self.phase = Phase::Idle;
                        return None;
                    };
                    self.dropped += 1;
                    self.ready = artifact;
                    let artifact = &self.ready;
                    return Some(Output::Debug(DebugEvent::Dropped { reason, artifact }));
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use std::boxed::Box;
    use std::string::{String, ToString};
    use std::vec::Vec;

    use super::{Arrival, BinGate, BuildEnd, DebugEvent, DiscoveryFsm, Output, Reason};
    use crate::cargo_message::{CargoMessage, TestArtifact, parse_cargo_message};
    use crate::metadata_scan::{BinCounts, MetadataScan};

    const ARTIFACT_RECORD: &str = include_str!("../tests/fixtures/compiler_artifact_test.jsonl");
    const PROBE: &str = "path+file:///work/buildscript_probe#2.7.1-rc.2";
    const METADATA: &str = r#"{"packages":[{"name":"buildscript_probe","version":"2.7.1-rc.2","id":"path+file:///work/buildscript_probe#2.7.1-rc.2","targets":[{"kind":["lib"],"crate_types":["lib"],"name":"buildscript_probe"},{"kind":["bin"],"crate_types":["bin"],"name":"probe"}],"manifest_path":"/work/buildscript_probe/Cargo.toml"}],"workspace_members":[],"resolve":null,"target_directory":"/work/probe_target","version":1}"#;

    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Listed(Reason, String),
        Dropped(Reason, String),
        BuildEnded(BuildEnd, usize),
    }

    fn record(kind: &str, target: &str) -> String {
        ARTIFACT_RECORD
            .replace(r#""kind":["lib"]"#, &std::format!(r#""kind":["{kind}"]"#))
            .replace(r#""name":"buildscript_probe""#, &std::format!(r#""name":"{target}""#))
    }

    fn parsed<'text>(record: &'text str, scratch: &'text mut [u8]) -> TestArtifact<'text> {
        match parse_cargo_message(record.as_bytes(), scratch, 4096, 8) {
            Ok(CargoMessage::TestArtifact(artifact)) => artifact,
            other => panic!("real compiler-artifact record was not selected: {other:?}"),
        }
    }

    fn bins_of_probe() -> BinCounts {
        let mut scan = Box::new(MetadataScan::new());
        scan.feed(METADATA.as_bytes()).expect("document fits");
        scan.finish().bins
    }

    fn fsm(gate: BinGate) -> Box<DiscoveryFsm> {
        let bins = if gate == BinGate::Metadata { bins_of_probe() } else { BinCounts::new() };
        Box::new(DiscoveryFsm::new(bins, gate))
    }

    fn arrive(fsm: &mut DiscoveryFsm, kind: &str, target: &str) -> Arrival {
        let text = record(kind, target);
        let mut scratch = [0_u8; 64];
        let artifact = parsed(&text, &mut scratch);
        let arrival = fsm.arrival(&artifact);
        if arrival == Arrival::Park {
            fsm.park(&artifact).expect("room to park");
        }
        arrival
    }

    fn drain(fsm: &mut DiscoveryFsm) -> Vec<Seen> {
        let mut seen = Vec::new();
        while let Some(output) = fsm.next_output() {
            seen.push(match output {
                Output::List(released) => Seen::Listed(
                    released.reason,
                    released.artifact.as_artifact().target_name.to_string(),
                ),
                Output::Debug(DebugEvent::Dropped { reason, artifact }) => {
                    Seen::Dropped(reason, artifact.as_artifact().target_name.to_string())
                }
                Output::Debug(DebugEvent::BuildEnded { end, parked }) => Seen::BuildEnded(end, parked),
            });
        }
        seen
    }

    fn assert_quiet(fsm: &mut DiscoveryFsm) {
        assert_eq!(drain(fsm), Vec::<Seen>::new());
    }
    #[test]
    fn gate_off_launches_a_test_artifact_immediately() {
        let mut discovery = fsm(BinGate::Off);

        assert_eq!(arrive(&mut discovery, "test", "cli"), Arrival::Launch);
        assert_quiet(&mut discovery);
    }

    #[test]
    fn a_library_artifact_launches_even_when_the_gate_waits_for_the_build() {
        let mut discovery = fsm(BinGate::BuildFinished);

        assert_eq!(arrive(&mut discovery, "lib", "buildscript_probe"), Arrival::Launch);
    }

    #[test]
    fn metadata_gate_parks_until_a_bin_of_that_package_is_built() {
        let mut discovery = fsm(BinGate::Metadata);
        assert_eq!(arrive(&mut discovery, "test", "cli"), Arrival::Park);

        discovery.bin_built("path+file:///elsewhere#0.1.0");
        assert_quiet(&mut discovery);
        discovery.bin_built(PROBE);

        assert_eq!(drain(&mut discovery), [Seen::Listed(Reason::BinBuilt, "cli".to_string())]);
        assert_quiet(&mut discovery);
    }

    #[test]
    fn build_success_releases_every_parked_artifact_in_arrival_order() {
        let mut discovery = fsm(BinGate::BuildFinished);
        assert_eq!(arrive(&mut discovery, "test", "first"), Arrival::Park);
        assert_eq!(arrive(&mut discovery, "bench", "second"), Arrival::Park);

        discovery.build_finished(true);

        assert_eq!(
            drain(&mut discovery),
            [
                Seen::BuildEnded(BuildEnd::FinishedSuccess, 2),
                Seen::Listed(Reason::BuildSucceeded, "first".to_string()),
                Seen::Listed(Reason::BuildSucceeded, "second".to_string()),
            ]
        );
        assert_eq!(discovery.dropped(), 0);
        assert_eq!(arrive(&mut discovery, "test", "late"), Arrival::Launch);
    }

    #[test]
    fn build_failure_drops_parked_artifacts_and_counts_them() {
        let mut discovery = fsm(BinGate::BuildFinished);
        arrive(&mut discovery, "test", "first");
        arrive(&mut discovery, "test", "second");

        discovery.build_finished(false);

        assert_eq!(
            drain(&mut discovery),
            [
                Seen::BuildEnded(BuildEnd::FinishedFailure, 2),
                Seen::Dropped(Reason::BuildFailed, "first".to_string()),
                Seen::Dropped(Reason::BuildFailed, "second".to_string()),
            ]
        );
        assert_eq!(discovery.dropped(), 2);
        discovery.exited(0);
        assert!(discovery.lost_artifacts_on_clean_exit());
    }

    #[test]
    fn stdout_eof_while_the_build_is_running_counts_as_failed() {
        let mut discovery = fsm(BinGate::BuildFinished);
        arrive(&mut discovery, "test", "orphan");

        discovery.stdout_eof();

        assert_eq!(
            drain(&mut discovery),
            [
                Seen::BuildEnded(BuildEnd::StdoutEof, 1),
                Seen::Dropped(Reason::StdoutEof, "orphan".to_string()),
            ]
        );
        assert!(discovery.stdout_closed());
        assert_eq!(discovery.dropped(), 1);
    }

    #[test]
    fn stdout_eof_after_a_successful_build_drops_nothing() {
        let mut discovery = fsm(BinGate::BuildFinished);
        discovery.build_finished(true);
        drain(&mut discovery);

        discovery.stdout_eof();

        assert_eq!(drain(&mut discovery), [Seen::BuildEnded(BuildEnd::StdoutEof, 0)]);
        assert_eq!(discovery.dropped(), 0);
    }

    #[test]
    fn drained_needs_both_streams_an_exit_and_nothing_parked_or_running() {
        let mut discovery = fsm(BinGate::BuildFinished);
        arrive(&mut discovery, "test", "held");
        discovery.stdout_eof();
        discovery.stderr_eof();
        discovery.exited(1);
        assert!(!discovery.drained(0));

        drain(&mut discovery);
        assert!(!discovery.drained(1));
        assert!(discovery.drained(0));
    }
}
