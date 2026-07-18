//! FFI functions for Mach exception handling (excluded from coverage).

use mach2::message::{
    MACH_MSG_TIMEOUT_NONE, MACH_MSGH_BITS_REMOTE_MASK, MACH_RCV_LARGE, MACH_RCV_MSG, MACH_SEND_MSG,
    MACH_SEND_TIMEOUT, mach_msg, mach_msg_destroy, mach_msg_header_t, mach_msg_timeout_t,
};
use mach2::port::MACH_PORT_NULL;
use mach2::port::mach_port_t;
use std::sync::mpsc;
#[cfg(any(test, feature = "test-support"))]
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::Instant;

use crate::platform::macos::exceptions::{
    ExceptionReply, build_exception_reply, failure_reply_for_message, parse_exception_message,
};
use crate::platform::macos::types::{
    ExceptionInfo, ExceptionListenerEvent, MachError, mach_result,
};

use super::spawn::{allocate_receive_port, insert_send_right};

const MESSAGE_BUFFER_CAPACITY: usize = 8 * 1024;
const EXCEPTION_REPLY_TIMEOUT: mach_msg_timeout_t = 100;
const EXCEPTION_REPLY_OPTIONS: i32 = MACH_SEND_MSG | MACH_SEND_TIMEOUT;

/// Storage passed to `mach_msg` must satisfy `mach_msg_header_t` alignment.
/// Parsing still happens through its byte slice; alignment is only for the C
/// API's output pointer contract.
#[repr(C, align(8))]
struct MachMessageBuffer {
    bytes: [u8; MESSAGE_BUFFER_CAPACITY],
}

impl Default for MachMessageBuffer {
    fn default() -> Self {
        Self {
            bytes: [0; MESSAGE_BUFFER_CAPACITY],
        }
    }
}

impl MachMessageBuffer {
    fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn message_bytes(&self, declared_size: usize) -> &[u8] {
        if (crate::platform::macos::exceptions::MACH_HEADER_SIZE..=self.bytes.len())
            .contains(&declared_size)
        {
            &self.bytes[..declared_size]
        } else {
            // Keep the full physical header available so malformed size fields
            // can still yield a safe reply identity.
            &self.bytes
        }
    }
}

enum MessageDestroyer {
    Mach,
    #[cfg(any(test, feature = "test-support"))]
    #[allow(dead_code)] // exercised by external integration tests under test-support
    Probe(Arc<AtomicUsize>),
}

/// Owns every right and out-of-line resource carried by one received Mach
/// message. The message remains armed until `Drop`, which invokes
/// `mach_msg_destroy` exactly once. A successfully consumed reply right is
/// removed from the header before that final destroy.
pub struct ReceivedMachMessage {
    buffer: Box<MachMessageBuffer>,
    received_size: usize,
    thread_port: Option<mach_port_t>,
    destroyer: MessageDestroyer,
    armed: bool,
}

impl ReceivedMachMessage {
    fn received(buffer: Box<MachMessageBuffer>, received_size: usize) -> Self {
        Self {
            buffer,
            received_size,
            thread_port: None,
            destroyer: MessageDestroyer::Mach,
            armed: true,
        }
    }

    fn message_bytes(&self) -> &[u8] {
        self.buffer.message_bytes(self.received_size)
    }

    fn all_bytes(&self) -> &[u8] {
        self.buffer.as_bytes()
    }

    fn set_validated_thread_port(&mut self, thread_port: mach_port_t) {
        self.thread_port = Some(thread_port);
    }

    /// Return the non-owning name of the validated thread send right. The
    /// right itself continues to belong to this received-message owner.
    ///
    /// # Panics
    /// Panics only if an internal caller exposes a receive owner before the
    /// exception parser has validated its thread descriptor.
    #[must_use]
    pub fn thread_port(&self) -> mach_port_t {
        self.thread_port
            .expect("received exception message must have a validated thread port")
    }

