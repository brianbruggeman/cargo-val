#[cfg(test)]
mod suite {
    use std::io::Write;

    fn record_process() {
        let Some(path) = std::env::var_os("CARGO_VAL_SHARD_LOG") else {
            return;
        };
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("shard log opens for append");
        log.write_all(format!("{}\n", std::process::id()).as_bytes())
            .expect("shard log takes the pid");
    }

    #[test]
    fn a_passes() {
        record_process();
    }

    #[test]
    fn b_aborts_the_process() {
        record_process();
        if std::env::var_os("CARGO_VAL_SHARD_ABORT").is_some() {
            std::process::abort();
        }
    }

    #[test]
    fn c_passes_after_the_crash() {
        record_process();
    }

    #[test]
    fn d_panics_on_request() {
        record_process();
        if std::env::var_os("CARGO_VAL_SHARD_FAIL").is_some() {
            panic!("requested shard failure in d");
        }
    }

    #[test]
    fn e_passes_last() {
        record_process();
        #[cfg(unix)]
        if std::env::var_os("CARGO_VAL_SHARD_IGNORE_INT").is_some() {
            unsafe extern "C" {
                fn signal(signal: i32, handler: usize) -> usize;
            }
            const SIGINT: i32 = 2;
            const SIG_IGN: usize = 1;
            unsafe { signal(SIGINT, SIG_IGN) };
        }
        if let Some(path) = std::env::var_os("CARGO_VAL_SHARD_READY") {
            let mut ready = std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .expect("the harness opened the ready fifo for reading");
            ready
                .write_all(format!("{}\n", std::process::id()).as_bytes())
                .expect("the ready fifo accepts the process id");
        }
        if let Some(path) = std::env::var_os("CARGO_VAL_SHARD_HOLD") {
            let _ = std::fs::File::open(path);
        }
    }

    #[test]
    #[ignore = "skipped by default"]
    fn f_ignored_by_default() {
        panic!("ignored fixture body should not run");
    }
}
