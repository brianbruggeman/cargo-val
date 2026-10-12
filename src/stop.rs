#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopReport {
    pub max_fail: usize,
    pub in_flight_at_trip: usize,
    pub launched_after_trip: usize,
    pub listed_not_run: usize,
    pub targets_not_listed: usize,
    pub doc_at_trip: DocAtTrip,
}

/// Whether the doc-test child had started executing when fail-fast tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocAtTrip {
    NotSpawned,
    InterruptedBeforeExecuting,
    ExecutingLeftToFinish,
    AlreadyFinished,
}

impl DocAtTrip {
    #[must_use]
    pub const fn text(self) -> &'static [u8] {
        match self {
            Self::NotSpawned => b"not spawned",
            Self::InterruptedBeforeExecuting => b"interrupted before executing",
            Self::ExecutingLeftToFinish => b"executing, left to finish",
            Self::AlreadyFinished => b"already finished",
        }
    }
}

/// What a failure-count observation means for the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    Clear,
    FirstTrip,
    Tripped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Trip {
    in_flight: usize,
    doc: DocAtTrip,
}

/// Trip accounting for `--max-fail`; the driver feeds it facts and performs the I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopPolicy {
    max_fail: Option<usize>,
    trip: Option<Trip>,
    launched_after_trip: usize,
    listed_not_run: usize,
    targets_not_listed: usize,
    targets_listed: usize,
}

impl StopPolicy {
    #[must_use]
    pub const fn new(max_fail: Option<usize>) -> Self {
        Self {
            max_fail,
            trip: None,
            launched_after_trip: 0,
            listed_not_run: 0,
            targets_not_listed: 0,
            targets_listed: 0,
        }
    }

    #[must_use]
    pub const fn observe_failures(&self, failures: usize) -> Observation {
        match (self.max_fail, self.trip) {
            (Some(max_fail), None) if failures >= max_fail => Observation::FirstTrip,
            (Some(max_fail), Some(_)) if failures >= max_fail => Observation::Tripped,
            _ => Observation::Clear,
        }
    }

    pub const fn record_trip(&mut self, in_flight: usize, doc: DocAtTrip) {
        if self.trip.is_none() {
            self.trip = Some(Trip { in_flight, doc });
        }
    }

    #[must_use]
    pub const fn tripped(&self) -> bool {
        self.trip.is_some()
    }

    pub const fn launched(&mut self) {
        self.launched_after_trip += 1;
    }

    pub const fn listed_test_skipped(&mut self) {
        self.listed_not_run += 1;
    }

    /// Whether a listed test launches; a stopped run counts it as listed-not-run instead.
    #[must_use]
    pub const fn admit_listed(&mut self, interrupted: bool, stopped: bool) -> bool {
        if interrupted {
            return false;
        }
        if stopped {
            self.listed_test_skipped();
            return false;
        }
        true
    }

    pub const fn target_skipped(&mut self) {
        self.targets_not_listed += 1;
    }

    pub const fn target_listed(&mut self) -> usize {
        let index = self.targets_listed;
        self.targets_listed += 1;
        index
    }

    #[must_use]
    pub const fn report(&self) -> Option<StopReport> {
        let (Some(max_fail), Some(trip)) = (self.max_fail, self.trip) else {
            return None;
        };
        Some(StopReport {
            max_fail,
            in_flight_at_trip: trip.in_flight,
            launched_after_trip: self.launched_after_trip,
            listed_not_run: self.listed_not_run,
            targets_not_listed: self.targets_not_listed,
            doc_at_trip: trip.doc,
        })
    }
}

/// Whether the listing stream is read: always once stopped (drain), else only while a job slot is free.
#[must_use]
pub const fn listing_wanted(stopped: bool, live_sessions: usize, jobs: usize) -> bool {
    stopped || live_sessions < jobs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_trips_without_max_fail() {
        let policy = StopPolicy::new(None);

        assert_eq!(policy.observe_failures(1_000), Observation::Clear);
        assert!(!policy.tripped());
        assert_eq!(policy.report(), None);
    }

    #[test]
    fn trips_exactly_at_max_fail() {
        let policy = StopPolicy::new(Some(3));

        assert_eq!(policy.observe_failures(2), Observation::Clear);
        assert_eq!(policy.observe_failures(3), Observation::FirstTrip);
    }

    #[test]
    fn records_in_flight_once() {
        let mut policy = StopPolicy::new(Some(1));

        policy.record_trip(4, DocAtTrip::NotSpawned);
        policy.record_trip(9, DocAtTrip::AlreadyFinished);

        assert_eq!(policy.observe_failures(1), Observation::Tripped);
        let report = policy.report().expect("tripped policy reports");
        assert_eq!(report.in_flight_at_trip, 4);
        assert_eq!(report.doc_at_trip, DocAtTrip::NotSpawned);
    }

    #[test]
    fn report_is_none_before_trip() {
        let mut policy = StopPolicy::new(Some(2));
        policy.launched();
        policy.target_skipped();

        assert!(!policy.tripped());
        assert_eq!(policy.report(), None);
    }

    #[test]
    fn report_matches_worked_sequence() {
        let mut policy = StopPolicy::new(Some(2));
        assert_eq!(policy.target_listed(), 0);
        assert_eq!(policy.target_listed(), 1);
        assert_eq!(policy.observe_failures(2), Observation::FirstTrip);
        policy.record_trip(3, DocAtTrip::ExecutingLeftToFinish);
        policy.launched();
        policy.launched();
        policy.listed_test_skipped();
        policy.listed_test_skipped();
        policy.listed_test_skipped();
        policy.target_skipped();

        assert_eq!(
            policy.report(),
            Some(StopReport {
                max_fail: 2,
                in_flight_at_trip: 3,
                launched_after_trip: 2,
                listed_not_run: 3,
                targets_not_listed: 1,
                doc_at_trip: DocAtTrip::ExecutingLeftToFinish,
            })
        );
    }

    #[test]
    fn real_ignore_modes_listing_admits_all_when_running_and_counts_when_stopped() {
        const LISTING: &str = include_str!("../tests/fixtures/listing/ignore_modes_list.txt");
        let tests = LISTING.lines().filter(|line| line.ends_with(": test")).count();
        let mut running = StopPolicy::new(Some(1));
        let mut stopped = StopPolicy::new(Some(1));
        let mut interrupted = StopPolicy::new(Some(1));
        stopped.record_trip(0, DocAtTrip::NotSpawned);

        let admitted = (0..tests).filter(|_| running.admit_listed(false, false)).count();
        let skipped = (0..tests).filter(|_| !stopped.admit_listed(false, true)).count();
        let dropped = (0..tests).filter(|_| !interrupted.admit_listed(true, true)).count();

        assert_eq!((tests, admitted, skipped, dropped), (3, 3, 3, 3));
        assert_eq!(stopped.report().expect("tripped").listed_not_run, 3);
    }

    #[test]
    fn listing_is_wanted_while_a_slot_is_free_or_once_stopped() {
        assert!(listing_wanted(false, 3, 4));
        assert!(!listing_wanted(false, 4, 4));
        assert!(listing_wanted(true, 4, 4));
    }
}
