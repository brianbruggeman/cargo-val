use core::cell::{Cell, Ref, RefCell};
use core::future::poll_fn;
use core::ops::Range;
use core::task::{Context, Poll};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;

use prime::os::readiness::{Readiness, ReadyState};
use proxima_core::markers::DropSafe;
use proxima_primitives::pipe::{Exhausted, Pipe, UnpinPipe};

use crate::edge_store_unix::{self as edge_store, Store, View};

pub(crate) const CAPTURE_BYTES: u32 = 16 * 1024;
const SCRATCH_BYTES: u32 = 4096;

/// Source pipe (`() -> io::Result<Range<u32>>`) for one child output stream: a prime readiness gate, then one nonblocking read per item.
///
/// Each item is the range of the bytes just read into a fixed capture window this source owns;
/// read them with [`ChildStream::window`]. Retained bytes are never rewritten. Once the window is
/// full the source keeps draining the pipe (so the child never blocks) into a fixed scratch tail
/// and yields ranges offset past `CAPTURE_BYTES`; such a range is valid only until the next read,
/// and [`ChildStream::truncated`] turns true. EOF resolves `Err(Exhausted)`,
/// as does every call after it or after a fault. Gate and window live here, never in the call
/// future, so dropping a pending call loses nothing. Built on `prime::os::readiness::Readiness`
/// (edge-triggered: each call drains to `EAGAIN` before parking); merge several with
/// `proxima_primitives::pipe::FanIn`.
pub struct ChildStream {
    descriptor: OwnedFd,
    gate: RefCell<Readiness>,
    window: RefCell<[u8; CAPTURE_BYTES as usize]>,
    scratch: RefCell<[u8; SCRATCH_BYTES as usize]>,
    length: Cell<u32>,
    scratch_length: Cell<u32>,
    ended: Cell<bool>,
    store: Rc<Store>,
    view: Cell<View>,
    window_sealed: Cell<bool>,
    scratch_sealed: Cell<bool>,
    seen: Cell<u64>,
}

impl ChildStream {
    #[must_use]
    pub fn new(descriptor: OwnedFd) -> Self {
        Self::with_store(descriptor, Rc::new(edge_store::unavailable()), 0)
    }

    /// A stream that seals every page it is about to overwrite or leave behind into `store` under `key`.
    #[must_use]
    pub fn with_store(descriptor: OwnedFd, store: Rc<Store>, key: u32) -> Self {
        Self {
            store,
            view: Cell::new((key, 0, 0, 0)),
            window_sealed: Cell::new(false),
            scratch_sealed: Cell::new(true),
            seen: Cell::new(0),
            gate: RefCell::new(Readiness::new(descriptor.as_raw_fd())),
            descriptor,
            window: RefCell::new([0; CAPTURE_BYTES as usize]),
            scratch: RefCell::new([0; SCRATCH_BYTES as usize]),
            length: Cell::new(0),
            scratch_length: Cell::new(0),
            ended: Cell::new(false),
        }
    }

