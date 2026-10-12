use core::cell::{Cell, RefCell};
use core::future::poll_fn;
use core::num::NonZeroI32;
use core::ops::{ControlFlow, Range};
use core::pin::Pin;
use core::task::{Context, Poll};
use std::boxed::Box;
use std::ffi::CString;
use std::io;
use std::process::ExitStatus;
use std::vec::Vec;

use prime::os::readiness::{Readiness, ReadyState};
use proxima_core::markers::DropSafe;
use proxima_primitives::pipe::{Exhausted, FanIn, Select, UnpinPipe};

#[cfg(target_os = "linux")]
use crate::edge_exit_linux::LinuxExit as ExitSource;
#[cfg(target_os = "macos")]
use crate::edge_exit_macos::MacosExit as ExitSource;

use crate::cli::CliParser;
use crate::edge_stream_unix::ChildStream;
use crate::host::unix_driver::{
    CargoDriver, LIVE_CAPACITY, ListingRun, NAME_CAPACITY, RunError, RunSummary, TestWatch,
    run_body, service_port,
};
use crate::host::unix_spawn::UnixChild;
use crate::outcomes::OutcomeLedger;
use crate::runner::RunningUnixChild;

pub(crate) enum Edge<'edge> {
    Out(&'edge ChildStream, &'edge Cell<bool>),
    Err(&'edge ChildStream, &'edge Cell<bool>),
    Exit(&'edge ExitSource),
    Port(Option<&'edge RefCell<Readiness>>),
}

pub(crate) enum Item {
    Out(io::Result<Range<u32>>),
    OutEnd,
    Err(io::Result<Range<u32>>),
    ErrEnd,
    Exit(io::Result<ExitStatus>),
    Port(io::Result<()>),
}

impl DropSafe for Edge<'_> {}

impl UnpinPipe for Edge<'_> {
    type In = ();
    type Out = Item;
    type Err = Exhausted;

    fn call(&self, (): ()) -> impl Future<Output = Result<Item, Exhausted>> + Unpin {
        poll_fn(move |context| match self {
            Edge::Out(stream, reported) => {
                stream_step(stream, reported, context, Item::Out, Item::OutEnd)
            }
            Edge::Err(stream, reported) => {
                stream_step(stream, reported, context, Item::Err, Item::ErrEnd)
            }
            Edge::Exit(exit) => step(*exit, context).map_ok(Item::Exit),
            Edge::Port(gate) => {
                gate.map_or(Poll::Ready(Err(Exhausted)), |gate| port_step(gate, context))
            }
        })
    }
}

// the merge swallows Exhausted, so the first one is turned into an item the loop can act on
fn stream_step(
    stream: &ChildStream,
    reported: &Cell<bool>,
    context: &mut Context<'_>,
    read: fn(io::Result<Range<u32>>) -> Item,
    ended: Item,
) -> Poll<Result<Item, Exhausted>> {
    match step(stream, context) {
        Poll::Ready(Err(Exhausted)) if !reported.replace(true) => Poll::Ready(Ok(ended)),
        other => other.map_ok(read),
    }
}

fn port_step(gate: &RefCell<Readiness>, context: &Context<'_>) -> Poll<Result<Item, Exhausted>> {
    match gate.borrow_mut().poll(context) {
        Ok(ReadyState::Parked) => Poll::Pending,
        Ok(ReadyState::Retry) => Poll::Ready(Ok(Item::Port(Ok(())))),
        Ok(ReadyState::OffWorker) => Poll::Ready(Ok(Item::Port(Err(io::Error::other(
            "port gate polled off a prime worker",
        ))))),
        Err(error) => Poll::Ready(Ok(Item::Port(Err(error)))),
    }
}

fn step<Source: UnpinPipe<In = (), Err = Exhausted>>(
    source: &Source,
    context: &mut Context<'_>,
) -> Poll<Result<Source::Out, Exhausted>> {
    let mut call = source.call(());
    Pin::new(&mut call).poll(context)
}

/// Awaits the merged cargo child sources, and the old port when `port` is given, until `on_item` breaks or every source is exhausted.
/// Returns how many polls parked on the reactor.
///
/// # Errors
///
/// Returns the first error `on_item` reports.
pub(crate) async fn pump_cargo(
    stdout: &ChildStream,
    stderr: &ChildStream,
    exit: &ExitSource,
    port: Option<&RefCell<Readiness>>,
    mut on_item: impl FnMut(Item) -> Result<ControlFlow<()>, RunError>,
) -> Result<usize, RunError> {
    let (out_reported, err_reported) = (Cell::new(false), Cell::new(false));
    let merged = FanIn::new(
        [
            Edge::Out(stdout, &out_reported),
            Edge::Err(stderr, &err_reported),
            Edge::Exit(exit),
            Edge::Port(port),
        ],
        Select::RoundRobin,
    );
    let mut parks = 0;
    loop {
        let polled = poll_fn(|context| {
            let polled = step(&merged, context);
            parks += usize::from(polled.is_pending());
            polled
        })
        .await;
        match polled {
            Ok(item) => {
                if on_item(item)?.is_break() {
                    return Ok(parks);
                }
            }
            Err(Exhausted) => return Ok(parks),
        }
    }
}

