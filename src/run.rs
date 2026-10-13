
use core::cell::{Cell, RefCell};
use core::future::poll_fn;
use core::ops::{ControlFlow, Range};
use core::pin::Pin;
use core::task::{Context, Poll};
use std::boxed::Box;
use std::io;
use std::process::ExitStatus;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::vec::Vec;

use prime::os::readiness::{Readiness, ReadyState};
use proxima_core::markers::DropSafe;
use proxima_primitives::pipe::{Exhausted, FanIn, Select, SignalSource, UnpinPipe};

use crate::ExitSource;
use crate::cli::CliParser;
use crate::edge_signal::SignalPipe;
use crate::edge_stream::ChildStream;
use crate::driver::{
    CargoDriver, DocObservation, DocRun, LIVE_CAPACITY, Ledger, ListingRun, RunError, RunSummary,
    Watches, run_body, session_ended, session_exited, session_output, write_fd,
};
use crate::edge_spawn::Child;

pub(crate) const CARGO_LANE: usize = 0;
pub(crate) const LISTING_LANE: usize = 1;
pub(crate) const DOC_LANE: usize = 2;
pub(crate) const JOB_BASE: usize = 3;
const LANES: usize = JOB_BASE + LIVE_CAPACITY;
const ARMS: usize = LANES * 3 + 2;

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
/// Returns [`RunError::Spawn`] when the child has no usable exit source.
pub(crate) fn attach(lane: &Lane, child: Child) -> Result<u32, RunError> {
    let exit = child.exit_source().map_err(|_| RunError::Spawn)?;
    let (process_id, stdout, stderr) = child.into_parts();
    // each stream owns a 20 KiB capture window, too large to sit in the awaited future
    *lane.0.borrow_mut() = Some(Box::new(ChildStream::new(stdout)));
    *lane.1.borrow_mut() = Some(Box::new(ChildStream::new(stderr)));
    *lane.2.borrow_mut() = Some(exit);
    lane.3.set(true);
    Ok(process_id)
}

/// Gives a session's lane duplicates of its child's streams, since the ledger's session keeps the originals until it retires.
///
/// # Errors
///
/// Returns [`RunError::Spawn`] when a stream cannot be duplicated or the child has no usable exit source.
pub(crate) fn attach_session(lane: &Lane, child: &Child) -> Result<(), RunError> {
    let exit = child.exit_source().map_err(|_| RunError::Spawn)?;
    let (stdout, stderr) = child.duplicate_streams().map_err(|_| RunError::Spawn)?;
    *lane.0.borrow_mut() = Some(Box::new(ChildStream::new(stdout)));
    *lane.1.borrow_mut() = Some(Box::new(ChildStream::new(stderr)));
    *lane.2.borrow_mut() = Some(exit);
    lane.3.set(true);
    Ok(())
}

