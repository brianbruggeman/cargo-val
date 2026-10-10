//! Caller-storage Windows overlapped pipe reads.

use core::marker::PhantomPinned;
use core::mem::MaybeUninit;
use core::pin::Pin;

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_OPERATION_ABORTED, GetLastError, HANDLE,
};
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::IO::{CancelIoEx, OVERLAPPED};

/// One result delivered after the associated completion packet is dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsPipeReadCompletion {
    /// The caller-owned session buffer now contains this many bytes.
    Bytes { length: usize },
    /// The child closed its final writer.
    Eof,
    /// A zero-byte write completed without closing the pipe.
    Empty,
    /// The pending operation completed after cancellation.
    Cancelled,
    /// Windows completed the request with another error.
    Failed { code: u32 },
}

/// The operation state represented by one fixed session slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsPipeReadState {
    /// No operation currently owns the slot's buffer.
    Idle,
    /// A read is pending and owns the buffer and `OVERLAPPED` storage.
    Pending,
    /// Cancellation was requested; storage remains owned by the operation.
    Cancelling,
    /// A successful completion is available as a borrowed slice.
    Ready { length: usize },
    /// End-of-stream was completed.
    Eof,
    /// An I/O failure was completed.
    Failed { code: u32 },
}

/// One stable overlapped operation and byte buffer owned by a child slot.
pub struct WindowsPipeReadSlot<'storage> {
    buffer: &'storage mut [MaybeUninit<u8>],
    overlapped: OVERLAPPED,
    state: WindowsPipeReadState,
    _pinned: PhantomPinned,
}

impl<'storage> WindowsPipeReadSlot<'storage> {
    /// Borrows fixed storage that must remain alive until every operation completes.
    #[must_use]
    pub fn new(buffer: &'storage mut [MaybeUninit<u8>]) -> Self {
        Self {
            buffer,
            overlapped: OVERLAPPED::default(),
            state: WindowsPipeReadState::Idle,
            _pinned: PhantomPinned,
        }
    }

    /// Starts one read directly into the session-owned storage.
    ///
    /// # Safety
    ///
    /// `parent_reader` must remain open and this pinned slot and its borrowed
    /// storage must remain alive and unmoved until `complete` accepts the
    /// matching completion packet, including after `cancel` succeeds.
    pub unsafe fn start(
        self: Pin<&mut Self>,
        parent_reader: HANDLE,
    ) -> Result<WindowsPipeReadStart, WindowsPipeReadStartError> {
        let slot = unsafe { self.get_unchecked_mut() };
        if slot.state != WindowsPipeReadState::Idle {
            return Err(WindowsPipeReadStartError::Busy { state: slot.state });
        }
        let length = u32::try_from(slot.buffer.len())
            .map_err(|_| WindowsPipeReadStartError::BufferTooLarge)?;
        if length == 0 {
            return Err(WindowsPipeReadStartError::EmptyBuffer);
        }

        slot.overlapped = OVERLAPPED::default();
        slot.state = WindowsPipeReadState::Pending;
        let result = unsafe {
            ReadFile(
                parent_reader,
                slot.buffer.as_mut_ptr().cast(),
                length,
                core::ptr::null_mut(),
                &mut slot.overlapped,
            )
        };
        if result != 0 {
            return Ok(WindowsPipeReadStart::Pending);
        }

        let code = unsafe { GetLastError() };
        if code == windows_sys::Win32::Foundation::ERROR_IO_PENDING {
            return Ok(WindowsPipeReadStart::Pending);
        }
        if code == ERROR_BROKEN_PIPE {
            slot.state = WindowsPipeReadState::Eof;
            return Ok(WindowsPipeReadStart::Completed(
                WindowsPipeReadCompletion::Eof,
            ));
        }
        if code == ERROR_OPERATION_ABORTED {
            slot.state = WindowsPipeReadState::Idle;
            return Ok(WindowsPipeReadStart::Completed(
                WindowsPipeReadCompletion::Cancelled,
            ));
        }
        slot.state = WindowsPipeReadState::Failed { code };
        Err(WindowsPipeReadStartError::ReadFailed { code })
    }

