#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]

#[cfg(not(test))]
use cargo_val::cli::{CliAction, CliParser};
#[cfg(all(
    not(test),
    any(target_os = "macos", all(target_os = "linux", target_env = "gnu")),
    feature = "proxima-core",
    feature = "proxima-codec",
    feature = "proxima-primitives"
))]
use cargo_val::host::unix_driver::run_unix;
#[cfg(not(test))]
use core::alloc::{GlobalAlloc, Layout};
#[cfg(not(test))]
use core::ffi::{CStr, c_char, c_int, c_void};
#[cfg(not(test))]
use core::panic::PanicInfo;

#[cfg(not(test))]
struct NoHeapAllocator;

#[cfg(not(test))]
// safety: the runtime uses caller-owned storage, so an accidental allocation
// must fail instead of silently adding a heap path.
unsafe impl GlobalAlloc for NoHeapAllocator {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 {
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, _pointer: *mut u8, _layout: Layout) {}
}

#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOCATOR: NoHeapAllocator = NoHeapAllocator;

#[cfg(not(test))]
const HELP_BANNER: &[u8] = include_bytes!("../tests/fixtures/help-banner.txt");
#[cfg(not(test))]
const VERSION_BANNER: &[u8] = concat!("cargo-val ", env!("CARGO_PKG_VERSION"), "\n").as_bytes();
#[cfg(all(
    not(test),
    not(all(
        any(target_os = "macos", all(target_os = "linux", target_env = "gnu")),
        feature = "proxima-core",
        feature = "proxima-codec",
        feature = "proxima-primitives"
    ))
))]
const NOT_IMPLEMENTED: &[u8] = b"cargo-val is not implemented yet; use --help for usage\n";

#[cfg(all(not(test), unix))]
#[cfg_attr(target_vendor = "apple", link(name = "System"))]
#[cfg_attr(not(target_vendor = "apple"), link(name = "c"))]
unsafe extern "C" {
    fn write(descriptor: c_int, buffer: *const c_void, length: usize) -> isize;
    fn _exit(status: c_int) -> !;
}

#[cfg(all(not(test), windows))]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetStdHandle(standard_handle: c_int) -> *mut c_void;
    fn WriteFile(
        handle: *mut c_void,
        buffer: *const c_void,
        length: u32,
        written: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn ExitProcess(status: u32) -> !;
}

#[cfg(all(not(test), unix))]
fn write_fd(descriptor: c_int, mut bytes: &[u8]) -> Result<(), ()> {
    while !bytes.is_empty() {
        // safety: the OS entry point provides a valid output descriptor and the
        // slice pointer remains live for the duration of this synchronous call.
        let written = unsafe { write(descriptor, bytes.as_ptr().cast(), bytes.len()) };
        let count = usize::try_from(written).map_err(|_| ())?;
        if count == 0 {
            return Err(());
        }
        bytes = &bytes[count..];
    }
    Ok(())
}

#[cfg(all(not(test), windows))]
fn write_fd(descriptor: c_int, mut bytes: &[u8]) -> Result<(), ()> {
    let standard_handle = if descriptor == 1 { -11 } else { -12 };
    // safety: the CRT provides the standard output or error handle for this process.
    let handle = unsafe { GetStdHandle(standard_handle) };
    while !bytes.is_empty() {
        let count = bytes.len().min(u32::MAX as usize) as u32;
        let mut written = 0;
        // safety: the byte slice and output count stay live for the OS call.
        let succeeded = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr().cast(),
                count,
                &mut written,
                core::ptr::null_mut(),
            )
        };
        if succeeded == 0 || written == 0 {
            return Err(());
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

#[cfg(not(test))]
/// # Safety
/// `arguments` must point to `argument_count` valid nul-terminated strings, as the C runtime provides.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn main(argument_count: c_int, arguments: *const *const c_char) -> c_int {
    let mut parser = CliParser::new();
    if argument_count > 1 && !arguments.is_null() {
        for index in 1..usize::try_from(argument_count).unwrap_or(0) {
            // safety: the C runtime provides `argument_count` argv pointers.
            let argument = unsafe { *arguments.add(index) };
            if argument.is_null() {
                continue;
            }
            // safety: the C runtime supplies argv entries as nul-terminated strings.
            let argument_bytes = unsafe { CStr::from_ptr(argument) }.to_bytes();
            parser.push(argument_bytes);
            match parser.action() {
                CliAction::Run => {}
                CliAction::Help => {
                    return c_int::from(write_fd(1, HELP_BANNER).is_err());
                }
                CliAction::Version => {
                    return c_int::from(write_fd(1, VERSION_BANNER).is_err());
                }
            }
        }
    }
    #[cfg(all(
        any(target_os = "macos", all(target_os = "linux", target_env = "gnu")),
        feature = "proxima-core",
        feature = "proxima-codec",
        feature = "proxima-primitives"
    ))]
    {
        match run_unix(&parser) {
            Ok(summary) => summary.exit_code(),
            Err(error) => {
                let line = error.render();
                let _ = write_fd(2, line.bytes());
                error.exit_code()
            }
        }
    }
    #[cfg(not(all(
        any(target_os = "macos", all(target_os = "linux", target_env = "gnu")),
        feature = "proxima-core",
        feature = "proxima-codec",
        feature = "proxima-primitives"
    )))]
    {
        let _ = write_fd(2, NOT_IMPLEMENTED);
        2
    }
}

#[cfg(all(not(test), unix))]
fn terminate(status: c_int) -> ! {
    // safety: `_exit` terminates the process without unwinding or std runtime state.
    unsafe { _exit(status) }
}

#[cfg(all(not(test), windows))]
fn terminate(status: c_int) -> ! {
    // safety: `ExitProcess` terminates the process without unwinding or std runtime state.
    unsafe { ExitProcess(status as u32) }
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_information: &PanicInfo<'_>) -> ! {
    terminate(101)
}

// liballoc's precompiled rlib references the personality even under panic=abort; nothing unwinds in this bin.
#[cfg(not(test))]
#[unsafe(no_mangle)]
extern "C" fn rust_eh_personality() {}