    fn mark_reply_right_consumed(&mut self) {
        let bits = u32::from_ne_bytes(
            self.buffer.bytes[0..4]
                .try_into()
                .expect("Mach header bit field has fixed width"),
        ) & !MACH_MSGH_BITS_REMOTE_MASK;
        self.buffer.bytes[0..4].copy_from_slice(&bits.to_ne_bytes());
        self.buffer.bytes[8..12].copy_from_slice(&MACH_PORT_NULL.to_ne_bytes());
    }

    fn destroy_once(&mut self) {
        if !std::mem::replace(&mut self.armed, false) {
            return;
        }
        match &self.destroyer {
            MessageDestroyer::Mach => {
                let header = self.buffer.bytes.as_mut_ptr().cast::<mach_msg_header_t>();
                // SAFETY: a successful `mach_msg` receive initialized this
                // aligned buffer. This owner is armed exactly once and retains
                // exclusive access until all carried resources are destroyed.
                unsafe { mach_msg_destroy(header) };
            }
            #[cfg(any(test, feature = "test-support"))]
            MessageDestroyer::Probe(counter) => {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    #[allow(dead_code)] // helper for the public test-support fixture in binary test builds
    fn fixture_with_bytes(
        bytes: &[u8],
        received_size: usize,
        thread_port: Option<mach_port_t>,
    ) -> (Self, Arc<AtomicUsize>) {
        let mut buffer = Box::<MachMessageBuffer>::default();
        let copied = bytes.len().min(MESSAGE_BUFFER_CAPACITY);
        buffer.bytes[..copied].copy_from_slice(&bytes[..copied]);
        let counter = Arc::new(AtomicUsize::new(0));
        (
            Self {
                buffer,
                received_size,
                thread_port,
                destroyer: MessageDestroyer::Probe(counter.clone()),
                armed: true,
            },
            counter,
        )
    }

    /// Build a Mach-free owner for integration tests. Its cleanup path only
    /// increments the returned counter and never calls a kernel API.
    #[cfg(feature = "test-support")]
    #[allow(dead_code)] // consumed by integration crates, not the feature-enabled binary
    #[must_use]
    pub fn test_fixture(thread_port: mach_port_t) -> (Self, Arc<AtomicUsize>) {
        use mach2::message::{
            MACH_MSG_TYPE_MOVE_SEND_ONCE, MACH_MSGH_BITS, MACH_MSGH_BITS_COMPLEX,
        };

        let header = mach_msg_header_t {
            msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0) | MACH_MSGH_BITS_COMPLEX,
            msgh_size: crate::platform::macos::exceptions::MACH_HEADER_SIZE as u32,
            msgh_remote_port: 99,
            msgh_local_port: MACH_PORT_NULL,
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: crate::platform::macos::exceptions::MACH_EXCEPTION_RAISE_STATE_IDENTITY_ID,
        };
        let mut bytes = [0_u8; crate::platform::macos::exceptions::MACH_HEADER_SIZE];
        bytes[0..4].copy_from_slice(&header.msgh_bits.to_ne_bytes());
        bytes[4..8].copy_from_slice(&header.msgh_size.to_ne_bytes());
        bytes[8..12].copy_from_slice(&header.msgh_remote_port.to_ne_bytes());
        bytes[12..16].copy_from_slice(&header.msgh_local_port.to_ne_bytes());
        bytes[16..20].copy_from_slice(&header.msgh_voucher_port.to_ne_bytes());
        bytes[20..24].copy_from_slice(&header.msgh_id.to_ne_bytes());
        Self::fixture_with_bytes(&bytes, bytes.len(), Some(thread_port))
    }
}

impl Drop for ReceivedMachMessage {
    fn drop(&mut self) {
        self.destroy_once();
    }
}

/// Blocking receive of one Mach message into an owner that will destroy all
/// carried rights and out-of-line resources when dropped.
///
/// # Errors
/// Returns `MachError` if the `mach_msg` receive call fails.
fn receive_message(port: mach_port_t) -> Result<ReceivedMachMessage, MachError> {
    let mut buffer = Box::<MachMessageBuffer>::default();
    let header = buffer.bytes.as_mut_ptr().cast::<mach_msg_header_t>();
    // SAFETY: `MachMessageBuffer` is aligned for `mach_msg_header_t`, remains
    // alive and exclusively borrowed for the call, and `rcv_size` is exactly
    // its writable capacity.
    let kr = unsafe {
        mach_msg(
            header,
            MACH_RCV_MSG | MACH_RCV_LARGE,
            0,
            MESSAGE_BUFFER_CAPACITY as u32,
            port,
            MACH_MSG_TIMEOUT_NONE,
            MACH_PORT_NULL,
        )
    };
    mach_result("mach_msg(recv)", kr)?;

    // A successful receive always writes a complete fixed header. Copy the
    // size field as bytes instead of materializing a reference into storage.
    let size = u32::from_ne_bytes([
        buffer.bytes[4],
        buffer.bytes[5],
        buffer.bytes[6],
        buffer.bytes[7],
    ]);
    Ok(ReceivedMachMessage::received(buffer, size as usize))
}

fn send_built_exception_reply(reply: &mut ExceptionReply) -> Result<(), MachError> {
    // SAFETY: mach_msg sends the reply message.
    let kr = unsafe {
        mach_msg(
            &raw mut reply.header,
            EXCEPTION_REPLY_OPTIONS,
            reply.header.msgh_size,
            0,
            MACH_PORT_NULL,
            EXCEPTION_REPLY_TIMEOUT,
            MACH_PORT_NULL,
        )
    };
    mach_result("mach_msg(send)", kr)
}

fn send_owned_reply_with(
    request: &mut ReceivedMachMessage,
    mut reply: ExceptionReply,
    send: impl FnOnce(&mut ExceptionReply) -> Result<(), MachError>,
) -> Result<(), MachError> {
    send(&mut reply)?;
    // MACH_MSG_TYPE_MOVE_SEND[_ONCE] was consumed only after a successful
    // send. Prevent the owner's eventual mach_msg_destroy from disposing the
    // same header right a second time; descriptor rights remain armed.
    request.mark_reply_right_consumed();
    Ok(())
}

fn reject_malformed_message_with(
    request: &mut ReceivedMachMessage,
    send: impl FnOnce(&mut ExceptionReply) -> Result<(), MachError>,
) -> Result<bool, MachError> {
    let Some(reply) = failure_reply_for_message(request.all_bytes()) else {
        return Ok(false);
    };
    send_owned_reply_with(request, reply, send)?;
    Ok(true)
}

/// Create an exception port (receive + send right) BEFORE fork.
/// Returns the port that the child will register on itself.
///
/// # Errors
/// Returns an error string if port allocation or send right insertion fails.
pub fn create_exception_port() -> Result<mach_port_t, String> {
    let port = allocate_receive_port().map_err(|e| e.to_string())?;
    if let Err(e) = insert_send_right(port) {
        // Clean up the receive right we just allocated
        drop(super::types::OwnedExceptionPort::new(port));
        return Err(e.to_string());
    }
    Ok(port)
}

/// Start the listener thread in the PARENT process (after fork).
/// Returns a receiver that yields exception messages and fatal listener errors.
#[must_use]
pub fn start_listener(exc_port: mach_port_t) -> mpsc::Receiver<ExceptionListenerEvent> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        exception_listener(exc_port, tx);
    });

    rx
}