    /// Requests cancellation while keeping the operation storage in use.
    ///
    /// # Safety
    ///
    /// The slot must remain pinned and alive until the matching completion is
    /// dispatched, even when the cancellation request returns success.
    pub unsafe fn cancel(
        self: Pin<&mut Self>,
        parent_reader: HANDLE,
    ) -> Result<(), WindowsPipeCancelError> {
        let slot = unsafe { self.get_unchecked_mut() };
        if slot.state != WindowsPipeReadState::Pending {
            return Err(WindowsPipeCancelError::NotPending { state: slot.state });
        }
        if unsafe { CancelIoEx(parent_reader, &slot.overlapped) } == 0 {
            return Err(WindowsPipeCancelError::RequestFailed {
                code: unsafe { GetLastError() },
            });
        }
        slot.state = WindowsPipeReadState::Cancelling;
        Ok(())
    }

    /// Accepts one completion packet from the slot's associated IOCP.
    ///
    /// # Safety
    ///
    /// The values must come from one fresh packet for this current operation.
    /// A packet must be consumed exactly once before this slot is reused.
    pub unsafe fn complete(
        self: Pin<&mut Self>,
        completed: *mut OVERLAPPED,
        bytes_transferred: u32,
        error_code: u32,
    ) -> Result<WindowsPipeReadCompletion, WindowsPipeCompletionError> {
        let slot = unsafe { self.get_unchecked_mut() };
        if !matches!(
            slot.state,
            WindowsPipeReadState::Pending | WindowsPipeReadState::Cancelling
        ) {
            return Err(WindowsPipeCompletionError::NotPending { state: slot.state });
        }
        if !core::ptr::eq(completed, &mut slot.overlapped) {
            return Err(WindowsPipeCompletionError::WrongOperation);
        }
        if error_code == 0 {
            if bytes_transferred == 0 {
                slot.state = WindowsPipeReadState::Idle;
                return Ok(WindowsPipeReadCompletion::Empty);
            }
            let length = usize::try_from(bytes_transferred)
                .map_err(|_| WindowsPipeCompletionError::LengthOverflow)?;
            if length > slot.buffer.len() {
                slot.state = WindowsPipeReadState::Failed {
                    code: windows_sys::Win32::Foundation::ERROR_INVALID_DATA,
                };
                return Err(WindowsPipeCompletionError::LengthExceedsStorage {
                    length,
                    capacity: slot.buffer.len(),
                });
            }
            slot.state = WindowsPipeReadState::Ready { length };
            return Ok(WindowsPipeReadCompletion::Bytes { length });
        }
        if error_code == ERROR_BROKEN_PIPE {
            slot.state = WindowsPipeReadState::Eof;
            return Ok(WindowsPipeReadCompletion::Eof);
        }
        if error_code == ERROR_OPERATION_ABORTED {
            slot.state = WindowsPipeReadState::Idle;
            return Ok(WindowsPipeReadCompletion::Cancelled);
        }
        slot.state = WindowsPipeReadState::Failed { code: error_code };
        Ok(WindowsPipeReadCompletion::Failed { code: error_code })
    }

    /// Returns completed bytes without copying them out of session storage.
    #[must_use]
    pub fn completed_bytes(self: Pin<&Self>) -> Option<&[u8]> {
        let slot = self.get_ref();
        let WindowsPipeReadState::Ready { length } = slot.state else {
            return None;
        };
        // safety: Ready is set only after Windows reports the initialized prefix.
        Some(unsafe { core::slice::from_raw_parts(slot.buffer.as_ptr().cast::<u8>(), length) })
    }

    /// Returns the current state without changing slot ownership.
    #[must_use]
    pub const fn state(&self) -> WindowsPipeReadState {
        self.state
    }

    /// Releases a consumed payload so the same fixed buffer can be read again.
    pub fn release_completed(self: Pin<&mut Self>) -> Result<(), WindowsPipeReadState> {
        let slot = unsafe { self.get_unchecked_mut() };
        match slot.state {
            WindowsPipeReadState::Ready { .. } => {
                slot.state = WindowsPipeReadState::Idle;
                Ok(())
            }
            state => Err(state),
        }
    }
}

