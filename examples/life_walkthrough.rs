//! Drives `Life` through every legal transition path and prints each step.

use cargo_val::failure_class::Exit;
use cargo_val::life::{Life, Scope};

type Unit = Life<&'static str, u32, &'static str>;

fn step(path: &str, label: &str, unit: &Unit) {
    println!("{path:<12} {label:<22} -> {unit:?}");
}

fn walk(path: &str, steps: impl FnOnce(&str) -> Unit) {
    let end = steps(path);
    println!("{path:<12} terminal={}", end.is_terminal());
}

fn main() {
    walk("settle", |path| {
        let queued: Unit = Life::Queued { input: "tests::alpha" };
        step(path, "queued", &queued);
        let admitted = queued.admit(3);
        step(path, "admit(3)", &admitted);
        let running = admitted.start(4242);
        step(path, "start(4242)", &running);
        let settled = running.settle("passed");
        step(path, "settle", &settled);
        settled
    });
    walk("degenerate", |path| {
        let running: Unit = Life::running(1);
        step(path, "running", &running);
        let settled = running.settle("passed");
        step(path, "settle", &settled);
        settled
    });
    walk("expire", |path| {
        let expired = Unit::running(4242).expire(500, 612);
        step(path, "expire(500, 612)", &expired);
        expired
    });
    walk("crash", |path| {
        let crashed = Unit::running(4242).crash(Exit::Signal(11));
        step(path, "crash(SIGSEGV)", &crashed);
        crashed
    });
    walk("tripped", |path| {
        let cancelled = Unit::Queued { input: "tests::beta" }.cancel(Scope::Tripped);
        step(path, "cancel(Tripped)", &cancelled);
        cancelled
    });
    walk("interrupted", |path| {
        let cancelled = Unit::running(4242).cancel(Scope::Interrupted(2));
        step(path, "cancel(Interrupted)", &cancelled);
        cancelled
    });
    walk("absorb", |path| {
        let expired = Unit::running(4242).expire(500, 612);
        let after = expired.cancel(Scope::Tripped).crash(Exit::Code(1));
        step(path, "cancel+crash ignored", &after);
        after
    });
}