    /// The bytes a yielded range covers, or `None` if the range reaches past what was read.
    pub fn window(&self, range: Range<u32>) -> Option<Ref<'_, [u8]>> {
        if range.start > range.end {
            return None;
        }
        if range.start >= CAPTURE_BYTES {
            let (start, end) = (range.start - CAPTURE_BYTES, range.end - CAPTURE_BYTES);
            if end > self.scratch_length.get() {
                return None;
            }
            return Ref::filter_map(self.scratch.borrow(), |bytes| {
                bytes.get(start as usize..end as usize)
            })
            .ok();
        }
        if range.end > self.length.get() {
            return None;
        }
        Ref::filter_map(self.window.borrow(), |bytes| {
            bytes.get(range.start as usize..range.end as usize)
        })
        .ok()
    }

    /// Every byte read so far that fit the capture window.
    pub fn retained(&self) -> Ref<'_, [u8]> {
        Ref::map(self.window.borrow(), |bytes| &bytes[..self.length.get() as usize])
    }

    /// Where this stream's sealed pages live in the store.
    pub fn sealed(&self) -> View {
        self.view.get()
    }

    /// The last page not yet sealed: the scratch tail, else the window if nothing was ever sealed, else empty.
    pub fn resident(&self) -> Ref<'_, [u8]> {
        if self.scratch_length.get() > 0 && !self.scratch_sealed.get() {
            return Ref::map(self.scratch.borrow(), |bytes| &bytes[..self.scratch_length.get() as usize]);
        }
        if self.window_sealed.get() {
            return Ref::map(self.scratch.borrow(), |bytes| &bytes[..0]);
        }
        self.retained()
    }

    /// Every byte read from the child so far, sealed or not.
    pub fn seen(&self) -> u64 {
        self.seen.get()
    }

    /// Seals the pages still resident so the store alone holds every byte, and returns the chain.
    ///
    /// A page the store refuses stays resident and is counted by neither the view nor the loss counter,
    /// so `seen - view total - resident` is exactly what a memory-only run could not keep.
    pub fn settle(&self) -> View {
        self.seal_tails();
        self.view.get()
    }

    fn seal_tails(&self) {
        if !self.window_sealed.get() && let Ok(window) = self.window.try_borrow() && self.append(&window[..self.length.get() as usize]) {
            self.window_sealed.set(true);
        }
        if !self.scratch_sealed.get() && let Ok(scratch) = self.scratch.try_borrow() && self.append(&scratch[..self.scratch_length.get() as usize]) {
            self.scratch_sealed.set(true);
        }
    }

    fn append(&self, bytes: &[u8]) -> bool {
        bytes.is_empty() || edge_store::append(&self.store, self.view.get(), bytes).map(|view| self.view.set(view)).is_ok()
    }

    /// Whether any bytes went past the retained window into the scratch tail.
    pub fn truncated(&self) -> bool {
        self.length.get() >= CAPTURE_BYTES && self.scratch_length.get() > 0
    }

    fn advance(&self, context: &Context<'_>) -> Poll<Result<io::Result<Range<u32>>, Exhausted>> {
        if self.ended.get() {
            return Poll::Ready(Err(Exhausted));
        }
        loop {
            match self.read_once() {
                Ok(range) if range.is_empty() => {
                    self.ended.set(true);
                    return Poll::Ready(Err(Exhausted));
                }
                Ok(range) => return Poll::Ready(Ok(Ok(range))),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    match self.gate.borrow_mut().poll(context) {
                        Ok(ReadyState::Retry) => {}
                        Ok(ReadyState::Parked) => return Poll::Pending,
                        Ok(ReadyState::OffWorker) => {
                            return self
                                .fault(io::Error::other("stream gate polled off a prime worker"));
                        }
                        Err(error) => return self.fault(error),
                    }
                }
                Err(error) => return self.fault(error),
            }
        }
    }

    fn fault(&self, error: io::Error) -> Poll<Result<io::Result<Range<u32>>, Exhausted>> {
        self.ended.set(true);
        Poll::Ready(Ok(Err(error)))
    }

    fn read_once(&self) -> io::Result<Range<u32>> {
        let start = self.length.get();
        if start >= CAPTURE_BYTES {
            return self.read_scratch();
        }
        let mut bytes = self
            .window
            .try_borrow_mut()
            .map_err(|_| io::Error::other("capture window is borrowed during a read"))?;
        let count = self.read_into(&mut bytes[start as usize..])?;
        self.length.set(start + count);
        self.seen.set(self.seen.get() + u64::from(count));
        Ok(start..start + count)
    }

    fn read_scratch(&self) -> io::Result<Range<u32>> {
        self.seal_window()?;
        let refused = self.seal_scratch()?;
        let mut bytes = self
            .scratch
            .try_borrow_mut()
            .map_err(|_| io::Error::other("scratch tail is borrowed during a read"))?;
        let count = self.read_into(&mut bytes[..])?;
        if count > 0 {
            if let Some((length, error)) = refused {
                edge_store::lose(&self.store, length, &error);
            }
            self.scratch_length.set(count);
            self.scratch_sealed.set(false);
            self.seen.set(self.seen.get() + u64::from(count));
        }
        Ok(CAPTURE_BYTES..CAPTURE_BYTES + count)
    }

    fn seal_window(&self) -> io::Result<()> {
        if !self.window_sealed.get() {
            let window = self.window.try_borrow().map_err(|_| io::Error::other("capture window is borrowed during a seal"))?;
            self.seal(&window[..self.length.get() as usize]);
            self.window_sealed.set(true);
        }
        Ok(())
    }

    // a page the store refuses stays resident until a read overwrites it, so a read that finds no bytes loses nothing
    fn seal_scratch(&self) -> io::Result<Option<(usize, io::Error)>> {
        if self.scratch_sealed.get() {
            return Ok(None);
        }
        let scratch = self.scratch.try_borrow().map_err(|_| io::Error::other("scratch tail is borrowed during a seal"))?;
        let bytes = &scratch[..self.scratch_length.get() as usize];
        match edge_store::append(&self.store, self.view.get(), bytes) {
            Ok(view) => {
                self.view.set(view);
                self.scratch_sealed.set(true);
                Ok(None)
            }
            Err(error) => Ok(Some((bytes.len(), error))),
        }
    }

    fn seal(&self, bytes: &[u8]) {
        match edge_store::append(&self.store, self.view.get(), bytes) {
            Ok(view) => self.view.set(view),
            Err(error) => edge_store::lose(&self.store, bytes.len(), &error),
        }
    }

    fn read_into(&self, spare: &mut [u8]) -> io::Result<u32> {
        let read = unsafe {
            libc::read(
                self.descriptor.as_raw_fd(),
                spare.as_mut_ptr().cast(),
                spare.len(),
            )
        };
        let count = usize::try_from(read).map_err(|_| io::Error::last_os_error())?;
        u32::try_from(count).map_err(|_| io::Error::other("read exceeds the capture window"))
    }
}

