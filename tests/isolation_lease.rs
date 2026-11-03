use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Barrier;
use std::thread;

#[path = "common/isolation.rs"]
mod isolation;

const CONCURRENT_RUNS: usize = 6;

fn lease_on_a_thread(label: &'static str) -> [(&'static str, PathBuf); 2] {
    thread::spawn(move || isolation::environment(label)).join().expect("the leasing thread finishes")
}

#[test]
fn runs_held_at_the_same_time_never_share_a_target_or_a_cargo_home() {
    let barrier = Barrier::new(CONCURRENT_RUNS);

    let leased: Vec<[(&'static str, PathBuf); 2]> = thread::scope(|scope| {
        let handles: Vec<_> = (0..CONCURRENT_RUNS)
            .map(|_| {
                scope.spawn(|| {
                    let environment = isolation::environment("lease-concurrent");
                    barrier.wait();
                    environment
                })
            })
            .collect();
        handles.into_iter().map(|handle| handle.join().expect("the leasing thread finishes")).collect()
    });

    let targets: HashSet<_> = leased.iter().map(|environment| environment[0].1.clone()).collect();
    let homes: HashSet<_> = leased.iter().map(|environment| environment[1].1.clone()).collect();
    assert_eq!((targets.len(), homes.len()), (CONCURRENT_RUNS, CONCURRENT_RUNS), "targets {targets:?} homes {homes:?}");
}

#[test]
fn a_run_released_by_a_finished_test_is_leased_warm_by_the_next_of_the_same_label() {
    let first = lease_on_a_thread("lease-sequential");

    let second = lease_on_a_thread("lease-sequential");

    assert_eq!(first, second, "the released run is taken before a new one is made");
}

#[test]
fn two_labels_never_share_a_cargo_home_or_a_target() {
    let alpha = lease_on_a_thread("lease-label-alpha");

    let beta = lease_on_a_thread("lease-label-beta");

    assert_ne!(alpha[0].1, beta[0].1);
    assert_ne!(alpha[1].1, beta[1].1);
}
