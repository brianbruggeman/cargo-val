
use core::cell::{Cell, RefCell};
use core::future::poll_fn;
use core::num::NonZeroI32;
use core::ops::{ControlFlow, Range};
use core::pin::Pin;
use core::task::{Context, Poll};
use std::boxed::Box;
use std::ffi::CString;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
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
    CargoDriver, LIVE_CAPACITY, Ledger, ListingRun, RunError, RunSummary, Watches, run_body,
    service_port, session_ended, session_exited, session_output, write_fd,
};
use crate::host::unix_spawn::UnixChild;

pub(crate) const CARGO_LANE: usize = 0;
pub(crate) const LISTING_LANE: usize = 1;
pub(crate) const JOB_BASE: usize = 2;
const LANES: usize = JOB_BASE + LIVE_CAPACITY;
const ARMS: usize = LANES * 3 + 1;

/// One child's stdout and stderr streams, its exit source, and whether its stdout may be read now.
pub(crate) type Lane = (
    RefCell<Option<Box<ChildStream>>>,
    RefCell<Option<Box<ChildStream>>>,
    RefCell<Option<ExitSource>>,
    Cell<bool>,
);

pub(crate) type Lanes = [Lane; LANES];

pub(crate) fn empty_lanes() -> Lanes {
    core::array::from_fn(|_| {
        (
            RefCell::new(None),
            RefCell::new(None),
            RefCell::new(None),
            Cell::new(true),
        )
    })
}

/// Gives `lane` the child's streams and exit source; an empty lane parks, so the merge never ends on it.
///
/// # Errors
///
/// Returns [`RunError::Spawn`] when the child has no usable process id.
pub(crate) fn attach(lane: &Lane, child: UnixChild) -> Result<libc::pid_t, RunError> {
    let (process_id, stdout, stderr) = child.into_parts();
    let exit = ExitSource::new(NonZeroI32::new(process_id).ok_or(RunError::Spawn)?);
    // each stream owns a 20 KiB capture window, too large to sit in the awaited future
    *lane.0.borrow_mut() = Some(Box::new(ChildStream::new(stdout.into_fd())));
    *lane.1.borrow_mut() = Some(Box::new(ChildStream::new(stderr.into_fd())));
    *lane.2.borrow_mut() = Some(exit);
    lane.3.set(true);
    Ok(process_id)
}

/// Gives a session's lane duplicates of its child's streams, since the ledger's session keeps the originals until it retires.
///
/// # Errors
///
/// Returns [`RunError::Spawn`] when a descriptor cannot be duplicated or the child has no usable process id.
pub(crate) fn attach_session(lane: &Lane, child: &UnixChild) -> Result<(), RunError> {
    let exit = ExitSource::new(NonZeroI32::new(child.process_id()).ok_or(RunError::Spawn)?);
    *lane.0.borrow_mut() = Some(Box::new(ChildStream::new(duplicate(child.stdout().raw())?)));
    *lane.1.borrow_mut() = Some(Box::new(ChildStream::new(duplicate(child.stderr().raw())?)));
    *lane.2.borrow_mut() = Some(exit);
    lane.3.set(true);
    Ok(())
}

fn duplicate(descriptor: libc::c_int) -> Result<OwnedFd, RunError> {
    // safety: the descriptor is an open pipe end owned by the session, and the duplicate is owned by the returned OwnedFd alone.
    let copy = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 0) };
    if copy < 0 {
        return Err(RunError::Spawn);
    }
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

