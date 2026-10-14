#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    const PEERS: usize = 12;
    const DEADLINE: Duration = Duration::from_secs(90);

    // returns only once every peer process has announced itself, so a child killed at spawn leaves the rest waiting until the deadline
    fn rendezvous(name: &str) {
        let Some(directory) = std::env::var_os("CARGO_VAL_FIXTURE_RENDEZVOUS_DIR") else {
            return;
        };
        let directory = PathBuf::from(directory);
        File::create(directory.join(name)).expect("the rendezvous directory accepts an arrival marker");
        let started = Instant::now();
        while fs::read_dir(&directory).expect("the rendezvous directory is readable").count() < PEERS {
            assert!(started.elapsed() < DEADLINE, "{name} saw fewer than {PEERS} live peers");
            std::thread::yield_now();
        }
        println!("{name} saw {PEERS} live peers");
    }

    #[test]
    fn peer_01() {
        rendezvous("peer_01");
    }

    #[test]
    fn peer_02() {
        rendezvous("peer_02");
    }

    #[test]
    fn peer_03() {
        rendezvous("peer_03");
    }

    #[test]
    fn peer_04() {
        rendezvous("peer_04");
    }

    #[test]
    fn peer_05() {
        rendezvous("peer_05");
    }

    #[test]
    fn peer_06() {
        rendezvous("peer_06");
    }

    #[test]
    fn peer_07() {
        rendezvous("peer_07");
    }

    #[test]
    fn peer_08() {
        rendezvous("peer_08");
    }

    #[test]
    fn peer_09() {
        rendezvous("peer_09");
    }

    #[test]
    fn peer_10() {
        rendezvous("peer_10");
    }

    #[test]
    fn peer_11() {
        rendezvous("peer_11");
    }

    #[test]
    fn peer_12() {
        rendezvous("peer_12");
    }
}
