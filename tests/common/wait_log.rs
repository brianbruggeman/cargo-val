use core::cell::RefCell;
use core::future::Future;
use core::time::Duration;
use std::sync::OnceLock;
use std::time::Instant;

use futures::FutureExt;
use proxima_core::time::timeout;

pub mod lock_report {
    include!("lock_report.rs");
}

pub type Log = RefCell<Vec<String>>;

/// One json line per wait boundary, in the c74 event shape, so a run that outlives its budget names the wait it was in.
pub fn stamp(log: &Log, wait: &str, phase: &str) {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let elapsed_ms = ORIGIN.get_or_init(Instant::now).elapsed().as_millis();
    log.borrow_mut().push(format!("{{\"event\":\"c74_wait\",\"wait\":\"{wait}\",\"phase\":\"{phase}\",\"elapsed_ms\":{elapsed_ms}}}"));
}

fn payload_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload.downcast_ref::<&str>().map(|text| (*text).to_owned()).or_else(|| payload.downcast_ref::<String>().cloned()).unwrap_or_else(|| "a non-text panic payload".to_owned())
}

/// Runs `body` under `budget`; the harness keeps only the panic message of the thread it armed, so a panic or a missed budget carries its text and the wait log out here.
pub async fn guarded<Outcome>(label: &str, (log, budget): (&Log, Duration), body: impl Future<Output = Outcome>) -> Outcome {
    match timeout(budget, std::panic::AssertUnwindSafe(body).catch_unwind()).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(payload)) => panic!("{label} panicked: {}\n{}", payload_text(payload.as_ref()), log.borrow().join("\n")),
        Err(elapsed) => panic!("{label} outlived {budget:?} ({elapsed:?}); the last stamp without its exit names the wait:\n{}\nlock holders at the budget:\n{}", log.borrow().join("\n"), lock_report::lock_holders(None, None)),
    }
}
