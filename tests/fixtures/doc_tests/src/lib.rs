/// Adds two counters.
///
/// ```
/// assert_eq!(cargo_val_doc_fixture::add(2, 3), 5);
/// ```
pub fn add(left: u64, right: u64) -> u64 {
    left + right
}

/// Subtracts and is documented incorrectly on purpose.
///
/// ```
/// assert_eq!(cargo_val_doc_fixture::subtract(5, 3), 3);
/// ```
pub fn subtract(left: u64, right: u64) -> u64 {
    left - right
}

/// Needs hardware the fixture never has.
///
/// ```ignore
/// cargo_val_doc_fixture::needs_device();
/// ```
pub fn needs_device() {}

#[cfg(test)]
mod tests {
    #[test]
    fn unit_adds() {
        let failing = std::env::var("CARGO_VAL_FIXTURE_FAILING").unwrap_or_default();
        assert!(
            !failing.contains("unit_adds"),
            "unit_adds selected to fail: {failing}"
        );
        assert_eq!(super::add(1, 1), 2);
    }
}

#[cfg(test)]
mod more_tests {
    #[test]
    fn unit_subtracts() {
        assert_eq!(super::subtract(3, 1), 2);
    }
}

#[cfg(all(not(test), any(target_os = "linux", target_os = "macos")))]
mod hold_before_main {
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};

    // runs before the doctest runner's main, so the runner cannot print "running N tests" until released
    #[cfg(target_os = "linux")]
    #[used]
    #[unsafe(link_section = ".init_array")]
    static HOLD: extern "C" fn() = hold;

    #[cfg(target_os = "macos")]
    #[used]
    #[unsafe(link_section = "__DATA,__mod_init_func")]
    static HOLD: extern "C" fn() = hold;

    extern "C" fn hold() {
        if let Some(directory) = std::env::var_os("CARGO_VAL_FIXTURE_BLOCK_DIR") {
            hold_and_block(&PathBuf::from(directory));
        }
    }

    // the held fifo's write end stays open until this process exits, so the harness reads EOF exactly then
    fn hold_and_block(directory: &Path) {
        let _held = OpenOptions::new()
            .write(true)
            .open(directory.join("held"))
            .expect("the harness opened the held fifo for reading");
        let mut ready = OpenOptions::new()
            .write(true)
            .open(directory.join("ready"))
            .expect("the harness opened the ready fifo for reading");
        ready
            .write_all(format!("{}\n", std::process::id()).as_bytes())
            .expect("the ready fifo accepts the process id");
        drop(ready);
        let _ = File::open(directory.join("block"));
    }
}