pub(crate) enum Edge<'edge> {
    Out(usize, &'edge Lane),
    Err(usize, &'edge Lane),
    Exit(usize, &'edge Lane),
    Signal(Option<&'edge RefCell<Readiness>>),
    Interrupt(Option<&'edge SignalSource>),
}

pub(crate) enum Item {
    Out(usize, io::Result<Range<u32>>),
    OutEnd(usize),
    Err(usize, io::Result<Range<u32>>),
    ErrEnd(usize),
    Exit(usize, io::Result<ExitStatus>),
    Signal(io::Result<()>),
    Interrupt,
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
            Edge::Signal(gate) => {
                gate.map_or(Poll::Ready(Err(Exhausted)), |gate| signal_step(gate, context))
            }
            Edge::Interrupt(source) => source
                .map_or(Poll::Ready(Err(Exhausted)), |source| interrupt_step(source, context)),
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

fn interrupt_step(source: &SignalSource, context: &mut Context<'_>) -> Poll<Result<Item, Exhausted>> {
    match step(source, context) {
        Poll::Ready(Ok(())) => Poll::Ready(Ok(Item::Interrupt)),
        Poll::Ready(Err(Exhausted)) | Poll::Pending => Poll::Pending,
    }
}

fn signal_step(gate: &RefCell<Readiness>, context: &Context<'_>) -> Poll<Result<Item, Exhausted>> {
    match gate.borrow_mut().poll(context) {
        Ok(ReadyState::Parked) => Poll::Pending,
        Ok(ReadyState::Retry) => Poll::Ready(Ok(Item::Signal(Ok(())))),
        Ok(ReadyState::OffWorker) => Poll::Ready(Ok(Item::Signal(Err(io::Error::other(
            "signal gate polled off a prime worker",
        ))))),
        Err(error) => Poll::Ready(Ok(Item::Signal(Err(error)))),
    }
}

fn edge<'edge>(
    lanes: &'edge Lanes,
    port: Option<(&'edge RefCell<Readiness>, &'edge SignalSource)>,
    index: usize,
) -> Edge<'edge> {
    if index == ARMS - 1 {
        return Edge::Signal(port.map(|(gate, _)| gate));
    }
    if index == ARMS - 2 {
        return Edge::Interrupt(port.map(|(_, source)| source));
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
    port: Option<(&RefCell<Readiness>, &SignalSource)>,
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

// a signal ends a session as its negated number, so the shard names the signal and a per-test child still fails
fn session_exit_code(status: io::Result<ExitStatus>) -> Result<i32, RunError> {
    status
        .map(|status| status.code().unwrap_or_else(|| terminating_signal(status).map_or(1, |signal| -signal)))
        .map_err(|_| RunError::Reactor)
}

#[cfg(unix)]
fn terminating_signal(status: ExitStatus) -> Option<i32> {
    status.signal()
}

#[cfg(not(unix))]
fn terminating_signal(_status: ExitStatus) -> Option<i32> {
    None
}

fn handle(
    item: Item,
    lanes: &Lanes,
    driver: &mut CargoDriver<'_>,
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    watches: &mut Watches,
) -> Result<(), RunError> {
    if matches!(
        item,
        Item::Out(DOC_LANE, _)
            | Item::OutEnd(DOC_LANE)
            | Item::Err(DOC_LANE, _)
            | Item::ErrEnd(DOC_LANE)
            | Item::Exit(DOC_LANE, _)
    ) {
        return run.with_doc(|doc, color, observation| {
            handle_doc(item, lanes, doc, color, observation)
        });
    }
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
            session_exited(run, ledger, watches, lane - JOB_BASE, session_exit_code(status)?)
        }
        Item::Signal(ready) => {
            ready.map_err(|_| RunError::Reactor)?;
            run.drain_signals()?;
            run.interrupt_sessions(ledger, watches);
            Ok(())
        }
        Item::Interrupt => Ok(()),
    }
}

fn handle_doc(
    item: Item,
    lanes: &Lanes,
    doc: &mut DocRun,
    color: bool,
    observation: &DocObservation,
) -> Result<(), RunError> {
    match item {
        Item::Out(_, range) => {
            let range = range.map_err(|_| RunError::Reactor)?;
            let slot = lanes[DOC_LANE].0.borrow();
            let stream = slot.as_deref().ok_or(RunError::Reactor)?;
            let bytes = stream.window(range).ok_or(RunError::Reactor)?;
            doc.stdout_chunk(&bytes, color, observation)
        }
        Item::Err(_, range) => {
            let range = range.map_err(|_| RunError::Reactor)?;
            let slot = lanes[DOC_LANE].1.borrow();
            let stream = slot.as_deref().ok_or(RunError::Reactor)?;
            let bytes = stream.window(range).ok_or(RunError::Reactor)?;
            doc.stderr_chunk(&bytes)
        }
        Item::OutEnd(_) => {
            doc.stdout_ended();
            Ok(())
        }
        Item::ErrEnd(_) => {
            doc.stderr_ended();
            Ok(())
        }
        Item::Exit(_, status) => {
            doc.exited(exit_code(status)?);
            Ok(())
        }
        Item::Signal(_) | Item::Interrupt => Ok(()),
    }
}

/// Awaits one doc-test child and the signal pipe alone, for a run that selects only doc tests.
///
/// # Errors
///
/// Returns [`RunError`] when a stream faults or the doc child cannot be attached.
pub(crate) async fn observe_doc(
    doc: &mut DocRun,
    signals: &SignalPipe,
    color: bool,
    observation: &DocObservation,
) -> Result<(), RunError> {
    let lanes = Box::new(empty_lanes());
    if let Some(child) = doc.take_child() {
        attach(&lanes[DOC_LANE], child)?;
    }
    let gate = RefCell::new(signals.gate().map_err(|_| RunError::Reactor)?);
    let interrupt = SignalSource::new(signals.interrupt());
    pump(&lanes, Some((&gate, &interrupt)), |item| {
        match item {
            Item::Interrupt => {}
            Item::Signal(ready) => {
                ready.map_err(|_| RunError::Reactor)?;
                signals.drain().map_err(|_| RunError::Reactor)?;
            }
            other => handle_doc(other, &lanes, doc, color, observation)?,
        }
        Ok(if doc.finished() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    })
    .await?;
    Ok(())
}

/// Runs a short-lived child to its exit through prime sources, handing its stdout to `on_stdout`; returns its exit code.
///
/// # Errors
///
/// Returns [`RunError`] when the child cannot be attached or a stream faults.
pub(crate) async fn collect_child(
    child: Child,
    mut on_stdout: impl FnMut(&[u8]),
) -> Result<i32, RunError> {
    let lanes = Box::new(empty_lanes());
    attach(&lanes[CARGO_LANE], child)?;
    let (mut code, mut out_ended, mut err_ended) = (None, false, false);
    pump(&lanes, None, |item| {
        match item {
            Item::Out(_, range) => {
                let range = range.map_err(|_| RunError::Reactor)?;
                let slot = lanes[CARGO_LANE].0.borrow();
                let stream = slot.as_deref().ok_or(RunError::Reactor)?;
                on_stdout(&stream.window(range).ok_or(RunError::Reactor)?);
            }
            Item::OutEnd(_) => out_ended = true,
            Item::ErrEnd(_) => err_ended = true,
            Item::Exit(_, status) => code = Some(exit_code(status)?),
            Item::Err(..) | Item::Signal(_) | Item::Interrupt => {}
        }
        Ok(if code.is_some() && out_ended && err_ended {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    })
    .await?;
    code.ok_or(RunError::Reactor)
}

/// Reads the directory a `rustc --print target-libdir` child prints into `output`, trimmed of its newline.
///
/// # Errors
///
/// Returns [`RunError::RustcLibdir`] when rustc fails, prints nothing, or prints more than `output` holds.
pub(crate) async fn collect_libdir(child: Child, output: &mut [u8]) -> Result<usize, RunError> {
    let mut length = 0;
    let mut overflow = false;
    let code = collect_child(child, |bytes| {
        let room = output.len() - length;
        overflow |= bytes.len() > room;
        let taken = bytes.len().min(room);
        output[length..length + taken].copy_from_slice(&bytes[..taken]);
        length += taken;
    })
    .await
    .map_err(|_| RunError::RustcLibdir)?;
    let mut end = length;
    while end > 0 && matches!(output[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    if code != 0 || overflow || end == 0 {
        return Err(RunError::RustcLibdir);
    }
    Ok(end)
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
    child: Child,
    driver: &mut CargoDriver<'_>,
    run: &ListingRun<'_>,
    ledger: &mut Ledger<'_>,
    watches: &mut Watches,
) -> Result<(), RunError> {
    let lanes = Box::new(empty_lanes());
    attach(&lanes[CARGO_LANE], child)?;
    if let Some(doc_child) = run.take_doc_child() {
        attach(&lanes[DOC_LANE], doc_child)?;
    }
    let signal = RefCell::new(run.signal_gate()?);
    let interrupt = run.interrupt_source();
    pump(&lanes, Some((&signal, &interrupt)), |item| {
        handle(item, &lanes, driver, run, ledger, watches)?;
        driver.resume(&lanes, run, ledger, watches)?;
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
pub async fn drive(arguments: Vec<Vec<u8>>) -> Result<RunSummary, RunError> {
    let mut parser = CliParser::new();
    for argument in &arguments {
        parser.push(argument);
    }
    run_body(&parser).await
}

#[cfg(all(test, unix))]
mod tests {
    use core::ops::ControlFlow;
    use core::task::Waker;
    use std::process::Command;
    use std::string::String;
    use std::vec;
    use std::vec::Vec;

    use prime::os::core_shard::run_on_this_thread;
    use proxima_runtime::CoreId;

    use super::{
        CARGO_LANE, Item, RunError, Child, attach, collect_libdir, empty_lanes, pump,
    };
    use crate::artifact_stream::ArtifactFramer;
    use crate::cargo_source::{CargoSource, CargoSourceStep};
    use crate::edge_exit::fixture::in_single_threaded_process;
    use crate::driver::spawn_libdir_query;
    use crate::edge_spawn::spawn;
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

    fn environment() -> Vec<Vec<u8>> {
        std::env::vars_os()
            .map(|(key, value)| {
                let mut entry = key.into_encoded_bytes();
                entry.push(b'=');
                entry.extend(value.into_encoded_bytes());
                entry
            })
            .collect()
    }

    fn observed_through_prime(arguments: &[String]) -> (Vec<String>, Vec<u8>, usize) {
        let rest: Vec<&[u8]> = arguments[1..].iter().map(String::as_bytes).collect();
        let variables = environment();
        let inherited: Vec<&[u8]> = variables.iter().map(Vec::as_slice).collect();
        let child = spawn(arguments[0].as_bytes(), &rest, &inherited, &[], None).expect("cargo spawns");
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
                    Item::Signal(_) | Item::Interrupt => {}
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


    fn libdir_through_prime(child: Child) -> Result<std::string::String, RunError> {
        run_on_this_thread(CoreId(0), async move {
            let mut output = [0_u8; 1024];
            let length = collect_libdir(child, &mut output).await?;
            Ok(std::string::String::from_utf8(output[..length].to_vec()).expect("libdir is utf-8"))
        })
        .expect("root completes")
    }

    #[test]
    fn host_libdir_is_the_directory_that_holds_libstd() {
        let entries = environment();
        let inherited: Vec<&[u8]> = entries.iter().map(Vec::as_slice).collect();
        let child = spawn_libdir_query(None, None, &inherited).expect("rustc spawns");

        let libdir = libdir_through_prime(child).expect("rustc prints its target libdir");

        let has_libstd = std::fs::read_dir(std::path::Path::new(&libdir))
            .expect("printed libdir exists")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().starts_with("libstd-"));
        assert!(has_libstd, "no libstd-* in {libdir}");
    }

    #[test]
    fn spawn_returns_while_rustc_is_still_running_and_prime_collects_its_output() {
        use std::io::Write;
        use std::os::unix::ffi::OsStrExt;

        let directory = tempdir().expect("scratch directory");
        let gate = directory.path().join("gate");
        let gate_c = std::ffi::CString::new(gate.as_os_str().as_bytes()).expect("gate path");
        assert_eq!(unsafe { libc::mkfifo(gate_c.as_ptr(), 0o600) }, 0, "mkfifo");
        let entries = environment();
        let inherited: Vec<&[u8]> = entries.iter().map(Vec::as_slice).collect();
        // exec of a freshly written file is held by the OS for 16-67 s under load, so the
        // fake rustc is the installed /bin/sh, which never pays that first-exec assessment.
        let child = spawn(
            b"/bin/sh",
            &[
                b"-c",
                b"read release < \"$1\"; echo /opt/fake-toolchain/lib",
                b"sh",
                gate.as_os_str().as_bytes(),
            ],
            &inherited,
            &[],
            None,
        )
        .expect("fake rustc spawns");
        let process_id = core::num::NonZeroI32::new(child.process_id().cast_signed()).expect("positive pid");
        assert!(
            prime::os::child::try_reap(process_id).expect("probe succeeds").is_none(),
            "rustc was still running when spawn returned"
        );
        let mut release = std::fs::OpenOptions::new()
            .write(true)
            .open(&gate)
            .expect("open gate for release");
        release.write_all(b"go\n").expect("release fake rustc");
        drop(release);

        let libdir = libdir_through_prime(child).expect("prime collects output");

        assert_eq!(libdir, "/opt/fake-toolchain/lib");
    }

    #[test]
    fn unknown_target_is_reported_as_an_output_failure() {
        let entries = environment();
        let inherited: Vec<&[u8]> = entries.iter().map(Vec::as_slice).collect();
        let child = spawn_libdir_query(None, Some(b"not-a-real-triple"), &inherited).expect("rustc spawns");

        let result = libdir_through_prime(child);

        assert_eq!(result, Err(RunError::RustcLibdir));
    }

    #[test]
    fn interrupt_arm_yields_once_on_the_scan_after_the_signal_fires() {
        let signal = proxima_core::signal::Signal::new();
        let source = proxima_primitives::pipe::SignalSource::new(&signal);
        let mut context = Context::from_waker(Waker::noop());
        let arms = [super::Edge::Interrupt(Some(&source))];
        let alone = proxima_primitives::pipe::FanIn::new(arms, proxima_primitives::pipe::Select::RoundRobin);

        assert!(matches!(super::step(&alone, &mut context), Poll::Pending), "unfired signal parks");
        signal.fire();

        assert!(matches!(super::step(&alone, &mut context), Poll::Ready(Ok(Item::Interrupt))));
        assert!(matches!(super::step(&alone, &mut context), Poll::Pending), "an exhausted source parks the merge");
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