impl DropSafe for ChildStream {}

impl UnpinPipe for ChildStream {
    type In = ();
    type Out = io::Result<Range<u32>>;
    type Err = Exhausted;

    fn call(&self, (): ()) -> impl Future<Output = Result<Self::Out, Exhausted>> + Unpin {
        poll_fn(move |context| self.advance(context))
    }
}

impl Pipe for ChildStream {
    type In = ();
    type Out = io::Result<Range<u32>>;
    type Err = Exhausted;

    fn call(&self, (): ()) -> impl Future<Output = Result<Self::Out, Exhausted>> {
        UnpinPipe::call(self, ())
    }
}

#[cfg(test)]
mod tests {
    use core::future::poll_fn;
    use core::num::NonZeroI32;
    use core::ops::Range;
    use core::pin::Pin;
    use core::task::{Context, Poll};
    use std::io;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::process::ExitStatus;
    use std::rc::Rc;
    use std::vec::Vec;

    use prime::os::core_shard::run_on_this_thread;
    use proxima_core::markers::DropSafe;
    use proxima_primitives::pipe::{Exhausted, FanIn, Select, UnpinPipe};
    use proxima_runtime::CoreId;

    use super::ChildStream;
    use crate::edge_store_unix::{self as edge_store, Store, View};
    use crate::edge_exit::fixture::in_single_threaded_process;
    use crate::edge_spawn_unix::pipe_pair::{OwnedDescriptor, UnixPipePair};

    #[cfg(target_os = "linux")]
    use crate::edge_exit_linux::LinuxExit as ExitSource;
    #[cfg(target_os = "macos")]
    use crate::edge_exit_macos::MacosExit as ExitSource;

    const CARGO_RECORDS: &[u8] = b"{\"reason\":\"build-finished\",\"success\":true}\n{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"test\"],\"test\":true},\"profile\":{\"test\":true},\"package_id\":\"path+file:///fixture#0.1.0\",\"manifest_path\":\"/fixture/Cargo.toml\",\"executable\":\"/fixture/first\"}\n";

