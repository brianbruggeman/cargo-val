//! Test executables whose package still has a `bin` target to build, held in arrival order.

use std::vec::Vec;

use crate::cargo_message::TestArtifact;

const PATH_CAPACITY: usize = 1024;
const ID_CAPACITY: usize = 256;
const NAME_CAPACITY: usize = 128;
const KIND_CAPACITY: usize = 16;

/// Why an artifact could not be parked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkFault {
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

    pub(crate) fn copy_of(artifact: &TestArtifact<'_>) -> Result<Self, ParkFault> {
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

/// A first-in first-out of parked artifacts that grows with what cargo emits, so holding one never costs cargo-val the right to read the message that releases it.
pub struct ParkedArtifacts {
    slots: Vec<OwnedArtifact>,
}

impl ParkedArtifacts {
    #[must_use]
    pub const fn new() -> Self {
        Self { slots: Vec::new() }
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.slots.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Holds a copy of `artifact` behind everything already parked.
    ///
    /// # Errors
    ///
    /// Returns [`ParkFault`] when a field passes its capacity.
    pub fn park(&mut self, artifact: &TestArtifact<'_>) -> Result<(), ParkFault> {
        self.slots.push(OwnedArtifact::copy_of(artifact)?);
        Ok(())
    }

    /// Removes and returns the earliest parked artifact whose package passes `ready`.
    pub fn take_ready(&mut self, ready: impl Fn(&str) -> bool) -> Option<OwnedArtifact> {
        let index = self.slots.iter().position(|slot| ready(slot.package_id()))?;
        Some(self.slots.remove(index))
    }
}

impl Default for ParkedArtifacts {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
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

    #[proxima::test]
    async fn released_in_arrival_order_and_only_when_the_package_is_ready() {
        let mut parked = ParkedArtifacts::new();
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

    #[proxima::test]
    async fn hundreds_park_without_a_cap_and_an_oversized_field_is_refused_not_dropped() {
        let mut parked = ParkedArtifacts::new();
        for _ in 0..300 {
            parked.park(&artifact("a#1", "t")).expect("parking never runs out of places");
        }
        assert_eq!(parked.len(), 300);
        let mut fresh = ParkedArtifacts::new();
        let long = "x".repeat(300);
        assert_eq!(fresh.park(&artifact(&long, "t")), Err(ParkFault::FieldTooLong));
        assert_eq!(fresh.len(), 0);
    }
}
