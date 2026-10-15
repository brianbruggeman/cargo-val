//! Who a deadline-killed group was blocked behind, read before the kill.
//!
//! A cargo file lock has a holder: a pid that took the lock and keeps it until it exits. The direction of the wait
//! comes from the child itself: cargo prints "Blocking waiting for file lock on <which>" and
//! [`LockWatch`](crate::isolated_target::LockWatch) turns that line into a [`LockKind`]. The holder comes from two
//! `lsof` reads: the lock files of that kind a member of the group has open, then the processes outside the group that
//! have each of them open. [`at_deadline`] builds the [`Exit`] the classifier reads; it must run before the group is
//! signalled, because the waiter and its open files are gone afterwards.
//!
//! `lsof` marks a process holding a flock in its lock field where the platform reports it (Linux reads `/proc/locks`);
//! macOS reports no lock state for `flock`, so there the holder is named only when exactly one process outside the
//! group has the file open. Anything else, and every platform where `lsof` does not run, answers `None`: a silent
//! kill stays a [`Class::Hang`](crate::failure_class::Class::Hang) rather than naming a pid the data does not support.

use std::borrow::ToOwned;
use std::path::Path;
use std::process::Command;
use std::string::{String, ToString};
use std::vec::Vec;

use crate::failure_class::Exit;
use crate::isolated_target::LockKind;

const LOCK_FILES: [(&str, LockKind); 5] = [
    (".cargo-lock", LockKind::BuildDirectory),
    (".cargo-build-lock", LockKind::BuildDirectory),
    (".cargo-artifact-lock", LockKind::Other),
    (".package-cache", LockKind::PackageCache),
    (".package-cache-mutate", LockKind::PackageCache),
];

const HELD: &str = "rRwWu";

/// The exit of a group killed at its deadline. `announced` is the lock the group's own stderr said it was waiting for; the exit names a holder only when a process outside `group` (the leader's pid) is found holding that kind of lock.
#[must_use]
pub fn at_deadline(group: u32, announced: Option<LockKind>) -> Exit {
    Exit::DeadlineKilled { waiting: announced.and_then(|kind| holder_of(group, kind).map(|holder| (kind, holder))) }
}

fn holder_of(group: u32, kind: LockKind) -> Option<u32> {
    let members = group_members(&run("ps", &["-axo", "pgid=,pid="]), group);
    let opened = run("lsof", &["-nP", "-Fn", "-p", &join(&members)]);
    lock_files(&opened, kind).into_iter().find_map(|file| holder_outside(&openers(&run("lsof", &["-nP", "-Fpl", "--", &file])), &members))
}

fn join(pids: &[u32]) -> String {
    pids.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")
}

// lsof exits 1 when nothing matches, so only stdout is read; a program that cannot run answers nothing
fn run(program: &str, arguments: &[&str]) -> String {
    Command::new(program).args(arguments).output().map(|output| String::from_utf8_lossy(&output.stdout).into_owned()).unwrap_or_default()
}

fn group_members(listing: &str, group: u32) -> Vec<u32> {
    listing
        .lines()
        .filter_map(|line| {
            let mut columns = line.split_whitespace();
            let leader: u32 = columns.next()?.parse().ok()?;
            let pid: u32 = columns.next()?.parse().ok()?;
            (leader == group).then_some(pid)
        })
        .collect()
}

fn lock_files(listing: &str, kind: LockKind) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for path in listing.lines().filter_map(|line| line.strip_prefix('n')) {
        let name = Path::new(path).file_name().and_then(|name| name.to_str());
        let ours = LOCK_FILES.iter().any(|(lock, lock_kind)| Some(*lock) == name && *lock_kind == kind);
        if ours && !found.iter().any(|seen| seen == path) {
            found.push(path.to_owned());
        }
    }
    found
}

// one entry per process with the file open; the flag is lsof's lock field saying the process holds a lock on it
fn openers(listing: &str) -> Vec<(u32, bool)> {
    let mut found: Vec<(u32, bool)> = Vec::new();
    for line in listing.lines() {
        if let Some(pid) = line.strip_prefix('p').and_then(|pid| pid.parse::<u32>().ok()) {
            found.push((pid, false));
        } else if let (Some(state), Some(last)) = (line.strip_prefix('l'), found.last_mut()) {
            last.1 |= state.chars().next().is_some_and(|mark| HELD.contains(mark));
        }
    }
    found
}

fn holder_outside(openers: &[(u32, bool)], members: &[u32]) -> Option<u32> {
    let outside = |(pid, _): &&(u32, bool)| !members.contains(pid);
    let marked_inside = openers.iter().any(|(pid, held)| *held && members.contains(pid));
    if openers.iter().any(|(_, held)| *held) {
        return openers.iter().filter(outside).find(|(_, held)| *held).map(|(pid, _)| *pid).filter(|_| !marked_inside);
    }
    let mut unmarked = openers.iter().filter(outside);
    match (unmarked.next(), unmarked.next()) {
        (Some((pid, _)), None) => Some(*pid),
        _ => None,
    }
}
