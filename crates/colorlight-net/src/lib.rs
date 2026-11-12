//! Raw Ethernet transport and card discovery for Colorlight 5A-75B cards.
//!
//! * [`Link`] is the small trait the sink and discovery are written against.
//! * [`RawLink`] is the real thing: a Linux `AF_PACKET` socket bound to a
//!   named interface. On other platforms it is a stub whose constructor fails
//!   with [`LinkError::Unsupported`], so the daemon still builds there.
//! * [`mock::MockLink`] is an in-memory link for tests.
//! * [`discover`] finds the cards on a link (DESIGN.md §4.2).
//!
//! Packet layouts live in `colorlight-proto`; this crate never builds
//! packet bytes itself.

use std::io;
use std::time::Duration;

mod discovery;
pub mod mock;

pub use discovery::{discover, MAX_RECEIVERS};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::RawLink;

#[cfg(not(target_os = "linux"))]
mod stub;
#[cfg(not(target_os = "linux"))]
pub use stub::RawLink;

/// Why a link operation failed.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// Opening the raw socket was refused.
    #[error(
        "permission denied opening a raw socket on {interface:?}: paneld needs CAP_NET_RAW. \
         Grant it with `sudo setcap cap_net_raw+ep <path to paneld>` or, under systemd, \
         `AmbientCapabilities=CAP_NET_RAW`"
    )]
    PermissionDenied { interface: String },
    /// No network interface of that name.
    #[error("unknown network interface {0:?} (see `ip link`)")]
    UnknownInterface(String),
    /// Raw Ethernet output only exists on Linux.
    #[error("raw Ethernet output is only supported on Linux (AF_PACKET)")]
    Unsupported,
    /// Any other OS error.
    #[error("{context}: {source}")]
    Io {
        context: &'static str,
        #[source]
        source: io::Error,
    },
}

impl LinkError {
    /// Whether the socket is probably unusable and should be re-opened
    /// (interface gone or down, bad descriptor), as opposed to a transient
    /// condition such as a full transmit queue.
    pub fn is_fatal(&self) -> bool {
        match self {
            LinkError::Io { source, .. } => matches!(
                source.raw_os_error(),
                Some(c) if is_fatal_errno(c)
            ),
            _ => true,
        }
    }
}

#[cfg(target_os = "linux")]
fn is_fatal_errno(c: i32) -> bool {
    [
        libc::ENODEV,
        libc::ENXIO,
        libc::ENETDOWN,
        libc::ENETUNREACH,
        libc::EBADF,
        libc::ENOTSOCK,
    ]
    .contains(&c)
}

#[cfg(not(target_os = "linux"))]
fn is_fatal_errno(_c: i32) -> bool {
    false
}

/// A raw Ethernet link to one interface. Frames are complete Ethernet frames,
/// header included.
pub trait Link: Send {
    /// Send every frame, in order. On return all of them have been handed to
    /// the kernel, or an error is returned (frames before the failure may
    /// have gone out).
    fn send(&mut self, frames: &[&[u8]]) -> Result<(), LinkError>;

    /// Wait up to `timeout` for a discovery reply and copy it into `buf`.
    /// Returns the frame length, or `None` on timeout. Frames that are not
    /// discovery replies may be filtered out by the implementation.
    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>, LinkError>;

    /// Wait `d` between two sends. A method of the link so that tests can
    /// record the pause instead of sleeping.
    fn pause(&mut self, d: Duration) {
        if !d.is_zero() {
            std::thread::sleep(d);
        }
    }
}

impl<L: Link + ?Sized> Link for Box<L> {
    fn send(&mut self, frames: &[&[u8]]) -> Result<(), LinkError> {
        (**self).send(frames)
    }
    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>, LinkError> {
        (**self).recv(buf, timeout)
    }
    fn pause(&mut self, d: Duration) {
        (**self).pause(d);
    }
}
