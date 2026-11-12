//! Linux `AF_PACKET` / `SOCK_RAW` transport.
//!
//! One socket per interface, bound to that interface and to EtherType
//! `0x0805` (the discovery reply, see `colorlight-proto`). The protocol
//! filter only affects what we *receive*: a bound packet socket sends any
//! frame it is given. Our own outgoing frames are never `0x0805`, so they
//! are not echoed back.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use colorlight_proto::{is_card_discovery_reply, DISCOVERY_RESPONSE_ETHERTYPE};

use crate::{Link, LinkError};

/// Kernel limit on `iovec`s / messages per `sendmmsg` call.
const MAX_BATCH: usize = 1024;

/// Send buffer we ask for. A full frame of 172x172 pixels is ~90 KB; the
/// kernel clamps this to `net.core.wmem_max`.
const SEND_BUFFER: libc::c_int = 1 << 20;

/// An `AF_PACKET` raw socket bound to one interface.
pub struct RawLink {
    fd: OwnedFd,
    interface: String,
    iovs: Vec<libc::iovec>,
    msgs: Vec<libc::mmsghdr>,
}

// The raw pointers in `iovs`/`msgs` are scratch space rebuilt on every call
// and never dereferenced across calls.
unsafe impl Send for RawLink {}

impl std::fmt::Debug for RawLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawLink")
            .field("interface", &self.interface)
            .finish()
    }
}

fn io_err(context: &'static str) -> LinkError {
    LinkError::Io {
        context,
        source: io::Error::last_os_error(),
    }
}

impl RawLink {
    /// Open a raw socket on `interface` (e.g. `eth1`). Needs `CAP_NET_RAW`.
    pub fn open(interface: &str) -> Result<RawLink, LinkError> {
        let unknown = || LinkError::UnknownInterface(interface.to_owned());
        let name = CString::new(interface).map_err(|_| unknown())?;
        // SAFETY: `name` is a valid NUL-terminated string.
        let ifindex = unsafe { libc::if_nametoindex(name.as_ptr()) };
        if ifindex == 0 {
            return Err(unknown());
        }

        let proto = DISCOVERY_RESPONSE_ETHERTYPE.to_be();
        // SAFETY: plain syscall; the result is checked.
        let raw = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::c_int::from(proto),
            )
        };
        if raw < 0 {
            let e = io::Error::last_os_error();
            return Err(match e.raw_os_error() {
                Some(libc::EPERM) | Some(libc::EACCES) => LinkError::PermissionDenied {
                    interface: interface.to_owned(),
                },
                _ => LinkError::Io {
                    context: "creating AF_PACKET socket",
                    source: e,
                },
            });
        }
        // SAFETY: `raw` is a fresh descriptor we own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        // SAFETY: all-zero is a valid sockaddr_ll.
        let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        addr.sll_family = libc::AF_PACKET as libc::c_ushort;
        addr.sll_protocol = proto;
        addr.sll_ifindex = ifindex as libc::c_int;
        // SAFETY: `addr` outlives the call and the length matches its type.
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                std::ptr::addr_of!(addr).cast(),
                std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io_err("binding AF_PACKET socket to interface"));
        }

        // Best effort: a small default buffer only costs throughput.
        let send_buffer = SEND_BUFFER;
        // SAFETY: pointer and length describe one c_int.
        unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                std::ptr::addr_of!(send_buffer).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }

        Ok(RawLink {
            fd,
            interface: interface.to_owned(),
            iovs: Vec::new(),
            msgs: Vec::new(),
        })
    }

    /// The interface this link is bound to.
    pub fn interface(&self) -> &str {
        &self.interface
    }
}

impl Link for RawLink {
    fn send(&mut self, frames: &[&[u8]]) -> Result<(), LinkError> {
        for chunk in frames.chunks(MAX_BATCH) {
            self.iovs.clear();
            self.iovs.extend(chunk.iter().map(|f| libc::iovec {
                iov_base: f.as_ptr().cast_mut().cast(),
                iov_len: f.len(),
            }));
            self.msgs.clear();
            for iov in &mut self.iovs {
                // SAFETY: all-zero is a valid mmsghdr (null name/control).
                let mut m: libc::mmsghdr = unsafe { std::mem::zeroed() };
                m.msg_hdr.msg_iov = iov;
                m.msg_hdr.msg_iovlen = 1;
                self.msgs.push(m);
            }

            let mut sent = 0;
            while sent < self.msgs.len() {
                // SAFETY: `msgs[sent..]` points at valid headers whose iovecs
                // point into `chunk`, which outlives the call. The kernel
                // only reads the buffers.
                let n = unsafe {
                    libc::sendmmsg(
                        self.fd.as_raw_fd(),
                        self.msgs[sent..].as_mut_ptr(),
                        (self.msgs.len() - sent) as libc::c_uint,
                        0,
                    )
                };
                if n < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(LinkError::Io {
                        context: "sending frames",
                        source: e,
                    });
                }
                sent += n as usize;
            }
        }
        Ok(())
    }

    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<usize>, LinkError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let ms = left.as_micros().div_ceil(1000).min(i32::MAX as u128) as libc::c_int;
            let mut p = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            let rc = unsafe { libc::poll(&mut p, 1, ms) };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(LinkError::Io {
                    context: "waiting for a reply",
                    source: e,
                });
            }
            if rc == 0 {
                return Ok(None);
            }
            // SAFETY: `buf` is valid for `buf.len()` bytes.
            let n =
                unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) {
                    continue;
                }
                return Err(LinkError::Io {
                    context: "receiving a reply",
                    source: e,
                });
            }
            let n = n as usize;
            if is_card_discovery_reply(&buf[..n.min(buf.len())]) {
                return Ok(Some(n));
            }
            // Not from the card: keep waiting for the rest of the timeout.
            if deadline <= std::time::Instant::now() {
                return Ok(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_interface_is_reported_before_privileges_matter() {
        match RawLink::open("definitely-not-a-nic0") {
            Err(LinkError::UnknownInterface(n)) => assert_eq!(n, "definitely-not-a-nic0"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn interface_name_with_nul_is_unknown() {
        assert!(matches!(
            RawLink::open("a\0b"),
            Err(LinkError::UnknownInterface(_))
        ));
    }
}
