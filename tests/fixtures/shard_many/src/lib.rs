#[cfg(test)]
mod abort {
    use std::io::Write;

    #[used]
    #[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
    #[cfg_attr(not(target_os = "macos"), unsafe(link_section = ".init_array"))]
    static ABORT_BEFORE_MAIN: extern "C" fn() = abort_before_main;

    extern "C" fn abort_before_main() {
        let arguments: Vec<String> = std::env::args().collect();
        let Some(exact) = arguments.iter().position(|argument| argument == "--exact") else {
            return;
        };
        if let Some(path) = std::env::var_os("CARGO_VAL_SHARD_LOG")
            && let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(path)
        {
            let _ = log.write_all(format!("{}\n", std::process::id()).as_bytes());
        }
        let names = arguments[exact + 1..].iter().filter(|argument| !argument.starts_with('-')).count();
        if names > 1 {
            suppress_core_dump();
            std::process::abort();
        }
    }

    // a core dump costs ~30 ms through the kernel pipe handler; the shard abort needs none
    fn suppress_core_dump() {
        let no_core = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        unsafe {
            libc::setrlimit(libc::RLIMIT_CORE, &no_core);
            #[cfg(target_os = "linux")]
            libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        }
    }
}

#[cfg(test)]
include!(concat!(env!("OUT_DIR"), "/tests.rs"));