/// Failure while starting a read from a pipe slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsPipeReadStartError {
    /// Another operation still owns this slot's storage.
    Busy { state: WindowsPipeReadState },
    /// Windows uses a 32-bit read length.
    BufferTooLarge,
    /// Windows cannot make progress from an empty destination.
    EmptyBuffer,
    /// The operating system rejected the read request.
    ReadFailed { code: u32 },
}

/// Whether a submitted read is pending or completed without a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsPipeReadStart {
    /// The IOCP will deliver the read result.
    Pending,
    /// Windows returned a terminal result before queuing an IOCP packet.
    Completed(WindowsPipeReadCompletion),
}

/// Failure while requesting cancellation of one read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsPipeCancelError {
    /// Cancellation is only valid for a pending operation.
    NotPending { state: WindowsPipeReadState },
    /// Windows rejected the cancellation request.
    RequestFailed { code: u32 },
}

/// Failure while associating a completion packet with a session slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsPipeCompletionError {
    /// The slot has no completion outstanding.
    NotPending { state: WindowsPipeReadState },
    /// The completion packet points at another slot's operation.
    WrongOperation,
    /// Windows reported more bytes than the fixed slot storage can hold.
    LengthExceedsStorage { length: usize, capacity: usize },
    /// A platform byte count cannot be represented as `usize`.
    LengthOverflow,
}

#[cfg(test)]
mod tests {
    use core::future::Future;
    use core::mem::MaybeUninit;
    use core::pin::pin;
    use core::sync::atomic::{AtomicU32, Ordering};
    use core::task::{Context, Poll, Waker};
    use proxima_primitives::pipe::Pipe;
    use std::format;
    use std::vec::Vec;
    use windows_sys::Win32::Foundation::{
        ERROR_OPERATION_ABORTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    use windows_sys::Win32::System::IO::{
        CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED,
    };

    use super::{WindowsPipeReadSlot, WindowsPipeReadState};
    use crate::host::windows_handles::WindowsOwnedHandle;
    use crate::host::windows_pipe_pair::WindowsPipePair;

    #[test]
    fn c45b_reads_borrowed_bytes_and_preserves_the_unread_suffix() {
        let mut pipe_pair =
            WindowsPipePair::new(&unique_pipe_name()).expect("the named pipe pair is created");
        let completion_port =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, core::ptr::null_mut(), 0, 1) };
        assert!(!completion_port.is_null());
        let completion_port = WindowsOwnedHandle::new(completion_port);
        let completion_port = completion_port.raw();
        let associated_port = unsafe {
            CreateIoCompletionPort(pipe_pair.parent_read_handle(), completion_port, 7, 0)
        };
        assert_eq!(associated_port, completion_port);

        let mut storage = [MaybeUninit::uninit(); 2];
        let storage_address = storage.as_mut_ptr().cast::<u8>();
        let mut sessions = crate::session_slots::SessionSlots::<(), 1>::new();
        let active_slot = sessions.insert(()).expect("one session slot is available");
        let mut reader = pin!(WindowsPipeReadSlot::new(&mut storage));
        let (start_result, initial_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| unsafe {
                reader.as_mut().start(pipe_pair.parent_read_handle())
            });
        assert!(matches!(
            start_result,
            Ok(super::WindowsPipeReadStart::Pending)
        ));
        assert_eq!(initial_allocations, 0);
        assert_eq!(reader.state(), WindowsPipeReadState::Pending);
        assert_eq!(wait_for_completion(completion_port, 0), None);
        assert_eq!(unsafe { GetLastError() }, WAIT_TIMEOUT);

