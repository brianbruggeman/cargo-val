use crate::schedule::{SessionSlotId, SessionSlots};

/// Terminal result reported by one completed test process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOutcome {
    /// The test process exited successfully.
    Passed,
    /// The test process reported a failure.
    Failed,
    /// The exact libtest child reported that this selected test was ignored.
    Ignored,
}

/// Bounded counts for discovered tests; live ownership stays in `SessionSlots`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutcomeCounts {
    /// Number of discovered test names.
    pub discovered: usize,
    /// Number excluded by the selection filter.
    pub filtered: usize,
    /// Number skipped by libtest.
    pub ignored: usize,
    /// Number of successful terminal results.
    pub passed: usize,
    /// Number of failed terminal results.
    pub failed: usize,
}

/// Errors returned when outcome counters overflow or a session is not live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeError {
    /// A fixed-width count reached its maximum value.
    CountOverflow,
    /// A terminal event refers to a stale, unknown, or already completed session.
    UnknownSession,
    /// The live-session table has no free slot.
    CapacityExhausted,
    /// A fresh ledger cannot take ownership while sessions are already live.
    ActiveSessions,
}

/// Failed insertion that returns ownership of the child to its caller.
#[derive(Debug, PartialEq, Eq)]
pub struct StartError<T> {
    /// Reason the child was not entered into the live-session table.
    pub error: OutcomeError,
    /// Child value remains available for caller cleanup or retry.
    pub child: T,
}

/// Outcome counters that operate on the coordinator's single live-session table.
pub struct OutcomeLedger<'sessions, T, const CAPACITY: usize> {
    live: &'sessions mut SessionSlots<T, CAPACITY>,
    counts: OutcomeCounts,
}

impl<'sessions, T, const CAPACITY: usize> OutcomeLedger<'sessions, T, CAPACITY> {
    /// Starts a new accounting period over an empty live-session table.
    ///
    /// # Errors
    ///
    /// Returns [`OutcomeError::ActiveSessions`] when the live table is not empty.
    pub fn new(live: &'sessions mut SessionSlots<T, CAPACITY>) -> Result<Self, OutcomeError> {
        if !live.is_empty() {
            return Err(OutcomeError::ActiveSessions);
        }
        Ok(Self {
            live,
            counts: OutcomeCounts {
                discovered: 0,
                filtered: 0,
                ignored: 0,
                passed: 0,
                failed: 0,
            },
        })
    }

    /// Records a discovered name rejected by the selection filter.
    ///
    /// # Errors
    ///
    /// Returns [`OutcomeError::CountOverflow`] when a counter would overflow.
    pub fn record_filtered(&mut self) -> Result<(), OutcomeError> {
        increment(&mut self.counts.discovered)?;
        increment(&mut self.counts.filtered)
    }

    /// Records a discovered name skipped by libtest.
    ///
    /// # Errors
    ///
    /// Returns [`OutcomeError::CountOverflow`] when a counter would overflow.
    pub fn record_ignored(&mut self) -> Result<(), OutcomeError> {
        increment(&mut self.counts.discovered)?;
        increment(&mut self.counts.ignored)
    }

    /// Inserts a selected child into the coordinator-owned live-session table.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] carrying the child back when a counter overflows or the live table is full.
    pub fn start(&mut self, child: T) -> Result<SessionSlotId, StartError<T>> {
        self.start_many(child, 1)
    }

