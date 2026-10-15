//! Test executables whose package still has a `bin` target to build, held in arrival order.

use std::string::String;
use std::vec::Vec;

use crate::cargo_message::TestArtifact;

/// An owned copy of one [`TestArtifact`]; each field is sized from the cargo message, so a long workspace path costs nothing.
#[derive(Clone)]
pub struct OwnedArtifact {
    executable: String,
    manifest_path: String,
    package_id: String,
    target_name: String,
    target_kind: String,
}

impl OwnedArtifact {
    pub(crate) fn copy_of(artifact: &TestArtifact<'_>) -> Self {
        Self {
            executable: String::from(artifact.executable),
            manifest_path: String::from(artifact.manifest_path),
            package_id: String::from(artifact.package_id),
            target_name: String::from(artifact.target_name),
            target_kind: String::from(artifact.target_kind),
        }
    }

    /// The package id the held artifact belongs to.
    #[must_use]
    pub fn package_id(&self) -> &str {
        &self.package_id
    }

    /// Borrows the copy as the artifact it was made from.
    #[must_use]
    pub fn as_artifact(&self) -> TestArtifact<'_> {
        TestArtifact {
            executable: &self.executable,
            package_id: &self.package_id,
            manifest_path: &self.manifest_path,
            target_name: &self.target_name,
            target_kind: &self.target_kind,
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
    pub fn park(&mut self, artifact: &TestArtifact<'_>) {
        self.slots.push(OwnedArtifact::copy_of(artifact));
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
    use super::ParkedArtifacts;
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
            parked.park(&artifact(package, target));
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
    async fn hundreds_park_without_a_cap_and_a_field_of_any_length_is_kept_whole() {
        let mut parked = ParkedArtifacts::new();
        for _ in 0..300 {
            parked.park(&artifact("a#1", "t"));
        }
        assert_eq!(parked.len(), 300);
        let mut fresh = ParkedArtifacts::new();
        let long = "x".repeat(3000);
        fresh.park(&artifact(&long, "t"));
        let held = fresh.take_ready(|_| true).expect("the long id was parked");
        assert_eq!(held.package_id(), long);
    }
}