    enum Edge<'edge> {
        Out(&'edge ChildStream),
        Err(&'edge ChildStream),
        Exit(&'edge ExitSource),
    }

    enum Item {
        Out(io::Result<Range<u32>>),
        Err(io::Result<Range<u32>>),
        Exit(io::Result<ExitStatus>),
    }

    impl DropSafe for Edge<'_> {}

    impl UnpinPipe for Edge<'_> {
        type In = ();
        type Out = Item;
        type Err = Exhausted;

        fn call(&self, (): ()) -> impl Future<Output = Result<Item, Exhausted>> + Unpin {
            poll_fn(move |context| match self {
                Edge::Out(stream) => step(*stream, context).map_ok(Item::Out),
                Edge::Err(stream) => step(*stream, context).map_ok(Item::Err),
                Edge::Exit(exit) => step(*exit, context).map_ok(Item::Exit),
            })
        }
    }

    fn step<S: UnpinPipe<In = (), Err = Exhausted>>(
        source: &S,
        context: &mut Context<'_>,
    ) -> Poll<Result<S::Out, Exhausted>> {
        let mut call = source.call(());
        Pin::new(&mut call).poll(context)
    }

    async fn once<S: UnpinPipe<In = (), Err = Exhausted>>(
        source: &S,
    ) -> Poll<Result<S::Out, Exhausted>> {
        poll_fn(|context| Poll::Ready(step(source, context))).await
    }

    fn adopt(descriptor: &OwnedDescriptor) -> OwnedFd {
        let duplicate = unsafe { libc::fcntl(descriptor.raw(), libc::F_DUPFD_CLOEXEC, 0) };
        assert!(duplicate >= 0, "duplicate the parent read end");
        unsafe { OwnedFd::from_raw_fd(duplicate) }
    }

    fn fork_writer(
        pipes: &UnixPipePair,
        release_barrier: [libc::c_int; 2],
        standard_output: &'static [u8],
        standard_error: &'static [u8],
    ) -> NonZeroI32 {
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "stream fixture child forks");
        if child == 0 {
            unsafe {
                libc::close(release_barrier[1]);
                libc::close(pipes.stdout().parent_read().raw());
                libc::close(pipes.stderr().parent_read().raw());
                let failed_out = libc::write(
                    pipes.stdout().child_write().raw(),
                    standard_output.as_ptr().cast(),
                    standard_output.len(),
                ) != standard_output.len().cast_signed();
                let failed_err = libc::write(
                    pipes.stderr().child_write().raw(),
                    standard_error.as_ptr().cast(),
                    standard_error.len(),
                ) != standard_error.len().cast_signed();
                let mut release = 0_u8;
                let released = libc::read(release_barrier[0], (&raw mut release).cast(), 1);
                libc::_exit(i32::from(failed_out || failed_err || released != 1));
            }
        }
        NonZeroI32::new(child).expect("fork returned a positive pid")
    }

    fn release_child(barrier_write: libc::c_int) {
        let written = unsafe { libc::write(barrier_write, b"x".as_ptr().cast(), 1) };
        unsafe { libc::close(barrier_write) };
        assert_eq!(written, 1, "release byte reaches the child");
    }

    #[derive(Debug, Default)]
    struct Captured {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        code: Option<i32>,
        pendings: u32,
        stdout_items_before_exit: u32,
        truncated: bool,
        retained: Vec<u8>,
        last_scratch: Vec<u8>,
        ended_after_merge: bool,
        views: [View; 2],
        residents: [Vec<u8>; 2],
    }

    fn drive_children(standard_output: &'static [u8], standard_error: &'static [u8]) -> Captured {
        drive_stored(standard_output, standard_error, Rc::new(edge_store::unavailable()))
    }

    fn drive_stored(standard_output: &'static [u8], standard_error: &'static [u8], store: Rc<Store>) -> Captured {
        let pipes = UnixPipePair::new().expect("stdout and stderr pipes are created");
        let mut release_barrier = [-1; 2];
        assert_eq!(unsafe { libc::pipe(release_barrier.as_mut_ptr()) }, 0);
        let pid = fork_writer(&pipes, release_barrier, standard_output, standard_error);
        unsafe { libc::close(release_barrier[0]) };
        let (stdout_read, stderr_read) = pipes.into_parent_reads();
        let barrier_write = release_barrier[1];

        run_on_this_thread(CoreId(0), async move {
            let stdout = ChildStream::with_store(adopt(&stdout_read), Rc::clone(&store), 0);
            let stderr = ChildStream::with_store(adopt(&stderr_read), Rc::clone(&store), 1);
            let exit = ExitSource::new(pid);
            let merged = FanIn::new(
                [Edge::Out(&stdout), Edge::Err(&stderr), Edge::Exit(&exit)],
                Select::RoundRobin,
            );
            let mut captured = Captured::default();
            let mut released = false;
            loop {
                let polled = poll_fn(|context| match step(&merged, context) {
                    Poll::Pending => {
                        captured.pendings += 1;
                        if !released {
                            released = true;
                            release_child(barrier_write);
                        }
                        Poll::Pending
                    }
                    ready @ Poll::Ready(_) => ready,
                })
                .await;
                match polled {
                    Ok(Item::Out(range)) => {
                        let range = range.expect("stdout read");
                        captured.stdout_items_before_exit += u32::from(captured.code.is_none());
                        if range.start >= super::CAPTURE_BYTES {
                            captured.last_scratch = stdout
                                .window(range.clone())
                                .expect("scratch range")
                                .to_vec();
                        }
                        captured
                            .stdout
                            .extend_from_slice(&stdout.window(range).expect("range in window"));
                    }
                    Ok(Item::Err(range)) => {
                        let range = range.expect("stderr read");
                        captured
                            .stderr
                            .extend_from_slice(&stderr.window(range).expect("range in window"));
                    }
                    Ok(Item::Exit(status)) => {
                        captured.code = Some(status.expect("exit status").code().unwrap_or(-1));
                    }
                    Err(Exhausted) => break,
                }
            }
            let stdout_ended = matches!(once(&stdout).await, Poll::Ready(Err(Exhausted)));
            let stderr_ended = matches!(once(&stderr).await, Poll::Ready(Err(Exhausted)));
            let exit_ended = matches!(once(&exit).await, Poll::Ready(Err(Exhausted)));
            captured.truncated = stdout.truncated();
            captured.retained = stdout
                .window(0..super::CAPTURE_BYTES)
                .map_or_else(Vec::new, |bytes| bytes.to_vec());
            captured.ended_after_merge = stdout_ended && stderr_ended && exit_ended;
            captured.views = [stdout.sealed(), stderr.sealed()];
            captured.residents = [stdout.resident().to_vec(), stderr.resident().to_vec()];
            captured
        })
        .expect("root completes")
    }