    /// Inserts a child that runs `names` selected tests, counting each as discovered; each later reaches a terminal count through [`Self::count`].
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] carrying the child back when a counter overflows or the live table is full.
    pub fn start_many(&mut self, child: T, names: usize) -> Result<SessionSlotId, StartError<T>> {
        let Some(next_discovered) = self.counts.discovered.checked_add(names) else {
            return Err(StartError {
                error: OutcomeError::CountOverflow,
                child,
            });
        };
        let slot = match self.live.insert(child) {
            Ok(slot) => slot,
            Err(child) => {
                return Err(StartError {
                    error: OutcomeError::CapacityExhausted,
                    child,
                });
            }
        };
        self.counts.discovered = next_discovered;
        Ok(slot)
    }

    /// Lends the live table read-only for Proxima [`HostEventPipe`] validation.
    ///
    /// Use [`crate::host_event::HostEventPipe`] to validate readiness events while this ledger
    /// owns lifecycle mutation. `SessionSlots` remains the one generation
    /// checked store; this view cannot insert or retire children.
    #[must_use]
    pub const fn sessions(&self) -> &SessionSlots<T, CAPACITY> {
        self.live
    }

    /// Mutates one live child's lifecycle state without changing table ownership.
    pub fn session_mut(&mut self, slot: SessionSlotId) -> Option<&mut T> {
        self.live.get_mut(slot)
    }

    /// Checks whether one terminal category can be committed for a live child.
    ///
    /// # Errors
    ///
    /// Returns [`OutcomeError::UnknownSession`] for a stale slot, or [`OutcomeError::CountOverflow`] when the category counter would overflow.
    pub fn can_finish(
        &self,
        slot: SessionSlotId,
        outcome: TerminalOutcome,
    ) -> Result<(), OutcomeError> {
        if self.live.get(slot).is_none() {
            return Err(OutcomeError::UnknownSession);
        }
        let count = match outcome {
            TerminalOutcome::Passed => self.counts.passed,
            TerminalOutcome::Failed => self.counts.failed,
            TerminalOutcome::Ignored => self.counts.ignored,
        };
        count.checked_add(1).ok_or(OutcomeError::CountOverflow)?;
        Ok(())
    }

    /// Counts one terminal result and retires that child from the shared table.
    ///
    /// # Errors
    ///
    /// Returns [`OutcomeError::UnknownSession`] for a stale slot, or [`OutcomeError::CountOverflow`] when the category counter would overflow.
    pub fn finish(
        &mut self,
        slot: SessionSlotId,
        outcome: TerminalOutcome,
    ) -> Result<T, OutcomeError> {
        self.can_finish(slot, outcome)?;
        let count = match outcome {
            TerminalOutcome::Passed => &mut self.counts.passed,
            TerminalOutcome::Failed => &mut self.counts.failed,
            TerminalOutcome::Ignored => &mut self.counts.ignored,
        };
        let next = count.checked_add(1).ok_or(OutcomeError::CountOverflow)?;
        let child = self.live.remove(slot).ok_or(OutcomeError::UnknownSession)?;
        *count = next;
        Ok(child)
    }

    /// Counts one terminal result for a test whose child keeps running other tests.
    ///
    /// # Errors
    ///
    /// Returns [`OutcomeError::CountOverflow`] when the category counter would overflow.
    pub fn count(&mut self, outcome: TerminalOutcome) -> Result<(), OutcomeError> {
        let count = match outcome {
            TerminalOutcome::Passed => &mut self.counts.passed,
            TerminalOutcome::Failed => &mut self.counts.failed,
            TerminalOutcome::Ignored => &mut self.counts.ignored,
        };
        increment(count)
    }

    /// Retires a child whose tests were counted one by one.
    pub fn retire(&mut self, slot: SessionSlotId) -> Option<T> {
        self.live.remove(slot)
    }

    /// Takes `names` back out of the discovered count: tests that were never run, or that will be counted again by a relaunch.
    ///
    /// # Errors
    ///
    /// Returns [`OutcomeError::CountOverflow`] when fewer names than that are unaccounted.
    pub fn withdraw(&mut self, names: usize) -> Result<(), OutcomeError> {
        if names > self.unaccounted() {
            return Err(OutcomeError::CountOverflow);
        }
        self.counts.discovered -= names;
        Ok(())
    }

    /// Returns the current category counts and remaining unaccounted names.
    #[must_use]
    pub const fn snapshot(&self) -> OutcomeCounts {
        self.counts
    }

    /// Returns discovered names that do not yet have a terminal category.
    #[must_use]
    pub const fn unaccounted(&self) -> usize {
        self.counts.discovered
            - self.counts.filtered
            - self.counts.ignored
            - self.counts.passed
            - self.counts.failed
    }
}

