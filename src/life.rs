//! One unit of work's lifecycle as a consuming enum FSM.
//!
//! `In` is what the unit is queued with, `Live` what it holds while running, `Out` how it settles.
//! The degenerate form is `Life::running(live).settle(out)`: two states, no policy, no allocation.
//! Policies (deadline, fail-fast, interrupt) are sources that call `expire` or `cancel`; the executor
//! performs effects by matching the new variant once. Sans-IO and core-only.

use crate::failure_class::Exit;

/// Why a unit was cancelled: a signal number the process was interrupted with, or a fail-fast trip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Interrupted(i32),
    Tripped,
}

/// `Admitted::slot` is an index into the driver's live-session table.
#[must_use]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Life<In, Live, Out> {
    Queued { input: In },
    Admitted { input: In, slot: usize },
    Running { live: Live },
    Settled { out: Out },
    Cancelled { scope: Scope },
    Expired { limit_ms: u32, age_ms: u64, live: Live },
    Crashed { exit: Exit, live: Live },
}

impl<In, Live, Out> Life<In, Live, Out> {
    pub const fn running(live: Live) -> Self {
        Self::Running { live }
    }

    pub fn admit(self, slot: usize) -> Self {
        match self {
            Self::Queued { input } => Self::Admitted { input, slot },
            other => other,
        }
    }

    pub fn start(self, live: Live) -> Self {
        match self {
            Self::Admitted { .. } => Self::Running { live },
            other => other,
        }
    }

    pub fn settle(self, out: Out) -> Self {
        match self {
            Self::Running { .. } => Self::Settled { out },
            other => other,
        }
    }

    pub fn cancel(self, scope: Scope) -> Self {
        match self {
            Self::Queued { .. } | Self::Admitted { .. } | Self::Running { .. } => Self::Cancelled { scope },
            other => other,
        }
    }

    pub fn expire(self, limit_ms: u32, age_ms: u64) -> Self {
        match self {
            Self::Running { live } => Self::Expired { limit_ms, age_ms, live },
            other => other,
        }
    }

    pub fn crash(self, exit: Exit) -> Self {
        match self {
            Self::Running { live } => Self::Crashed { exit, live },
            other => other,
        }
    }

    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Settled { .. } | Self::Cancelled { .. } | Self::Expired { .. } | Self::Crashed { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_alloc::measure_allocations;

    type Unit = Life<u8, u16, u32>;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Kind {
        Queued,
        Admitted,
        Running,
        Settled,
        Cancelled,
        Expired,
        Crashed,
    }

    const KINDS: [Kind; 7] =
        [Kind::Queued, Kind::Admitted, Kind::Running, Kind::Settled, Kind::Cancelled, Kind::Expired, Kind::Crashed];

    fn kind(unit: &Unit) -> Kind {
        match unit {
            Life::Queued { .. } => Kind::Queued,
            Life::Admitted { .. } => Kind::Admitted,
            Life::Running { .. } => Kind::Running,
            Life::Settled { .. } => Kind::Settled,
            Life::Cancelled { .. } => Kind::Cancelled,
            Life::Expired { .. } => Kind::Expired,
            Life::Crashed { .. } => Kind::Crashed,
        }
    }

    fn make(kind: Kind) -> Unit {
        match kind {
            Kind::Queued => Life::Queued { input: 1 },
            Kind::Admitted => Life::Admitted { input: 1, slot: 3 },
            Kind::Running => Life::running(7),
            Kind::Settled => Life::Settled { out: 9 },
            Kind::Cancelled => Life::Cancelled { scope: Scope::Tripped },
            Kind::Expired => Life::Expired { limit_ms: 500, age_ms: 612, live: 7 },
            Kind::Crashed => Life::Crashed { exit: Exit::Signal(11), live: 7 },
        }
    }

    const TRANSITIONS: [&str; 6] = ["admit", "start", "settle", "cancel", "expire", "crash"];

    fn apply(unit: Unit, transition: &str) -> Unit {
        match transition {
            "admit" => unit.admit(5),
            "start" => unit.start(8),
            "settle" => unit.settle(10),
            "cancel" => unit.cancel(Scope::Interrupted(2)),
            "expire" => unit.expire(100, 150),
            _ => unit.crash(Exit::Code(101)),
        }
    }

    fn expected(from: Kind, transition: &str) -> Kind {
        match (from, transition) {
            (Kind::Queued, "admit") => Kind::Admitted,
            (Kind::Admitted, "start") => Kind::Running,
            (Kind::Running, "settle") => Kind::Settled,
            (Kind::Queued | Kind::Admitted | Kind::Running, "cancel") => Kind::Cancelled,
            (Kind::Running, "expire") => Kind::Expired,
            (Kind::Running, "crash") => Kind::Crashed,
            (same, _) => same,
        }
    }

    #[test]
    fn every_variant_and_transition_pair_lands_on_the_documented_variant() {
        let mut pairs = 0;
        for from in KINDS {
            for transition in TRANSITIONS {
                let after = apply(make(from), transition);
                assert_eq!(kind(&after), expected(from, transition), "{from:?} then {transition}");
                pairs += 1;
            }
        }
        assert_eq!(pairs, 42, "7 variants x 6 transitions");
    }

    #[test]
    fn an_ignored_transition_returns_the_same_value_untouched() {
        for from in KINDS {
            for transition in TRANSITIONS {
                if expected(from, transition) == from {
                    assert_eq!(apply(make(from), transition), make(from), "{from:?} then {transition}");
                }
            }
        }
    }

    #[test]
    fn terminals_absorb_and_the_first_cause_wins() {
        let cancelled = make(Kind::Cancelled).expire(1, 2).crash(Exit::Code(1)).settle(4);
        assert_eq!(cancelled, make(Kind::Cancelled));
        let expired = make(Kind::Expired).cancel(Scope::Tripped).crash(Exit::Code(1));
        assert_eq!(expired, make(Kind::Expired));
        assert!(expired.is_terminal() && !make(Kind::Running).is_terminal());
    }

    #[test]
    fn a_running_unit_carries_its_live_value_into_expired_and_crashed() {
        assert_eq!(make(Kind::Running).expire(500, 612), make(Kind::Expired));
        assert_eq!(make(Kind::Running).crash(Exit::Signal(11)), make(Kind::Crashed));
    }

    #[test]
    fn a_hundred_thousand_degenerate_units_visit_two_states_and_allocate_nothing() {
        const UNITS: usize = 100_000;
        let ((visited, settled), allocations) = measure_allocations(|| {
            let mut visited = 0_usize;
            let mut settled = 0_usize;
            for unit in 0..UNITS {
                let running: Life<(), (), usize> = Life::running(());
                visited += 1;
                let done = running.settle(unit);
                visited += 1;
                if matches!(done, Life::Settled { out } if out == unit) {
                    settled += 1;
                }
            }
            (visited, settled)
        });
        assert_eq!(allocations, 0, "degenerate path allocated");
        assert_eq!(visited, UNITS * 2, "two states per unit");
        assert_eq!(settled, UNITS, "every unit settled with its own output");
    }
}