pub(crate) enum Edge<'edge> {
    Out(usize, &'edge Lane),
    Err(usize, &'edge Lane),
    Exit(usize, &'edge Lane),
    Port(Option<&'edge RefCell<Readiness>>),
}

pub(crate) enum Item {
    Out(usize, io::Result<Range<u32>>),
    OutEnd(usize),
    Err(usize, io::Result<Range<u32>>),
    ErrEnd(usize),
    Exit(usize, io::Result<ExitStatus>),
    Port(io::Result<()>),
}

impl DropSafe for Edge<'_> {}

impl UnpinPipe for Edge<'_> {
    type In = ();
    type Out = Item;
    type Err = Exhausted;

    fn call(&self, (): ()) -> impl Future<Output = Result<Item, Exhausted>> + Unpin {
        poll_fn(move |context| match self {
            Edge::Out(id, lane) => {
                stream_step(&lane.0, lane.3.get(), context, (*id, Item::Out, Item::OutEnd))
            }
            Edge::Err(id, lane) => stream_step(&lane.1, true, context, (*id, Item::Err, Item::ErrEnd)),
            Edge::Exit(id, lane) => exit_step(&lane.2, *id, context),
            Edge::Port(gate) => {
                gate.map_or(Poll::Ready(Err(Exhausted)), |gate| port_step(gate, context))
            }
        })
    }
}

fn step<Source: UnpinPipe<In = (), Err = Exhausted>>(
    source: &Source,
    context: &mut Context<'_>,
) -> Poll<Result<Source::Out, Exhausted>> {
    let mut call = source.call(());
    Pin::new(&mut call).poll(context)
}

type StreamItems = (usize, fn(usize, io::Result<Range<u32>>) -> Item, fn(usize) -> Item);

// the merge swallows Exhausted, so the first one is turned into an item the loop can act on; a gated or empty lane parks
fn stream_step(
    slot: &RefCell<Option<Box<ChildStream>>>,
    wanted: bool,
    context: &mut Context<'_>,
    (id, read, ended): StreamItems,
) -> Poll<Result<Item, Exhausted>> {
    if !wanted {
        return Poll::Pending;
    }
    let mut guard = slot.borrow_mut();
    let polled = match guard.as_deref() {
        Some(stream) => step(stream, context),
        None => return Poll::Pending,
    };
    match polled {
        Poll::Ready(Err(Exhausted)) => {
            *guard = None;
            Poll::Ready(Ok(ended(id)))
        }
        other => other.map_ok(|chunk| read(id, chunk)),
    }
}

fn exit_step(
    slot: &RefCell<Option<ExitSource>>,
    id: usize,
    context: &mut Context<'_>,
) -> Poll<Result<Item, Exhausted>> {
    let mut guard = slot.borrow_mut();
    let polled = match guard.as_ref() {
        Some(exit) => step(exit, context),
        None => return Poll::Pending,
    };
    match polled {
        Poll::Ready(Ok(status)) => {
            *guard = None;
            Poll::Ready(Ok(Item::Exit(id, status)))
        }
        Poll::Ready(Err(Exhausted)) => {
            *guard = None;
            Poll::Pending
        }
        Poll::Pending => Poll::Pending,
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

fn edge<'edge>(
    lanes: &'edge Lanes,
    port: Option<&'edge RefCell<Readiness>>,
    index: usize,
) -> Edge<'edge> {
    if index == ARMS - 1 {
        return Edge::Port(port);
    }
    let lane = &lanes[index / 3];
    match index % 3 {
        0 => Edge::Out(index / 3, lane),
        1 => Edge::Err(index / 3, lane),
        _ => Edge::Exit(index / 3, lane),
    }
}

/// Awaits every lane's streams and exit, and the old port when `port` is given, until `on_item` breaks.
/// Returns how many polls parked on the reactor.
///
/// # Errors
///
/// Returns the first error `on_item` reports.
pub(crate) async fn pump(
    lanes: &Lanes,
    port: Option<&RefCell<Readiness>>,
    mut on_item: impl FnMut(Item) -> Result<ControlFlow<()>, RunError>,
) -> Result<usize, RunError> {
    let merged = FanIn::new(
        core::array::from_fn::<_, ARMS, _>(|index| edge(lanes, port, index)),
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

fn exit_code(status: io::Result<ExitStatus>) -> Result<i32, RunError> {
    status
        .map(|status| status.code().unwrap_or(1))
        .map_err(|_| RunError::Reactor)
}

fn handle(
    item: Item,
    lanes: &Lanes,
    driver: &mut CargoDriver<'_>,
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    watches: &mut Watches,
) -> Result<(), RunError> {
    match item {
        Item::Out(CARGO_LANE, range) => {
            let range = range.map_err(|_| RunError::CargoStream)?;
            driver.cargo_chunk(lanes, range, 0, run, ledger)
        }
        Item::Out(LISTING_LANE, range) => {
            let range = range.map_err(|_| RunError::ListingStream)?;
            driver.listing_chunk(lanes, range, 0, run, ledger, watches)
        }
        Item::Out(lane, range) => session_chunk(lanes, lane, true, range, run, ledger, watches),
        Item::Err(lane, range) if lane >= JOB_BASE => {
            session_chunk(lanes, lane, false, range, run, ledger, watches)
        }
        Item::Err(lane, range) => {
            let range = range.map_err(|_| RunError::Reactor)?;
            let slot = lanes[lane].1.borrow();
            let stream = slot.as_deref().ok_or(RunError::Reactor)?;
            let bytes = stream.window(range).ok_or(RunError::Reactor)?;
            if lane == CARGO_LANE {
                driver.forward_stderr(&bytes)
            } else {
                write_fd(2, &bytes)
            }
        }
        Item::OutEnd(CARGO_LANE) => {
            driver.cargo_out_ended();
            Ok(())
        }
        Item::OutEnd(LISTING_LANE) => {
            driver.listing_stdout_ended();
            Ok(())
        }
        Item::OutEnd(lane) => session_ended(run, ledger, watches, lane - JOB_BASE, true),
        Item::ErrEnd(CARGO_LANE) => {
            driver.stderr_ended();
            Ok(())
        }
        Item::ErrEnd(LISTING_LANE) => {
            driver.listing_stderr_ended();
            Ok(())
        }
        Item::ErrEnd(lane) => session_ended(run, ledger, watches, lane - JOB_BASE, false),
        Item::Exit(CARGO_LANE, status) => {
            driver.cargo_exited(exit_code(status)?);
            Ok(())
        }
        Item::Exit(LISTING_LANE, status) => {
            driver.listing_exited(exit_code(status)?);
            Ok(())
        }
        Item::Exit(lane, status) => {
            session_exited(run, ledger, watches, lane - JOB_BASE, exit_code(status)?)
        }
        Item::Port(ready) => {
            ready.map_err(|_| RunError::Reactor)?;
            service_port(run)
        }
    }
}

fn session_chunk(
    lanes: &Lanes,
    lane: usize,
    stdout: bool,
    range: io::Result<Range<u32>>,
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    watches: &mut Watches,
) -> Result<(), RunError> {
    let range = range.map_err(|_| RunError::Reactor)?;
    let slot = if stdout { lanes[lane].0.borrow() } else { lanes[lane].1.borrow() };
    let stream = slot.as_deref().ok_or(RunError::Reactor)?;
    let bytes = stream.window(range).ok_or(RunError::Reactor)?;
    session_output(run, ledger, watches, lane - JOB_BASE, stdout, &bytes)
}

/// Observes the cargo build child, its listings, and the old port through prime sources only, until every launched child is accounted for.
///
/// # Errors
///
/// Returns [`RunError`] when a stream faults, a record is rejected, or a launched child cannot be accounted for.
pub(crate) async fn observe_cargo(
    child: UnixChild,
    driver: &mut CargoDriver<'_>,
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    watches: &mut Watches,
) -> Result<(), RunError> {
    let lanes = Box::new(empty_lanes());
    attach(&lanes[CARGO_LANE], child)?;
    let port = RefCell::new(Readiness::new(run.port_fd()));
    pump(&lanes, Some(&port), |item| {
        handle(item, &lanes, driver, run, ledger, watches)?;
        driver.resume(&lanes, run, ledger, watches)?;
        run.pump_doc_tests()?;
        lanes[CARGO_LANE].3.set(driver.cargo_open());
        lanes[LISTING_LANE].3.set(driver.listing_open(run, ledger));
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

    use super::{CARGO_LANE, CString, Item, RunError, attach, empty_lanes, pump};
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
            let lanes = empty_lanes();
            attach(&lanes[CARGO_LANE], child).expect("cargo child attaches");
            let mut frame_bytes = vec![0; FRAME_BYTES];
            let mut scratch = [0; SCRATCH_BYTES];
            let mut framer = ArtifactFramer::new(&mut frame_bytes, FRAME_BYTES - 1).expect("framer fits");
            let mut found = Vec::new();
            let mut raw = Vec::new();
            let (mut exited, mut out_ended, mut err_ended) = (false, false, false);
            let parks = pump(&lanes, None, |item| {
                match item {
                    Item::Out(_, range) => {
                        let slot = lanes[CARGO_LANE].0.borrow();
                        let stream = slot.as_deref().expect("stdout stream is attached");
                        let bytes = stream
                            .window(range.expect("stdout read"))
                            .ok_or(RunError::CargoStream)?;
                        raw.extend_from_slice(&bytes);
                        collect(&mut framer, &mut scratch, &bytes, false, &mut found);
                    }
                    Item::Err(_, range) => {
                        range.expect("stderr read");
                    }
                    Item::Exit(_, status) => {
                        assert_eq!(status.expect("exit status").code(), Some(0), "cargo exits cleanly");
                        exited = true;
                    }
                    Item::OutEnd(_) => out_ended = true,
                    Item::ErrEnd(_) => err_ended = true,
                    Item::Port(_) => {}
                }
                Ok(if exited && out_ended && err_ended {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                })
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

