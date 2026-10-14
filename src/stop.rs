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

/// The counters a fail-fast stop reports; the trip itself is a signal the driver fires and reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopPolicy {
    max_fail: Option<usize>,
    at_trip: Option<(usize, DocAtTrip)>,
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
            at_trip: None,
            launched_after_trip: 0,
            listed_not_run: 0,
            targets_not_listed: 0,
            targets_listed: 0,
        }
    }

    pub const fn record_trip(&mut self, in_flight: usize, doc: DocAtTrip) {
        if self.at_trip.is_none() {
            self.at_trip = Some((in_flight, doc));
        }
    }

    pub const fn launched(&mut self) {
        self.launched_after_trip += 1;
    }

    pub const fn listed_test_skipped(&mut self) {
        self.listed_not_run += 1;
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
        let (Some(max_fail), Some((in_flight, doc))) = (self.max_fail, self.at_trip) else {
            return None;
        };
        Some(StopReport {
            max_fail,
            in_flight_at_trip: in_flight,
            launched_after_trip: self.launched_after_trip,
            listed_not_run: self.listed_not_run,
            targets_not_listed: self.targets_not_listed,
            doc_at_trip: doc,
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

    #[proxima::test]
    async fn report_is_none_before_a_trip_is_recorded() {
        let mut policy = StopPolicy::new(Some(2));
        policy.launched();
        policy.target_skipped();

        assert_eq!(policy.report(), None);
    }

    #[proxima::test]
    async fn report_is_none_without_max_fail_even_after_a_recorded_trip() {
        let mut policy = StopPolicy::new(None);
        policy.record_trip(1, DocAtTrip::NotSpawned);

        assert_eq!(policy.report(), None);
    }

    #[proxima::test]
    async fn records_in_flight_once() {
        let mut policy = StopPolicy::new(Some(1));

        policy.record_trip(4, DocAtTrip::NotSpawned);
        policy.record_trip(9, DocAtTrip::AlreadyFinished);

        let report = policy.report().expect("tripped policy reports");
        assert_eq!(report.in_flight_at_trip, 4);
        assert_eq!(report.doc_at_trip, DocAtTrip::NotSpawned);
    }

    #[proxima::test]
    async fn report_matches_worked_sequence() {
        let mut policy = StopPolicy::new(Some(2));
        assert_eq!(policy.target_listed(), 0);
        assert_eq!(policy.target_listed(), 1);
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

    #[proxima::test]
    async fn listing_is_wanted_while_a_slot_is_free_or_once_stopped() {
        assert!(listing_wanted(false, 3, 4));
        assert!(!listing_wanted(false, 4, 4));
        assert!(listing_wanted(true, 4, 4));
    }
}