        write_child(
            pipe_pair
                .child_write_handle()
                .expect("child writer is live"),
            b"out",
        );
        let ((completed, completion_key, bytes_transferred, error_code), completion_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| {
                wait_for_completion(completion_port, 5_000).expect("the read completion arrives")
            });
        assert_eq!(completion_allocations, 0);
        assert_eq!(completion_key, 7);
        assert_eq!(bytes_transferred, 2);
        let (_, transition_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| unsafe {
                reader
                    .as_mut()
                    .complete(completed, bytes_transferred, error_code)
                    .expect("the packet belongs to this pending read")
            });
        assert_eq!(transition_allocations, 0);
        let first_payload = reader
            .as_ref()
            .completed_bytes()
            .expect("the first payload is borrowed from session storage");
        assert_eq!(first_payload, b"ou");
        assert_eq!(first_payload.as_ptr(), storage_address);
        let (first_event, first_event_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| {
                accept_stdout(&mut sessions, active_slot, first_payload)
            });
        assert_eq!(first_event_allocations, 0);
        match first_event.expect("the live session accepts stdout") {
            crate::host_event::AcceptedHostEvent::Stdout { bytes, .. } => {
                assert_eq!(bytes.as_ptr(), storage_address);
            }
            crate::host_event::AcceptedHostEvent::Stderr { .. }
            | crate::host_event::AcceptedHostEvent::Exit { .. } => {
                panic!("stdout completion changed event kind")
            }
        }
        let (_, release_allocations) = crate::dispatch_stream::tests::measure_allocations(|| {
            reader
                .as_mut()
                .release_completed()
                .expect("the borrowed payload is consumed before reuse")
        });
        assert_eq!(release_allocations, 0);

        let (second_start_result, second_start_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| unsafe {
                reader.as_mut().start(pipe_pair.parent_read_handle())
            });
        assert!(matches!(
            second_start_result,
            Ok(super::WindowsPipeReadStart::Pending)
        ));
        assert_eq!(second_start_allocations, 0);
        let (
            (completed, completion_key, bytes_transferred, error_code),
            second_completion_allocations,
        ) = crate::dispatch_stream::tests::measure_allocations(|| {
            wait_for_completion(completion_port, 5_000).expect("the unread suffix completes")
        });
        assert_eq!(second_completion_allocations, 0);
        assert_eq!(completion_key, 7);
        assert_eq!(bytes_transferred, 1);
        let (_, second_transition_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| unsafe {
                reader
                    .as_mut()
                    .complete(completed, bytes_transferred, error_code)
                    .expect("the packet belongs to this pending read")
            });
        assert_eq!(second_transition_allocations, 0);
        let second_payload = reader
            .as_ref()
            .completed_bytes()
            .expect("the suffix is borrowed from session storage");
        assert_eq!(second_payload, b"t");
        assert_eq!(second_payload.as_ptr(), storage_address);
        let (second_event, second_event_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| {
                accept_stdout(&mut sessions, active_slot, second_payload)
            });
        assert_eq!(second_event_allocations, 0);
        match second_event.expect("the live session accepts the suffix") {
            crate::host_event::AcceptedHostEvent::Stdout { bytes, .. } => {
                assert_eq!(bytes.as_ptr(), storage_address);
            }
            crate::host_event::AcceptedHostEvent::Stderr { .. }
            | crate::host_event::AcceptedHostEvent::Exit { .. } => {
                panic!("suffix completion changed event kind")
            }
        }
        let (_, second_release_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| {
                reader
                    .as_mut()
                    .release_completed()
                    .expect("the final borrowed payload is consumed")
            });
        assert_eq!(second_release_allocations, 0);

