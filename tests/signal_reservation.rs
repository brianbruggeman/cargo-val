#![cfg(unix)]

use cargo_val::build_defaults::SIGNAL_SLOTS;
use cargo_val::driver::{RunError, spawn_reserved};
use cargo_val::edge_signal_unix::{TABLE_FULL, abandon_slot, release_child, reserve_slot, track_child};
use cargo_val::edge_spawn_unix::{Child, spawn};
use proxima_primitives::sync::blocking::Mutex;

// the signal table and the child probe are process-wide, so every test in this file takes turns
static TURN: Mutex<()> = Mutex::new(());

fn hold_slots(count: usize) {
    (0..count).for_each(|_| reserve_slot().expect("a free slot admits the reservation"));
}

fn empty_table() {
    (0..SIGNAL_SLOTS).for_each(|_| abandon_slot());
}

fn unreaped_children() -> bool {
    let reaped = unsafe { libc::waitpid(-1, core::ptr::null_mut(), libc::WNOHANG) };
    reaped >= 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ECHILD)
}

fn spawn_true() -> Result<Child, RunError> {
    spawn(b"true", &[], &[], &[], None).map_err(|_| RunError::Spawn)
}

#[proxima::test(timeout = "60s")]
async fn a_full_signal_table_refuses_the_spawn_before_any_child_exists() {
    let _turn = TURN.lock();
    hold_slots(SIGNAL_SLOTS);
    let children_before = unreaped_children();

    let refused = spawn_reserved(spawn_true);
    let children_after = unreaped_children();
    empty_table();

    assert!(!children_before, "no child exists before the spawn is attempted");
    assert_eq!(refused.err(), Some(RunError::SignalSlots));
    assert!(!children_after, "a refused spawn must leave no child behind");
}

#[proxima::test(timeout = "60s")]
async fn a_free_signal_table_spawns_a_child_that_the_reservation_then_tracks() {
    let _turn = TURN.lock();

    let child = spawn_reserved(spawn_true).expect("a free slot admits the spawn");
    let process_id = child.process_id();
    let tracked = track_child(process_id);
    let children_after = unreaped_children();
    release_child(process_id);
    unsafe { libc::waitpid(process_id.cast_signed(), core::ptr::null_mut(), 0) };

    assert_eq!(tracked, Ok(()));
    assert!(children_after, "the probe must see the child the control spawn made");
}

#[proxima::test(timeout = "60s")]
async fn a_failed_spawn_returns_its_reservation_to_the_table() {
    let _turn = TURN.lock();
    hold_slots(SIGNAL_SLOTS - 1);

    let failed = spawn_reserved(|| spawn(b"cargo-val-no-such-program", &[], &[], &[], None).map_err(|_| RunError::Spawn));
    let reusable = reserve_slot();
    let exhausted = reserve_slot();
    empty_table();

    assert_eq!(failed.err(), Some(RunError::Spawn));
    assert_eq!(reusable, Ok(()));
    assert_eq!(exhausted, Err(TABLE_FULL));
}

#[proxima::test]
async fn an_abandoned_reservation_is_reusable() {
    let _turn = TURN.lock();
    hold_slots(SIGNAL_SLOTS);

    let refused = reserve_slot();
    abandon_slot();
    let admitted = reserve_slot();
    empty_table();

    assert_eq!(refused, Err(TABLE_FULL));
    assert_eq!(admitted, Ok(()));
}

#[proxima::test]
async fn tracking_a_child_needs_a_reservation() {
    let _turn = TURN.lock();

    assert_eq!(track_child(4242), Err(libc::EINVAL));
}