fn increment(count: &mut usize) -> Result<(), OutcomeError> {
    *count = count.checked_add(1).ok_or(OutcomeError::CountOverflow)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{OutcomeError, OutcomeLedger, StartError, TerminalOutcome};
    use crate::schedule::SessionSlots;

    #[proxima::test]
    async fn c48h_accounts_outcomes_in_the_coordinator_owned_session_table() {
        let (result, allocations) = crate::test_alloc::measure_allocations(|| {
            let mut live = SessionSlots::<u8, 3>::new();
            let mut ledger = OutcomeLedger::new(&mut live).expect("table starts empty");
            ledger.record_filtered().expect("filtered count increments");
            ledger.record_ignored().expect("ignored count increments");
            let passed_one = ledger.start(10).expect("first slot is available");
            let passed_two = ledger.start(11).expect("second slot is available");
            let failed = ledger.start(12).expect("third slot is available");
            assert_eq!(
                ledger
                    .finish(passed_one, TerminalOutcome::Passed)
                    .expect("first pass is accounted and retired"),
                10
            );
            assert_eq!(
                ledger
                    .finish(passed_two, TerminalOutcome::Passed)
                    .expect("second pass is accounted and retired"),
                11
            );
            assert_eq!(
                ledger
                    .finish(failed, TerminalOutcome::Failed)
                    .expect("failure is accounted and retired"),
                12
            );
            let duplicate = ledger.finish(failed, TerminalOutcome::Failed);
            (
                ledger.snapshot(),
                ledger.unaccounted(),
                duplicate,
                ledger.live.len(),
            )
        });
        let (counts, unaccounted, duplicate, live_sessions) = result;

        assert_eq!(counts.discovered, 5);
        assert_eq!(counts.filtered, 1);
        assert_eq!(counts.ignored, 1);
        assert_eq!(counts.passed, 2);
        assert_eq!(counts.failed, 1);
        assert_eq!(unaccounted, 0);
        assert_eq!(duplicate, Err(OutcomeError::UnknownSession));
        assert_eq!(live_sessions, 0);
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"discovered\":5,\"filtered\":1,\"ignored\":1,\"passed\":2,\"failed\":1,\"unaccounted\":0,\"duplicate_rejected\":1,\"live_session_tables\":1,\"live_sessions\":0,\"allocations\":0}}"
        );
    }

    #[proxima::test]
    async fn c48h_rejects_a_stale_generation_without_changing_counts() {
        let (result, allocations) = crate::test_alloc::measure_allocations(|| {
            let mut live = SessionSlots::<u8, 1>::new();
            let mut ledger = OutcomeLedger::new(&mut live).expect("table starts empty");
            let stale = ledger.start(7).expect("slot is available");
            ledger
                .live
                .remove(stale)
                .expect("fixture retires generation zero");
            let active = ledger.start(8).expect("slot is reused");
            let before = ledger.snapshot();
            let stale_finish = ledger.finish(stale, TerminalOutcome::Failed);
            (
                stale_finish,
                before,
                ledger.snapshot(),
                ledger.live.get(active).copied(),
            )
        });
        let (stale_finish, before, after, active_value) = result;

        assert_eq!(stale_finish, Err(OutcomeError::UnknownSession));
        assert_eq!(before, after);
        assert_eq!(active_value, Some(8));
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"stale_generation_finishes_counted\":0,\"active_generation_retained\":1,\"allocations\":{allocations}}}"
        );
    }

    #[proxima::test]
    async fn c48h_rejects_replacing_ledger_while_a_child_is_live() {
        let mut live = SessionSlots::<u8, 1>::new();
        let slot = {
            let mut ledger = OutcomeLedger::new(&mut live).expect("table starts empty");
            ledger.start(9).expect("slot is available")
        };

        assert!(matches!(
            OutcomeLedger::new(&mut live),
            Err(OutcomeError::ActiveSessions)
        ));
        assert_eq!(live.get(slot), Some(&9));
    }

    #[proxima::test]
    async fn c48h_returns_child_when_the_live_table_is_full() {
        let mut live = SessionSlots::<u8, 1>::new();
        let mut ledger = OutcomeLedger::new(&mut live).expect("table starts empty");
        ledger.start(4).expect("first child owns the only slot");

        let failure = ledger
            .start(5)
            .expect_err("second child cannot enter a full table");
        assert_eq!(
            failure,
            StartError {
                error: OutcomeError::CapacityExhausted,
                child: 5
            }
        );
        assert_eq!(ledger.snapshot().discovered, 1);
        assert_eq!(ledger.live.len(), 1);
        std::println!(
            "{{\"capacity_rejection\":1,\"returned_child\":{},\"discovered_count\":{},\"live_sessions\":{}}}",
            failure.child,
            ledger.snapshot().discovered,
            ledger.live.len()
        );
    }

    #[proxima::test]
    async fn c48h_returns_child_when_discovered_count_overflows() {
        let mut live = SessionSlots::<u8, 1>::new();
        let mut ledger = OutcomeLedger::new(&mut live).expect("table starts empty");
        ledger.counts.discovered = usize::MAX;

        let failure = ledger
            .start(6)
            .expect_err("discovered count cannot advance past usize::MAX");
        assert_eq!(
            failure,
            StartError {
                error: OutcomeError::CountOverflow,
                child: 6
            }
        );
        assert_eq!(ledger.snapshot().discovered, usize::MAX);
        assert!(ledger.live.is_empty());
        std::println!(
            "{{\"count_overflow_rejection\":1,\"returned_child\":{},\"discovered_count_unchanged\":\"usize::MAX\",\"live_sessions\":{}}}",
            failure.child,
            ledger.live.len()
        );
    }
}
