//! Non-Linux stub: same API as the Linux `RawLink`, but it can't be built.

use std::convert::Infallible;
use std::time::Duration;

use crate::{Link, LinkError};

/// Placeholder for the Linux raw socket. [`RawLink::open`] always fails, so
/// no value of this type exists.
#[derive(Debug)]
pub struct RawLink(Infallible);

impl RawLink {
    /// Always fails with [`LinkError::Unsupported`].
    pub fn open(_interface: &str) -> Result<RawLink, LinkError> {
        Err(LinkError::Unsupported)
    }

    pub fn interface(&self) -> &str {
        match self.0 {}
    }
}

impl Link for RawLink {
    fn send(&mut self, _frames: &[&[u8]]) -> Result<(), LinkError> {
        match self.0 {}
    }

    fn recv(&mut self, _buf: &mut [u8], _timeout: Duration) -> Result<Option<usize>, LinkError> {
        match self.0 {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_is_unsupported() {
        assert!(matches!(RawLink::open("eth1"), Err(LinkError::Unsupported)));
    }
}
