#[cfg(test)]
mod suite {
    use std::io::Write;

    #[used]
    #[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
    #[cfg_attr(not(target_os = "macos"), unsafe(link_section = ".init_array"))]
    static ABORT_BEFORE_MAIN: extern "C" fn() = abort_before_main;

    // only a test child (`--exact`) aborts, so listing the binary still works; the pid lands before the abort
    extern "C" fn abort_before_main() {
        if !std::env::args().any(|argument| argument == "--exact") {
            return;
        }
        if let Some(path) = std::env::var_os("CARGO_VAL_SHARD_LOG")
            && let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(path)
        {
            let _ = log.write_all(format!("{}\n", std::process::id()).as_bytes());
        }
        if std::env::var_os("CARGO_VAL_INIT_ABORT").is_some() {
            std::process::abort();
        }
    }

    #[test]
    fn a_passes() {}

    #[test]
    fn b_passes() {}

    #[test]
    fn c_passes() {}

    #[test]
    fn d_passes() {}

    #[test]
    fn e_passes() {}
}