    #[cfg(target_os = "macos")]
    #[proxima::test]
    async fn c42_reads_both_child_streams_from_the_shared_kqueue() {
        in_single_threaded_process(|| {
            let captured = drive_children(b"out", b"err");

            assert_eq!(captured.stdout, b"out");
            assert_eq!(captured.stderr, b"err");
            assert_eq!(captured.code, Some(0));
            assert!(captured.pendings >= 1, "child holds on the barrier first");
            assert!(captured.ended_after_merge, "every source stays Exhausted");
        });
    }

    #[cfg(target_os = "linux")]
    #[proxima::test]
    async fn c40_reads_both_child_streams_without_intermediate_buffers() {
        in_single_threaded_process(|| {
            let captured = drive_children(b"out", b"err");

            assert_eq!(captured.stdout, b"out");
            assert_eq!(captured.stderr, b"err");
            assert_eq!(captured.code, Some(0));
            assert!(captured.pendings >= 1, "child holds on the barrier first");
            assert!(captured.ended_after_merge, "every source stays Exhausted");
        });
    }

    #[proxima::test]
    async fn c49_native_child_stdout_yields_before_the_child_closes_it() {
        in_single_threaded_process(|| {
            let captured = drive_children(CARGO_RECORDS, b"");

            assert_eq!(captured.stdout, CARGO_RECORDS);
            assert_eq!(captured.stderr, b"");
            assert!(
                captured.stdout_items_before_exit >= 1,
                "stdout bytes arrive while the child is still held"
            );
            assert_eq!(captured.code, Some(0));
            assert!(captured.ended_after_merge, "every source stays Exhausted");
        });
    }

    const OVERFLOW_BYTES: usize = 3 * super::CAPTURE_BYTES as usize;

    static OVERFLOW_PAYLOAD: [u8; OVERFLOW_BYTES] = {
        let mut payload = [0_u8; OVERFLOW_BYTES];
        let mut index = 0;
        while index < OVERFLOW_BYTES {
            payload[index] = (index % 251).to_le_bytes()[0];
            index += 1;
        }
        payload
    };

    #[proxima::test]
    async fn stream_past_capacity_keeps_draining_through_the_scratch_tail() {
        in_single_threaded_process(|| {
            let captured = drive_children(&OVERFLOW_PAYLOAD, b"");
            let retained = super::CAPTURE_BYTES as usize;

            assert!(captured.truncated, "three windows of output overflow one");
            assert_eq!(captured.retained, &OVERFLOW_PAYLOAD[..retained]);
            assert_eq!(captured.stdout, OVERFLOW_PAYLOAD, "every byte was drained");
            assert_ne!(captured.last_scratch, b"");
            assert_eq!(
                captured.last_scratch,
                &OVERFLOW_PAYLOAD[OVERFLOW_BYTES - captured.last_scratch.len()..]
            );
            assert_eq!(captured.code, Some(0), "the child was never blocked");
            assert!(captured.ended_after_merge, "every source stays Exhausted");
        });
    }