/// Send the deferred exception reply (`KERN_FAILURE` = let OS handle).
/// Must be called from the main thread after crash data collection. The
/// request remains responsible for all received resources; only a successfully
/// consumed reply right is disarmed.
///
/// # Errors
/// Returns `MachError` when the reply identity is unsafe or the bounded Mach
/// send fails.
pub fn send_deferred_reply(request: &mut ReceivedMachMessage) -> Result<(), MachError> {
    let header =
        crate::platform::macos::exceptions::message_header(request.all_bytes()).map_err(|_| {
            MachError {
                function: "send_deferred_reply(unsafe reply identity)",
                kern_return: -1,
            }
        })?;
    let reply = build_exception_reply(&header).map_err(|_| MachError {
        function: "send_deferred_reply(unsafe reply identity)",
        kern_return: -1,
    })?;
    send_owned_reply_with(request, reply, send_built_exception_reply)
}

/// Blocking loop that receives Mach exception messages and sends them to the channel.
/// The reply is NOT sent here -- it is deferred to the main thread to avoid a race
/// between the kernel delivering the fatal signal and the monitor collecting data.
#[allow(clippy::needless_pass_by_value)] // Sender is moved into this spawned thread
fn exception_listener(port: mach_port_t, tx: mpsc::Sender<ExceptionListenerEvent>) {
    loop {
        let mut request = match receive_message(port) {
            Ok(request) => request,
            Err(e) => {
                let message = format!("mach_msg receive failed: {e}");
                eprintln!("[monitor] {message}");
                let _ = tx.send(ExceptionListenerEvent::Fatal { message });
                break;
            }
        };
        let received_at = Instant::now();

        let parsed = match parse_exception_message(request.message_bytes()) {
            Ok(parsed) => parsed,
            Err(e) => {
                eprintln!("[monitor] Failed to parse exception message: {e}");
                match reject_malformed_message_with(&mut request, send_built_exception_reply) {
                    Ok(true) => continue,
                    Ok(false) => {
                        let message = format!(
                            "malformed Mach exception request has no safe reply identity: {e}"
                        );
                        eprintln!("[monitor] {message}");
                        let _ = tx.send(ExceptionListenerEvent::Fatal { message });
                        break;
                    }
                    Err(reply_error) => {
                        let message = format!(
                            "failed to reject malformed Mach exception request: {reply_error}"
                        );
                        eprintln!("[monitor] {message}");
                        let _ = tx.send(ExceptionListenerEvent::Fatal { message });
                        break;
                    }
                }
            }
        };
        // Parsing validated both MOVE_SEND descriptors. Their rights remain in
        // the receive buffer and are owned solely by `request`; only the
        // thread name is exposed as a non-owning view for capture.
        request.set_validated_thread_port(parsed.thread_port);

        let info = ExceptionInfo {
            received_at,
            exception_type: parsed.exception_type,
            code: parsed.code(),
            subcode: parsed.subcode(),
            raw_codes: parsed.raw_codes,
            request,
        };

        // Do NOT send reply here -- main thread sends it after data collection
        if tx.send(ExceptionListenerEvent::Exception(info)).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mach2::message::{MACH_MSG_TYPE_MOVE_SEND_ONCE, MACH_MSGH_BITS, MACH_MSGH_BITS_COMPLEX};

    fn fake_request() -> (ReceivedMachMessage, Arc<AtomicUsize>) {
        let header = mach_msg_header_t {
            msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0) | MACH_MSGH_BITS_COMPLEX,
            msgh_size: crate::platform::macos::exceptions::MACH_HEADER_SIZE as u32,
            msgh_remote_port: 99,
            msgh_local_port: MACH_PORT_NULL,
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: crate::platform::macos::exceptions::MACH_EXCEPTION_RAISE_STATE_IDENTITY_ID,
        };
        let mut bytes = [0_u8; crate::platform::macos::exceptions::MACH_HEADER_SIZE];
        bytes[0..4].copy_from_slice(&header.msgh_bits.to_ne_bytes());
        bytes[4..8].copy_from_slice(&header.msgh_size.to_ne_bytes());
        bytes[8..12].copy_from_slice(&header.msgh_remote_port.to_ne_bytes());
        bytes[12..16].copy_from_slice(&header.msgh_local_port.to_ne_bytes());
        bytes[16..20].copy_from_slice(&header.msgh_voucher_port.to_ne_bytes());
        bytes[20..24].copy_from_slice(&header.msgh_id.to_ne_bytes());
        ReceivedMachMessage::fixture_with_bytes(&bytes, bytes.len(), Some(42))
    }

    fn fake_send_error() -> MachError {
        MachError {
            function: "fake send",
            kern_return: -100,
        }
    }

    #[test]
    fn receive_storage_satisfies_mach_header_alignment() {
        assert!(
            std::mem::align_of::<MachMessageBuffer>() >= std::mem::align_of::<mach_msg_header_t>()
        );
        let buffer = MachMessageBuffer::default();
        assert_eq!(
            buffer
                .bytes
                .as_ptr()
                .align_offset(std::mem::align_of::<mach_msg_header_t>()),
            0
        );
    }

    #[test]
    fn parser_slice_uses_declared_message_size_not_receive_capacity() {
        let buffer = MachMessageBuffer::default();
        assert_eq!(buffer.message_bytes(364).len(), 364);
        assert_eq!(
            buffer.message_bytes(MESSAGE_BUFFER_CAPACITY + 1).len(),
            MESSAGE_BUFFER_CAPACITY
        );
    }

    #[test]
    fn received_owner_destroys_exactly_once_after_explicit_cleanup_and_drop() {
        let (mut request, destroys) = fake_request();

        request.destroy_once();
        assert_eq!(destroys.load(Ordering::SeqCst), 1);
        drop(request);

        assert_eq!(destroys.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn successful_reply_disarms_only_the_consumed_remote_right() {
        let (mut request, destroys) = fake_request();
        let header = crate::platform::macos::exceptions::message_header(request.all_bytes())
            .expect("fixture header");
        let reply = build_exception_reply(&header).expect("safe reply identity");

        send_owned_reply_with(&mut request, reply, |_| Ok(())).expect("fake send succeeds");

        let header = crate::platform::macos::exceptions::message_header(request.all_bytes())
            .expect("fixture header after send");
        assert_eq!(header.msgh_bits & MACH_MSGH_BITS_REMOTE_MASK, 0);
        assert_eq!(header.msgh_remote_port, MACH_PORT_NULL);
        drop(request);
        assert_eq!(destroys.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_reply_leaves_remote_right_armed_for_owner_cleanup() {
        let (mut request, destroys) = fake_request();
        let header = crate::platform::macos::exceptions::message_header(request.all_bytes())
            .expect("fixture header");
        let reply = build_exception_reply(&header).expect("safe reply identity");

        let error = send_owned_reply_with(&mut request, reply, |_| Err(fake_send_error()))
            .expect_err("fake send fails");
        assert_eq!(error.kern_return, -100);

        let header = crate::platform::macos::exceptions::message_header(request.all_bytes())
            .expect("fixture header after failed send");
        assert_eq!(
            header.msgh_bits & MACH_MSGH_BITS_REMOTE_MASK,
            MACH_MSG_TYPE_MOVE_SEND_ONCE
        );
        assert_eq!(header.msgh_remote_port, 99);
        drop(request);
        assert_eq!(destroys.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn malformed_request_reply_then_drop_cleans_owner_once() {
        let (mut request, destroys) = fake_request();
        assert!(parse_exception_message(request.message_bytes()).is_err());

        let replied = reject_malformed_message_with(&mut request, |_| Ok(()))
            .expect("fake failure reply succeeds");

        assert!(replied);
        drop(request);
        assert_eq!(destroys.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn disconnected_channel_drops_received_owner_once() {
        let (request, destroys) = fake_request();
        let (tx, rx) = mpsc::channel();
        drop(rx);

        let result = tx.send(ExceptionListenerEvent::Exception(ExceptionInfo {
            received_at: Instant::now(),
            exception_type: 1,
            code: 2,
            subcode: 3,
            raw_codes: vec![2, 3],
            request,
        }));
        drop(result);

        assert_eq!(destroys.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exception_replies_use_a_short_explicit_send_timeout() {
        assert_ne!(EXCEPTION_REPLY_OPTIONS & MACH_SEND_TIMEOUT, 0);
        assert_eq!(EXCEPTION_REPLY_TIMEOUT, 100);
    }
}
