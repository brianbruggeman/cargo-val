#[cfg(test)]
mod abort {
    use std::io::Write;

    #[used]
    #[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
    #[cfg_attr(not(target_os = "macos"), unsafe(link_section = ".init_array"))]
    static ABORT_BEFORE_MAIN: extern "C" fn() = abort_before_main;

    extern "C" fn abort_before_main() {
        if !std::env::args().any(|argument| argument == "--exact") {
            return;
        }
        if let Some(path) = std::env::var_os("CARGO_VAL_SHARD_LOG")
            && let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(path)
        {
            let _ = log.write_all(format!("{}\n", std::process::id()).as_bytes());
        }
        std::process::abort();
    }
}

#[cfg(test)]
include!(concat!(env!("OUT_DIR"), "/tests.rs"));