    #[proxima::test]
    async fn stream_parks_until_writer_writes() {
        in_single_threaded_process(|| {
            let pipes = UnixPipePair::new().expect("pipe is created");
            let reader = adopt(pipes.stdout().parent_read());
            let writer = pipes.stdout().child_write().raw();

            let outcome = run_on_this_thread(CoreId(0), async move {
                let source = ChildStream::new(reader);
                let mut pendings = 0;
                let first = once(&source).await;
                pendings += u32::from(first.is_pending());
                let written = unsafe { libc::write(writer, b"cargo-val".as_ptr().cast(), 9) };
                assert_eq!(written, 9, "writer delivers the payload");
                let Poll::Ready(Ok(Ok(range))) = once(&source).await else {
                    panic!("written bytes yield a range");
                };
                let bytes = source
                    .window(range.clone())
                    .expect("range in window")
                    .to_vec();
                drop(pipes);
                let closed = once(&source).await;
                let again = once(&source).await;
                (
                    pendings,
                    range,
                    bytes,
                    matches!(closed, Poll::Ready(Err(Exhausted))),
                    matches!(again, Poll::Ready(Err(Exhausted))),
                )
            })
            .expect("root completes");

            assert_eq!(outcome.0, 1, "empty pipe parks once");
            assert_eq!(outcome.1, 0..9);
            assert_eq!(outcome.2, b"cargo-val");
            assert!(outcome.3, "closed writer ends the source");
            assert!(outcome.4, "the source stays Exhausted");
        });
    }

    use std::fs::File;
    use std::string::ToString;
    use std::sync::OnceLock;

    use super::CAPTURE_BYTES;

    const STREAM_BYTES: usize = 1024 * 1024;

    fn pattern(stream: usize) -> Vec<u8> {
        (0..STREAM_BYTES).map(|index| ((index * 31 + stream) % 251).to_le_bytes()[0]).collect()
    }

