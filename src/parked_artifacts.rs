//! Test executables whose package still has a `bin` target to build, held in arrival order.

use std::boxed::Box;
use std::vec;

use crate::cargo_message::TestArtifact;

const CAPACITY: usize = 64;
const PATH_CAPACITY: usize = 1024;
const ID_CAPACITY: usize = 256;
const NAME_CAPACITY: usize = 128;
const KIND_CAPACITY: usize = 16;

/// Why an artifact could not be parked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkFault {
    /// All 64 places are taken.
    Full,
    /// A path, id, name or kind passes its fixed capacity.
    FieldTooLong,
}

/// An owned copy of one [`TestArtifact`]; its fields live in fixed arrays.
#[derive(Clone, Copy)]
pub struct OwnedArtifact {
    executable: [u8; PATH_CAPACITY],
    executable_length: usize,
    manifest_path: [u8; PATH_CAPACITY],
    manifest_length: usize,
    package_id: [u8; ID_CAPACITY],
    package_length: usize,
    target_name: [u8; NAME_CAPACITY],
    target_name_length: usize,
    target_kind: [u8; KIND_CAPACITY],
    target_kind_length: usize,
}

fn store<const CAPACITY: usize>(
    destination: &mut [u8; CAPACITY],
    text: &str,
) -> Result<usize, ParkFault> {
    destination
        .get_mut(..text.len())
        .ok_or(ParkFault::FieldTooLong)?
        .copy_from_slice(text.as_bytes());
    Ok(text.len())
}

fn text(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("")
}

impl OwnedArtifact {
    const EMPTY: Self = Self {
        executable: [0; PATH_CAPACITY],
        executable_length: 0,
        manifest_path: [0; PATH_CAPACITY],
        manifest_length: 0,
        package_id: [0; ID_CAPACITY],
        package_length: 0,
        target_name: [0; NAME_CAPACITY],
        target_name_length: 0,
        target_kind: [0; KIND_CAPACITY],
        target_kind_length: 0,
    };

    fn copy_of(artifact: &TestArtifact<'_>) -> Result<Self, ParkFault> {
        let mut owned = Self::EMPTY;
        owned.executable_length = store(&mut owned.executable, artifact.executable)?;
        owned.manifest_length = store(&mut owned.manifest_path, artifact.manifest_path)?;
        owned.package_length = store(&mut owned.package_id, artifact.package_id)?;
        owned.target_name_length = store(&mut owned.target_name, artifact.target_name)?;
        owned.target_kind_length = store(&mut owned.target_kind, artifact.target_kind)?;
        Ok(owned)
    }

    /// The package id the held artifact belongs to.
    #[must_use]
    pub fn package_id(&self) -> &str {
        text(&self.package_id[..self.package_length])
    }

    /// Borrows the copy as the artifact it was made from.
    #[must_use]
    pub fn as_artifact(&self) -> TestArtifact<'_> {
        TestArtifact {
            executable: text(&self.executable[..self.executable_length]),
            package_id: self.package_id(),
            manifest_path: text(&self.manifest_path[..self.manifest_length]),
            target_name: text(&self.target_name[..self.target_name_length]),
            target_kind: text(&self.target_kind[..self.target_kind_length]),
        }
    }
}

/// A fixed first-in first-out of parked artifacts.
pub struct ParkedArtifacts {
    slots: Box<[OwnedArtifact]>,
    count: usize,
}

impl ParkedArtifacts {
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: vec![OwnedArtifact::EMPTY; CAPACITY].into_boxed_slice(),
            count: 0,
        }
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Holds a copy of `artifact` behind everything already parked.
    ///
    /// # Errors
    ///
    /// Returns [`ParkFault`] when the queue is full or a field passes its capacity.
    pub fn park(&mut self, artifact: &TestArtifact<'_>) -> Result<(), ParkFault> {
        if self.count == CAPACITY {
            return Err(ParkFault::Full);
        }
        self.slots[self.count] = OwnedArtifact::copy_of(artifact)?;
        self.count += 1;
        Ok(())
    }

    /// Removes and returns the earliest parked artifact whose package passes `ready`.
    pub fn take_ready(&mut self, ready: impl Fn(&str) -> bool) -> Option<OwnedArtifact> {
        let index = self.slots[..self.count]
            .iter()
            .position(|slot| ready(slot.package_id()))?;
        let taken = self.slots[index];
        self.slots.copy_within(index + 1..self.count, index);
        self.count -= 1;
        Some(taken)
    }
}

impl Default for ParkedArtifacts {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::boxed::Box;

    use super::{ParkFault, ParkedArtifacts};
    use crate::cargo_message::TestArtifact;

    fn artifact<'text>(package: &'text str, target: &'text str) -> TestArtifact<'text> {
        TestArtifact {
            executable: "/repo/target/debug/deps/same_name_targets-2612d669b7d847d4",
            package_id: package,
            manifest_path: "/repo/Cargo.toml",
            target_name: target,
            target_kind: "test",
        }
    }

    #[test]
    fn released_in_arrival_order_and_only_when_the_package_is_ready() {
        let mut parked = Box::new(ParkedArtifacts::new());
        for (package, target) in [("a#1", "one"), ("b#1", "two"), ("a#1", "three")] {
            parked.park(&artifact(package, target)).expect("room");
        }

        assert!(parked.take_ready(|package| package == "c#1").is_none());
        let first = parked.take_ready(|package| package == "a#1").expect("a ready");
        assert_eq!(first.as_artifact().target_name, "one");
        let second = parked.take_ready(|_| true).expect("anything ready");
        assert_eq!(second.as_artifact().target_name, "two");
        let third = parked.take_ready(|_| true).expect("last");
        assert_eq!(third.as_artifact().target_name, "three");
        assert!(parked.is_empty());
    }

    #[test]
    fn full_and_oversized_are_refused_not_dropped() {
        let mut parked = Box::new(ParkedArtifacts::new());
        for _ in 0..64 {
            parked.park(&artifact("a#1", "t")).expect("room");
        }
        assert_eq!(parked.park(&artifact("a#1", "t")), Err(ParkFault::Full));
        let mut fresh = Box::new(ParkedArtifacts::new());
        let long = "x".repeat(300);
        assert_eq!(fresh.park(&artifact(&long, "t")), Err(ParkFault::FieldTooLong));
        assert_eq!(fresh.len(), 0);
    }
}
