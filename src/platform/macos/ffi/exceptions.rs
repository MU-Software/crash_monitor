//! FFI functions for Mach exception handling (excluded from coverage).

use mach2::message::{
    MACH_MSG_TIMEOUT_NONE, MACH_MSGH_BITS, MACH_MSGH_BITS_REMOTE_MASK, MACH_RCV_LARGE,
    MACH_RCV_MSG, MACH_SEND_MSG, mach_msg, mach_msg_header_t,
};
use mach2::port::MACH_PORT_NULL;
use mach2::port::mach_port_t;
use std::sync::mpsc;
use std::thread;

use crate::platform::macos::exceptions::{message_header, parse_exception_message};
use crate::platform::macos::types::{ExceptionInfo, MachError, mach_result};

use super::spawn::{allocate_receive_port, insert_send_right};
use super::types::OwnedMachPort;

/// Blocking receive a Mach message into the provided buffer.
///
/// # Errors
/// Returns `MachError` if the `mach_msg` receive call fails.
pub fn receive_message(port: mach_port_t, buf: &mut [u8]) -> Result<(), MachError> {
    let header = buf.as_mut_ptr().cast::<mach_msg_header_t>();
    // SAFETY: mach_msg blocks until a message arrives, writing into buf.
    let kr = unsafe {
        mach_msg(
            header,
            MACH_RCV_MSG | MACH_RCV_LARGE,
            0,
            buf.len() as u32,
            port,
            MACH_MSG_TIMEOUT_NONE,
            MACH_PORT_NULL,
        )
    };
    mach_result("mach_msg(recv)", kr)
}

/// Send a Mach exception reply (`KERN_FAILURE` = let OS handle).
///
/// # Errors
/// Returns `MachError` if the `mach_msg` send call fails.
pub fn send_exception_reply(request_header: &mach_msg_header_t) -> Result<(), MachError> {
    #[repr(C)]
    struct Reply {
        header: mach_msg_header_t,
        ndr: [u8; 8],
        ret_code: i32,
    }

    let reply = Reply {
        header: mach_msg_header_t {
            msgh_bits: MACH_MSGH_BITS(request_header.msgh_bits & MACH_MSGH_BITS_REMOTE_MASK, 0),
            msgh_size: std::mem::size_of::<Reply>() as u32,
            msgh_remote_port: request_header.msgh_remote_port,
            msgh_local_port: MACH_PORT_NULL,
            msgh_voucher_port: 0,
            msgh_id: request_header.msgh_id + 100,
        },
        ndr: [0; 8],
        ret_code: mach2::kern_return::KERN_FAILURE,
    };

    // SAFETY: mach_msg sends the reply message.
    let kr = unsafe {
        mach_msg(
            (&raw const reply.header).cast_mut(),
            MACH_SEND_MSG,
            std::mem::size_of::<Reply>() as u32,
            0,
            MACH_PORT_NULL,
            MACH_MSG_TIMEOUT_NONE,
            MACH_PORT_NULL,
        )
    };
    mach_result("mach_msg(send)", kr)
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
        drop(OwnedMachPort::new(port));
        return Err(e.to_string());
    }
    Ok(port)
}

/// Start the listener thread in the PARENT process (after fork).
/// Returns a receiver that yields `ExceptionInfo` when a crash occurs.
#[must_use]
pub fn start_listener(exc_port: mach_port_t) -> mpsc::Receiver<ExceptionInfo> {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        exception_listener(exc_port, tx);
    });

    rx
}

/// Send the deferred exception reply (`KERN_FAILURE` = let OS handle).
/// Must be called from the main thread after crash data collection.
pub fn send_deferred_reply(header: &mach_msg_header_t) {
    if let Err(e) = send_exception_reply(header) {
        eprintln!("[monitor] Failed to send exception reply: {e}");
    }
}

/// Blocking loop that receives Mach exception messages and sends them to the channel.
/// The reply is NOT sent here -- it is deferred to the main thread to avoid a race
/// between the kernel delivering the fatal signal and the monitor collecting data.
#[allow(clippy::needless_pass_by_value)] // Sender is moved into this spawned thread
fn exception_listener(port: mach_port_t, tx: mpsc::Sender<ExceptionInfo>) {
    let mut msg_buf = [0u8; 1024];

    loop {
        if let Err(e) = receive_message(port, &mut msg_buf) {
            eprintln!("[monitor] mach_msg receive failed: {e}");
            break;
        }

        let (thread_port, task_port, exception_type, code, subcode) =
            match parse_exception_message(&msg_buf) {
                Ok(parsed) => parsed,
                Err(e) => {
                    eprintln!("[monitor] Failed to parse exception message: {e}");
                    continue;
                }
            };

        let reply_header = match message_header(&msg_buf) {
            Ok(h) => *h, // copy the header so the buffer can be reused
            Err(e) => {
                eprintln!("[monitor] Failed to read message header: {e}");
                continue;
            }
        };

        let info = ExceptionInfo {
            thread_port,
            task_port,
            exception_type,
            code,
            subcode,
            reply_header,
        };

        // Do NOT send reply here -- main thread sends it after data collection
        if tx.send(info).is_err() {
            break;
        }
    }
}