    fn standard_output() -> &'static [u8] {
        static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
        BYTES.get_or_init(|| pattern(0))
    }

    fn standard_error() -> &'static [u8] {
        static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
        BYTES.get_or_init(|| pattern(1))
    }

    fn reassemble(store: &Store, view: View, resident: &[u8]) -> Vec<u8> {
        let mut offsets = Vec::new();
        let mut cursor = view.2;
        let mut page = [0_u8; CAPTURE_BYTES as usize];
        let mut pages = Vec::new();
        while cursor != 0 {
            let (previous, length) = edge_store::read_page(store, cursor, &mut page).expect("sealed page reads back");
            offsets.push(cursor);
            pages.push(page[..length].to_vec());
            cursor = previous;
        }
        pages.reverse();
        let mut bytes = pages.concat();
        bytes.extend_from_slice(resident);
        bytes
    }

    fn store_in(directory: &std::path::Path) -> (Rc<Store>, std::path::PathBuf) {
        let store = Rc::new(edge_store::open(Some(directory)));
        let data = directory.join("cargo-val").join("run").join(std::process::id().to_string()).join("data");
        (store, data)
    }

    #[proxima::test]
    async fn each_stream_of_a_one_mebibyte_pair_reassembles_byte_for_byte_from_the_store() {
        in_single_threaded_process(|| {
            let directory = tempfile::tempdir().expect("temporary target");
            let (store, data) = store_in(directory.path());

            let captured = drive_stored(standard_output(), standard_error(), Rc::clone(&store));

            let out = reassemble(&store, captured.views[0], &captured.residents[0]);
            let err = reassemble(&store, captured.views[1], &captured.residents[1]);
            assert_eq!((out.len(), err.len()), (STREAM_BYTES, STREAM_BYTES));
            assert!(out == standard_output(), "stdout differs from the written pattern");
            assert!(err == standard_error(), "stderr differs from the written pattern");
            assert_eq!(store.2.get(), 0, "no byte was lost");
            let size = std::fs::metadata(&data).expect("data file exists").len();
            assert_eq!(size, store.1.get(), "the file ends where the store says it does");
            assert!(size > 2 * STREAM_BYTES as u64 - 2 * 4096, "the store holds both streams plus headers: {size}");
        });
    }

    #[proxima::test]
    async fn a_quiet_stream_under_one_window_never_spills() {
        in_single_threaded_process(|| {
            let directory = tempfile::tempdir().expect("temporary target");
            let (store, data) = store_in(directory.path());

            let captured = drive_stored(&standard_output()[..4096], b"", Rc::clone(&store));

            assert_eq!(captured.views[0], (0, 0, 0, 0), "nothing sealed");
            assert_eq!(captured.residents[0], &standard_output()[..4096]);
            assert_eq!(std::fs::metadata(&data).expect("data file exists").len(), 8, "only the magic");
        });
    }

    #[proxima::test]
    async fn a_failing_store_counts_lost_bytes_notices_once_and_never_blocks_the_child() {
        in_single_threaded_process(|| {
            let directory = tempfile::tempdir().expect("temporary target");
            let file = File::options().read(true).write(true).create_new(true).open(directory.path().join("data")).expect("data file");
            let store = Rc::new(edge_store::from_file(file).expect("store opens"));
            let limit = libc::rlimit { rlim_cur: 64 * 1024, rlim_max: libc::RLIM_INFINITY };
            unsafe {
                libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
                assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &raw const limit), 0, "file size limit applies");
            }

            let captured = drive_stored(standard_output(), b"", Rc::clone(&store));

            assert_eq!(captured.stdout, standard_output(), "the lane kept draining every byte");
            assert_eq!(captured.code, Some(0), "the child exit is unchanged");
            assert!(store.3.get(), "the notice was given");
            assert!(store.2.get() > 0, "lost bytes are counted: {}", store.2.get());
            let stored = captured.views[0].3;
            let tail = captured.residents[0].len() as u64;
            assert_eq!(stored + store.2.get() + tail, (STREAM_BYTES as u64), "sealed + lost + resident account for every byte");
        });
    }

    fn sealed_lengths(store: &Store, view: View) -> Vec<usize> {
        let mut lengths = Vec::new();
        let mut cursor = view.2;
        let mut page = [0_u8; CAPTURE_BYTES as usize];
        while cursor != 0 {
            let (previous, length) = edge_store::read_page(store, cursor, &mut page).expect("sealed page reads back");
            lengths.push(length);
            cursor = previous;
        }
        lengths.reverse();
        lengths
    }

    #[proxima::test]
    async fn eof_with_a_partly_filled_scratch_keeps_every_byte_in_sealed_pages_plus_resident() {
        in_single_threaded_process(|| {
            let directory = tempfile::tempdir().expect("temporary target");
            let (store, _) = store_in(directory.path());
            let written = &OVERFLOW_PAYLOAD[..CAPTURE_BYTES as usize + 1000];

            let captured = drive_stored(written, b"", Rc::clone(&store));

            let rebuilt = reassemble(&store, captured.views[0], &captured.residents[0]);
            assert!(rebuilt == written, "sealed pages plus resident differ from the {} bytes written", written.len());
            assert!(captured.residents[0].is_empty(), "the end-of-stream read sealed the scratch tail");
            assert_eq!(sealed_lengths(&store, captured.views[0]), [CAPTURE_BYTES as usize, 1000], "window page, then the partly filled scratch page");
            assert_eq!(store.2.get(), 0, "no byte was lost");
        });
    }

    #[proxima::test]
    async fn one_byte_past_an_exactly_full_window_seals_a_full_window_page_then_a_one_byte_page() {
        in_single_threaded_process(|| {
            let directory = tempfile::tempdir().expect("temporary target");
            let (store, _) = store_in(directory.path());
            let written = &OVERFLOW_PAYLOAD[..=CAPTURE_BYTES as usize];

            let captured = drive_stored(written, b"", Rc::clone(&store));

            assert_eq!(sealed_lengths(&store, captured.views[0]), [16384, 1], "one whole window page, then the single extra byte");
            assert!(captured.residents[0].is_empty(), "the end-of-stream read sealed the extra byte");
            assert_eq!(captured.views[0].3, 16385, "sealed byte count");
            assert!(captured.truncated);
        });
    }

    #[proxima::test]
    async fn an_exactly_full_window_is_sealed_as_one_page_by_the_end_of_stream_read() {
        in_single_threaded_process(|| {
            let directory = tempfile::tempdir().expect("temporary target");
            let (store, _) = store_in(directory.path());
            let written = &OVERFLOW_PAYLOAD[..CAPTURE_BYTES as usize];

            let captured = drive_stored(written, b"", Rc::clone(&store));

            assert_eq!(sealed_lengths(&store, captured.views[0]), [16384], "the full window is the one sealed page");
            assert!(captured.residents[0].is_empty(), "nothing is left unsealed");
            assert!(!captured.truncated, "no byte went past the window");
        });
    }
}