        let (eof_start_result, eof_start_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| unsafe {
                reader.as_mut().start(pipe_pair.parent_read_handle())
            });
        assert!(matches!(
            eof_start_result,
            Ok(super::WindowsPipeReadStart::Pending)
        ));
        assert_eq!(eof_start_allocations, 0);
        pipe_pair.close_child_write_handle();
        let (completed, completion_key, bytes_transferred, error_code) =
            wait_for_completion(completion_port, 5_000).expect("pipe closure completes the read");
        assert_eq!(completion_key, 7);
        assert_eq!(
            error_code,
            windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE
        );
        let (_, eof_transition_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| unsafe {
                reader
                    .as_mut()
                    .complete(completed, bytes_transferred, error_code)
                    .expect("the EOF packet belongs to this pending read")
            });
        assert_eq!(eof_transition_allocations, 0);
        assert_eq!(reader.state(), WindowsPipeReadState::Eof);
        std::println!(
            "{{\"read_completions\":3,\"stdout_chunks\":[\"ou\",\"t\"],\"borrowed_payloads\":2,\"payload_copies\":0,\"eof_records\":1,\"slot_transition_allocations\":0,\"synchronous_reads\":0}}"
        );
    }

    #[test]
    fn c45b_retains_storage_until_cancel_completion() {
        let pipe_pair =
            WindowsPipePair::new(&unique_pipe_name()).expect("the named pipe pair is created");
        let completion_port =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, core::ptr::null_mut(), 0, 1) };
        assert!(!completion_port.is_null());
        let completion_port = WindowsOwnedHandle::new(completion_port);
        let completion_port = completion_port.raw();
        let associated_port = unsafe {
            CreateIoCompletionPort(pipe_pair.parent_read_handle(), completion_port, 9, 0)
        };
        assert_eq!(associated_port, completion_port);

        let mut storage = [MaybeUninit::uninit(); 8];
        let mut reader = pin!(WindowsPipeReadSlot::new(&mut storage));
        let (start_result, start_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| unsafe {
                reader.as_mut().start(pipe_pair.parent_read_handle())
            });
        assert!(matches!(
            start_result,
            Ok(super::WindowsPipeReadStart::Pending)
        ));
        assert_eq!(start_allocations, 0);
        assert_eq!(reader.state(), WindowsPipeReadState::Pending);
        assert_eq!(wait_for_completion(completion_port, 0), None);
        assert_eq!(unsafe { GetLastError() }, WAIT_TIMEOUT);
        write_child(
            pipe_pair
                .child_write_handle()
                .expect("child writer is live"),
            b"x",
        );
        let (completed, completion_key, bytes_transferred, error_code) =
            wait_for_completion(completion_port, 5_000)
                .expect("the short read completes before cancellation is tested");
        assert_eq!(completion_key, 9);
        assert_eq!(bytes_transferred, 1);
        assert_eq!(error_code, 0);
        let short_read = unsafe {
            reader
                .as_mut()
                .complete(completed, bytes_transferred, error_code)
                .expect("the packet belongs to this short read")
        };
        assert_eq!(
            short_read,
            super::WindowsPipeReadCompletion::Bytes { length: 1 }
        );
        assert_eq!(reader.as_ref().completed_bytes(), Some(&b"x"[..]));
        reader
            .as_mut()
            .release_completed()
            .expect("short-read bytes are consumed before reusing storage");

        let (start_result, start_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| unsafe {
                reader.as_mut().start(pipe_pair.parent_read_handle())
            });
        assert!(matches!(
            start_result,
            Ok(super::WindowsPipeReadStart::Pending)
        ));
        assert_eq!(start_allocations, 0);
        assert_eq!(reader.state(), WindowsPipeReadState::Pending);
        assert_eq!(wait_for_completion(completion_port, 0), None);
        assert_eq!(unsafe { GetLastError() }, WAIT_TIMEOUT);

        let (cancellation_result, cancellation_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| unsafe {
                reader.as_mut().cancel(pipe_pair.parent_read_handle())
            });
        cancellation_result.expect("cancellation is requested without waiting in the call");
        assert_eq!(cancellation_allocations, 0);
        assert_eq!(reader.state(), WindowsPipeReadState::Cancelling);
        let busy = unsafe { reader.as_mut().start(pipe_pair.parent_read_handle()) };
        assert!(matches!(
            busy,
            Err(super::WindowsPipeReadStartError::Busy { .. })
        ));
        let (completed, completion_key, bytes_transferred, error_code) =
            wait_for_completion(completion_port, 5_000)
                .expect("cancellation produces a completion");
        assert_eq!(completion_key, 9);
        assert_eq!(error_code, ERROR_OPERATION_ABORTED);
        let (_, cancellation_completion_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| unsafe {
                reader
                    .as_mut()
                    .complete(completed, bytes_transferred, error_code)
                    .expect("the cancellation packet releases the operation storage")
            });
        assert_eq!(cancellation_completion_allocations, 0);
        assert_eq!(reader.state(), WindowsPipeReadState::Idle);
        std::println!(
            "{{\"pending_before_cancel\":1,\"cancel_completions\":1,\"storage_releases_before_cancel_completion\":0,\"slot_transition_allocations\":0}}"
        );
    }

    #[test]
    fn c45b_reports_synchronous_eof_without_an_iocp_packet() {
        let mut pipe_pair =
            WindowsPipePair::new(&unique_pipe_name()).expect("the named pipe pair is created");
        let completion_port =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, core::ptr::null_mut(), 0, 1) };
        assert!(!completion_port.is_null());
        let completion_port = WindowsOwnedHandle::new(completion_port);
        let completion_port = completion_port.raw();
        let associated_port = unsafe {
            CreateIoCompletionPort(pipe_pair.parent_read_handle(), completion_port, 11, 0)
        };
        assert_eq!(associated_port, completion_port);
        pipe_pair.close_child_write_handle();

        let mut storage = [MaybeUninit::uninit(); 8];
        let mut reader = pin!(WindowsPipeReadSlot::new(&mut storage));
        let (start_result, start_allocations) =
            crate::dispatch_stream::tests::measure_allocations(|| unsafe {
                reader.as_mut().start(pipe_pair.parent_read_handle())
            });
        assert_eq!(
            start_result,
            Ok(super::WindowsPipeReadStart::Completed(
                super::WindowsPipeReadCompletion::Eof
            ))
        );
        assert_eq!(start_allocations, 0);
        assert_eq!(reader.state(), WindowsPipeReadState::Eof);
        assert_eq!(wait_for_completion(completion_port, 0), None);
        assert_eq!(unsafe { GetLastError() }, WAIT_TIMEOUT);
        std::println!(
            "{{\"immediate_eof_results\":1,\"iocp_packets\":0,\"slot_transition_allocations\":0}}"
        );
    }

    fn wait_for_completion(
        completion_port: HANDLE,
        timeout_milliseconds: u32,
    ) -> Option<(*mut OVERLAPPED, usize, u32, u32)> {
        let mut bytes_transferred = 0;
        let mut completion_key = 0;
        let mut overlapped = core::ptr::null_mut();
        let completed = unsafe {
            GetQueuedCompletionStatus(
                completion_port,
                &mut bytes_transferred,
                &mut completion_key,
                &mut overlapped,
                timeout_milliseconds,
            )
        };
        if overlapped.is_null() {
            return None;
        }
        Some((
            overlapped,
            completion_key,
            bytes_transferred,
            if completed == 0 {
                unsafe { GetLastError() }
            } else {
                0
            },
        ))
    }

    fn write_child(handle: HANDLE, bytes: &[u8]) {
        let mut bytes_written = 0;
        assert_eq!(
            unsafe {
                WriteFile(
                    handle,
                    bytes.as_ptr(),
                    u32::try_from(bytes.len()).expect("test payload length fits u32"),
                    &mut bytes_written,
                    core::ptr::null_mut(),
                )
            },
            1
        );
        assert_eq!(bytes_written as usize, bytes.len());
    }

    fn unique_pipe_name() -> Vec<u16> {
        static NEXT_ID: AtomicU32 = AtomicU32::new(0);
        let identifier = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        format!(
            "\\\\.\\pipe\\cargo-val-c45b-{}-{}\0",
            std::process::id(),
            identifier
        )
        .encode_utf16()
        .collect()
    }

    fn accept_stdout<'bytes>(
        sessions: &mut crate::session_slots::SessionSlots<(), 1>,
        slot: crate::session_slots::SessionSlotId,
        bytes: &'bytes [u8],
    ) -> Result<crate::host_event::AcceptedHostEvent<'bytes>, crate::host_event::StaleHostEvent>
    {
        let event_pipe = crate::host_event::HostEventPipe::new();
        let future = event_pipe.call(crate::host_event::HostEventInput {
            event: crate::host_event::HostEvent::Stdout { slot, bytes },
            sessions,
        });
        let mut future = pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        match Future::poll(future.as_mut(), &mut context) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("host-event pipe unexpectedly yielded"),
        }
    }
}