/// Observes the cargo build child through prime sources only, then settles whatever its artifacts launched.
///
/// # Errors
///
/// Returns [`RunError`] when a stream faults, a record is rejected, or a launched child cannot be accounted for.
pub(crate) async fn observe_cargo(
    child: UnixChild,
    driver: &mut CargoDriver<'_, '_>,
    run: &ListingRun<'_>,
    ledger: &mut OutcomeLedger<'_, RunningUnixChild<NAME_CAPACITY>, LIVE_CAPACITY>,
    watches: &mut [Option<TestWatch>; LIVE_CAPACITY],
) -> Result<(), RunError> {
    let (process_id, stdout_descriptor, stderr_descriptor) = child.into_parts();
    let process_id = NonZeroI32::new(process_id).ok_or(RunError::Spawn)?;
    // each stream owns a 20 KiB capture window, too large to sit in the awaited future
    let stdout = Box::new(ChildStream::new(stdout_descriptor.into_fd()));
    let stderr = Box::new(ChildStream::new(stderr_descriptor.into_fd()));
    let exit = ExitSource::new(process_id);
    let port = RefCell::new(Readiness::new(run.port_fd()));
    pump_cargo(&stdout, &stderr, &exit, Some(&port), |item| {
        match item {
            Item::Out(range) => {
                let range = range.map_err(|_| RunError::CargoStream)?;
                let bytes = stdout.window(range).ok_or(RunError::CargoStream)?;
                driver.advance(&bytes, false, run, ledger, watches)?;
                drop(bytes);
                driver.run_pending(run, ledger, watches)?;
            }
            Item::OutEnd => driver.stdout_ended(run, ledger, watches)?,
            Item::Err(range) => {
                let range = range.map_err(|_| RunError::Reactor)?;
                let bytes = stderr.window(range).ok_or(RunError::Reactor)?;
                driver.forward_stderr(&bytes)?;
            }
            Item::ErrEnd => driver.stderr_ended(),
            Item::Exit(status) => {
                let status = status.map_err(|_| RunError::Reactor)?;
                driver.cargo_exited(status.code().unwrap_or(1));
            }
            Item::Port(ready) => {
                ready.map_err(|_| RunError::Reactor)?;
                service_port(run, ledger, watches)?;
            }
        }
        run.pump_doc_tests()?;
        Ok(if driver.finished(run, ledger) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    })
    .await?;
    Ok(())
}

/// The whole run as one prime root: argument parsing, setup, the awaited cargo child, and the delegated remainder.
///
/// # Errors
///
/// Returns [`RunError`] naming the stage that failed.
pub async fn drive(arguments: Vec<CString>) -> Result<RunSummary, RunError> {
    let mut parser = CliParser::new();
    for argument in &arguments {
        parser.push(argument.to_bytes());
    }
    run_body(&parser).await
}

#[cfg(test)]
mod tests {
    use core::ffi::{CStr, c_char};
    use core::ops::ControlFlow;
    use core::task::Waker;
    use std::process::Command;
    use std::string::String;
    use std::vec;
    use std::vec::Vec;

    use prime::os::core_shard::run_on_this_thread;
    use proxima_runtime::CoreId;

    use super::{ChildStream, CString, ExitSource, Item, NonZeroI32, RunError, pump_cargo};
    use crate::artifact_stream::ArtifactFramer;
    use crate::cargo_source::{CargoSource, CargoSourceStep};
    use crate::edge_exit::fixture::in_single_threaded_process;
    use crate::host::unix_spawn::spawn_raw_unix;
    use crate::test_dir::tempdir;

    use core::task::{Context, Poll};

    const FRAME_BYTES: usize = 131_072;
    const SCRATCH_BYTES: usize = 16_384;
    const FIXTURE_MANIFEST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/workspace/Cargo.toml");

    fn cargo_arguments(target_directory: &str) -> Vec<String> {
        [
            "cargo",
            "test",
            "--no-run",
            "--locked",
            "--message-format=json",
            "--manifest-path",
            FIXTURE_MANIFEST,
            "--target-dir",
            target_directory,
        ]
        .map(String::from)
        .to_vec()
    }

    fn collect(
        framer: &mut ArtifactFramer<'_>,
        scratch: &mut [u8],
        chunk: &[u8],
        eof: bool,
        found: &mut Vec<String>,
    ) {
        let mut context = Context::from_waker(Waker::noop());
        let mut offset = 0;
        loop {
            let (step, consumed) = if eof {
                let Poll::Ready(result) = CargoSource::poll_lent_eof(framer, scratch, 16, &mut context) else {
                    panic!("eof drain is never pending");
                };
                (result.expect("eof drain parses"), 0)
            } else {
                let Poll::Ready(result) =
                    CargoSource::poll_lent(framer, &chunk[offset..], scratch, 16, &mut context)
                else {
                    panic!("lent chunk is never pending");
                };
                result.expect("chunk parses")
            };
            offset += consumed;
            match step {
                CargoSourceStep::Artifact(artifact) => found.push(String::from(artifact.executable)),
                CargoSourceStep::Eof => return,
                _ => {}
            }
            if !eof && offset == chunk.len() && !framer.frame_ready().expect("frame state reads") {
                return;
            }
        }
    }

    fn artifacts_of(stdout: &[u8]) -> Vec<String> {
        let mut frame_bytes = vec![0; FRAME_BYTES];
        let mut scratch = [0; SCRATCH_BYTES];
        let mut framer = ArtifactFramer::new(&mut frame_bytes, FRAME_BYTES - 1).expect("framer fits");
        let mut found = Vec::new();
        collect(&mut framer, &mut scratch, stdout, false, &mut found);
        collect(&mut framer, &mut scratch, &[], true, &mut found);
        found
    }

    fn blocking_capture(arguments: &[String]) -> Vec<String> {
        let output = Command::new(&arguments[0])
            .args(&arguments[1..])
            .output()
            .expect("cargo runs to completion");
        assert!(output.status.success(), "fixture builds");
        artifacts_of(&output.stdout)
    }

    fn environment() -> Vec<CString> {
        std::env::vars_os()
            .map(|(key, value)| {
                let mut entry = key.into_encoded_bytes();
                entry.push(b'=');
                entry.extend(value.into_encoded_bytes());
                CString::new(entry).expect("environment entries hold no nul")
            })
            .collect()
    }

    fn observed_through_prime(arguments: &[String]) -> (Vec<String>, Vec<u8>, usize) {
        let program = CString::new(arguments[0].as_str()).expect("program name");
        let owned: Vec<CString> = arguments
            .iter()
            .map(|argument| CString::new(argument.as_str()).expect("argument"))
            .collect();
        let mut argv: Vec<*const c_char> = owned.iter().map(|argument| argument.as_ptr()).collect();
        argv.push(core::ptr::null());
        let variables = environment();
        let mut envp: Vec<*const c_char> = variables.iter().map(|entry| entry.as_ptr()).collect();
        envp.push(core::ptr::null());
        let child = spawn_raw_unix(program.as_c_str(), &argv, &envp, None::<&CStr>, true).expect("cargo spawns");
        run_on_this_thread(CoreId(0), async move {
            let (process_id, stdout_descriptor, stderr_descriptor) = child.into_parts();
            let stdout = ChildStream::new(stdout_descriptor.into_fd());
            let stderr = ChildStream::new(stderr_descriptor.into_fd());
            let exit = ExitSource::new(NonZeroI32::new(process_id).expect("positive pid"));
            let mut frame_bytes = vec![0; FRAME_BYTES];
            let mut scratch = [0; SCRATCH_BYTES];
            let mut framer = ArtifactFramer::new(&mut frame_bytes, FRAME_BYTES - 1).expect("framer fits");
            let mut found = Vec::new();
            let mut raw = Vec::new();
            let parks = pump_cargo(&stdout, &stderr, &exit, None, |item| {
                match item {
                    Item::Out(range) => {
                        let bytes = stdout
                            .window(range.expect("stdout read"))
                            .ok_or(RunError::CargoStream)?;
                        raw.extend_from_slice(&bytes);
                        collect(&mut framer, &mut scratch, &bytes, false, &mut found);
                    }
                    Item::Err(range) => {
                        range.expect("stderr read");
                    }
                    Item::Exit(status) => {
                        assert_eq!(status.expect("exit status").code(), Some(0), "cargo exits cleanly");
                    }
                    Item::OutEnd | Item::ErrEnd | Item::Port(_) => {}
                }
                Ok(ControlFlow::Continue(()))
            })
            .await
            .expect("pump completes");
            collect(&mut framer, &mut scratch, &[], true, &mut found);
            (found, raw, parks)
        })
        .expect("root completes")
    }

    #[test]
    fn cargo_build_child_discovery_over_prime_sources_matches_blocking_capture() {
        let target = tempdir().expect("target directory");
        let arguments = cargo_arguments(target.path().to_str().expect("utf8 temp path"));
        let mut expected = blocking_capture(&arguments);
        expected.sort();
        assert_eq!(expected.len(), 6, "fixture lib and five integration tests: {expected:?}");

        in_single_threaded_process(|| {
            let (found, raw, parks) = observed_through_prime(&arguments);

            assert_eq!(found, artifacts_of(&raw), "incremental order equals a whole-stream parse of the same bytes");
            let mut sorted = found;
            sorted.sort();
            assert_eq!(sorted, expected, "same executables as a blocking capture; cargo's emit order varies per run");
            assert!(parks >= 1, "the cargo child was awaited at least once: {parks}");
        });
    }
}

