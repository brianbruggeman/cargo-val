//! Whether a test or bench executable still waits for its package's bins, independent of how cargo's bytes arrive.

use crate::metadata_scan::BinCounts;

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

fn waits_for_bins(bins: &BinCounts, gate: BinGate, build: BuildState, package_id: &str) -> bool {
    match (build, gate) {
        (BuildState::Succeeded, _) | (_, BinGate::Off) => false,
        (_, BinGate::Metadata) => bins.awaiting(package_id),
        (_, BinGate::BuildFinished) => true,
    }
}

/// Decides, from the build so far, whether a package's test executables still wait for its bins.
pub struct DiscoveryFsm {
    bins: BinCounts,
    gate: BinGate,
    build: BuildState,
    stdout_eof: bool,
    stderr_eof: bool,
    exit_code: Option<i32>,
    build_reported: bool,
}

impl DiscoveryFsm {
    #[must_use]
    pub const fn new(bins: BinCounts, gate: BinGate) -> Self {
        Self {
            bins,
            gate,
            build: BuildState::Running,
            stdout_eof: false,
            stderr_eof: false,
            exit_code: None,
            build_reported: false,
        }
    }

    #[must_use]
    pub const fn gate(&self) -> BinGate {
        self.gate
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
    pub const fn lost_artifacts_on_clean_exit(&self, dropped: usize) -> bool {
        dropped > 0 && matches!(self.exit_code, Some(0))
    }

    /// Whether cargo has said everything, exited, and nothing is parked or still running.
    #[must_use]
    pub const fn drained(&self, unaccounted: usize, parked_empty: bool) -> bool {
        self.stdout_eof
            && self.stderr_eof
            && self.exit_code.is_some()
            && unaccounted == 0
            && parked_empty
    }

    /// Whether an executable of this package waits for its bins now.
    #[must_use]
    pub fn waits(&self, package_id: &str) -> bool {
        waits_for_bins(&self.bins, self.gate, self.build, package_id)
    }

    /// Calls `visit` with the id of `package_id` and of each package it depends on, from the metadata the bin gate read.
    pub fn dependency_closure(&self, package_id: &str, visit: impl FnMut(&[u8])) {
        self.bins.dependency_closure(package_id, visit);
    }

    pub fn bin_built(&mut self, package_id: &str) {
        self.bins.note_built(package_id);
    }

    /// Whether cargo reported `build-finished`; without it a failing exit means cargo refused before building.
    #[must_use]
    pub const fn build_reported(&self) -> bool {
        self.build_reported
    }

    pub const fn build_finished(&mut self, success: bool) {
        self.build_reported = true;
        self.build = if success { BuildState::Succeeded } else { BuildState::Failed };
    }

    pub const fn stdout_eof(&mut self) {
        self.stdout_eof = true;
        if matches!(self.build, BuildState::Running) {
            self.build = BuildState::Failed;
        }
    }

    pub const fn stderr_eof(&mut self) {
        self.stderr_eof = true;
    }

    pub const fn exited(&mut self, code: i32) {
        self.exit_code = Some(code);
    }
}

#[cfg(test)]
mod tests {
    use std::boxed::Box;

    use super::{BinGate, DiscoveryFsm};
    use crate::metadata_scan::{BinCounts, MetadataScan};

    const PROBE: &str = "path+file:///work/buildscript_probe#2.7.1-rc.2";
    const ELSEWHERE: &str = "path+file:///elsewhere#0.1.0";
    const METADATA: &str = r#"{"packages":[{"name":"buildscript_probe","version":"2.7.1-rc.2","id":"path+file:///work/buildscript_probe#2.7.1-rc.2","targets":[{"kind":["lib"],"crate_types":["lib"],"name":"buildscript_probe"},{"kind":["bin"],"crate_types":["bin"],"name":"probe"}],"manifest_path":"/work/buildscript_probe/Cargo.toml"}],"workspace_members":["path+file:///work/buildscript_probe#2.7.1-rc.2"],"resolve":null,"target_directory":"/work/probe_target","version":1}"#;

    fn bins_of_probe() -> BinCounts {
        let mut scan = Box::new(MetadataScan::new());
        scan.feed(METADATA.as_bytes()).expect("document fits");
        scan.finish().bins
    }

    fn fsm(gate: BinGate) -> DiscoveryFsm {
        let bins = if gate == BinGate::Metadata { bins_of_probe() } else { BinCounts::new() };
        DiscoveryFsm::new(bins, gate)
    }

    #[proxima::test]
    async fn gate_off_never_waits() {
        let discovery = fsm(BinGate::Off);

        assert!(!discovery.waits(PROBE));
    }

    #[proxima::test]
    async fn metadata_gate_waits_until_a_bin_of_that_package_is_built() {
        let mut discovery = fsm(BinGate::Metadata);
        assert!(discovery.waits(PROBE));
        assert!(!discovery.waits(ELSEWHERE));

        discovery.bin_built(ELSEWHERE);
        assert!(discovery.waits(PROBE));
        discovery.bin_built(PROBE);

        assert!(!discovery.waits(PROBE));
    }

    #[proxima::test]
    async fn build_success_stops_every_package_waiting() {
        let mut discovery = fsm(BinGate::BuildFinished);
        assert!(discovery.waits(PROBE));

        discovery.build_finished(true);

        assert!(!discovery.waits(PROBE));
        assert!(!discovery.waits(ELSEWHERE));
    }

    #[proxima::test]
    async fn build_failure_keeps_packages_waiting_and_a_clean_exit_cannot_stand_over_dropped_artifacts() {
        let mut discovery = fsm(BinGate::BuildFinished);

        discovery.build_finished(false);
        discovery.exited(0);

        assert!(discovery.waits(PROBE));
        assert!(discovery.lost_artifacts_on_clean_exit(2));
        assert!(!discovery.lost_artifacts_on_clean_exit(0));
    }

    #[proxima::test]
    async fn stdout_eof_while_the_build_is_running_counts_as_failed() {
        let mut discovery = fsm(BinGate::BuildFinished);

        discovery.stdout_eof();

        assert!(discovery.stdout_closed());
        assert!(discovery.waits(PROBE));
    }

    #[proxima::test]
    async fn stdout_eof_after_a_successful_build_keeps_the_build_succeeded() {
        let mut discovery = fsm(BinGate::BuildFinished);
        discovery.build_finished(true);

        discovery.stdout_eof();

        assert!(discovery.stdout_closed());
        assert!(!discovery.waits(PROBE));
    }

    #[proxima::test]
    async fn drained_needs_both_streams_an_exit_and_nothing_parked_or_running() {
        let mut discovery = fsm(BinGate::BuildFinished);
        discovery.stdout_eof();
        discovery.stderr_eof();
        discovery.exited(1);
        assert!(!discovery.drained(0, false));

        assert!(!discovery.drained(1, true));
        assert!(discovery.drained(0, true));
    }
}
