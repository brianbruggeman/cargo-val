use crate::session_slots::{SessionSlotId, SessionSlots};

/// Terminal result reported by one completed test process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOutcome {
    /// The test process exited successfully.
    Passed,
    /// The test process reported a failure.
    Failed,
}

/// Bounded accounting for discovered tests and directly owned live sessions.
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
}

/// Fixed-capacity outcome FSM; its owner supplies concurrency through slots.
pub struct OutcomeLedger<const CAPACITY: usize> {
    counts: OutcomeCounts,
    live: SessionSlots<(), CAPACITY>,
}

impl<const CAPACITY: usize> OutcomeLedger<CAPACITY> {
    /// Creates empty counts and inline storage for live child sessions.
    #[must_use]
    pub fn new() -> Self {
        Self {
            counts: OutcomeCounts {
                discovered: 0,
                filtered: 0,
                ignored: 0,
                passed: 0,
                failed: 0,
            },
            live: SessionSlots::new(),
        }
    }

    /// Records a discovered name rejected by the selection filter.
    pub fn record_filtered(&mut self) -> Result<(), OutcomeError> {
        increment(&mut self.counts.discovered)?;
        increment(&mut self.counts.filtered)
    }

    /// Records a discovered name skipped by libtest.
    pub fn record_ignored(&mut self) -> Result<(), OutcomeError> {
        increment(&mut self.counts.discovered)?;
        increment(&mut self.counts.ignored)
    }

    /// Records a selected test and gives its child direct ownership of a slot.
    pub fn start(&mut self) -> Result<SessionSlotId, OutcomeError> {
        let slot = self
            .live
            .insert(())
            .map_err(|_| OutcomeError::CapacityExhausted)?;
        if let Err(error) = increment(&mut self.counts.discovered) {
            let _ = self.live.remove(slot);
            return Err(error);
        }
        Ok(slot)
    }

    /// Accepts exactly one terminal event for a live child session.
    pub fn finish(
        &mut self,
        slot: SessionSlotId,
        outcome: TerminalOutcome,
    ) -> Result<(), OutcomeError> {
        let count = match outcome {
            TerminalOutcome::Passed => &mut self.counts.passed,
            TerminalOutcome::Failed => &mut self.counts.failed,
        };
        let next = count.checked_add(1).ok_or(OutcomeError::CountOverflow)?;
        if self.live.remove(slot).is_none() {
            return Err(OutcomeError::UnknownSession);
        }
        *count = next;
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

impl<const CAPACITY: usize> Default for OutcomeLedger<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

fn increment(count: &mut usize) -> Result<(), OutcomeError> {
    *count = count.checked_add(1).ok_or(OutcomeError::CountOverflow)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{OutcomeError, OutcomeLedger, TerminalOutcome};

    #[test]
    fn accounts_every_discovered_test_and_rejects_duplicate_terminal_event() {
        let (result, allocations) = crate::dispatch_stream::tests::measure_allocations(|| {
            let mut ledger = OutcomeLedger::<3>::new();
            ledger.record_filtered().expect("filtered count increments");
            ledger.record_ignored().expect("ignored count increments");
            let passed_one = ledger.start().expect("first live slot is available");
            let passed_two = ledger.start().expect("second live slot is available");
            let failed = ledger.start().expect("third live slot is available");
            ledger
                .finish(passed_one, TerminalOutcome::Passed)
                .expect("first pass is accounted");
            ledger
                .finish(passed_two, TerminalOutcome::Passed)
                .expect("second pass is accounted");
            ledger
                .finish(failed, TerminalOutcome::Failed)
                .expect("failure is accounted");
            let duplicate = ledger.finish(failed, TerminalOutcome::Failed);
            (ledger.snapshot(), ledger.unaccounted(), duplicate)
        });
        let (counts, unaccounted, duplicate) = result;

        assert_eq!(counts.discovered, 5);
        assert_eq!(counts.filtered, 1);
        assert_eq!(counts.ignored, 1);
        assert_eq!(counts.passed, 2);
        assert_eq!(counts.failed, 1);
        assert_eq!(unaccounted, 0);
        assert_eq!(duplicate, Err(OutcomeError::UnknownSession));
        assert_eq!(allocations, 0);
        std::println!(
            "{{\"discovered\":5,\"filtered\":1,\"ignored\":1,\"passed\":2,\"failed\":1,\"unaccounted\":0,\"duplicate_rejected\":1,\"allocations\":0}}"
        );
    }
}
