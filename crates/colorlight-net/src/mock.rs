//! In-memory [`Link`] for tests: records what is sent, replays replies.

use std::collections::VecDeque;
use std::io;
use std::time::Duration;

use crate::{Link, LinkError};

/// Something a [`MockLink`] was asked to do, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// One frame sent.
    Frame(Vec<u8>),
    /// A [`Link::pause`].
    Pause(Duration),
}

type Responder = Box<dyn FnMut(&[u8]) -> Vec<Vec<u8>> + Send>;

/// A link that never touches the network.
///
/// * Every sent frame is appended to [`MockLink::events`].
/// * A `responder` (see [`MockLink::with_responder`]) is called for each sent
///   frame and may queue replies, which `recv` returns in order.
/// * [`MockLink::fail_next_sends`] makes the next sends fail.
#[derive(Default)]
pub struct MockLink {
    pub events: Vec<Event>,
    inbox: VecDeque<Vec<u8>>,
    responder: Option<Responder>,
    fail_sends: usize,
    fail_fatal: bool,
}

impl MockLink {
    pub fn new() -> Self {
        Self::default()
    }

    /// A link whose "card" answers sent frames with `f(frame)`.
    pub fn with_responder(f: impl FnMut(&[u8]) -> Vec<Vec<u8>> + Send + 'static) -> Self {
        MockLink {
            responder: Some(Box::new(f)),
            ..Self::default()
        }
    }

    /// Queue a frame for `recv` to return.
    pub fn push_reply(&mut self, frame: Vec<u8>) {
        self.inbox.push_back(frame);
    }

    /// Make the next `n` calls to `send` fail (with an error that is fatal
    /// if `fatal`, else transient). Nothing is recorded for failed calls.
    pub fn fail_next_sends(&mut self, n: usize, fatal: bool) {
        self.fail_sends = n;
        self.fail_fatal = fatal;
    }

    /// The sent frames, without pauses.
    pub fn frames(&self) -> Vec<&[u8]> {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::Frame(f) => Some(f.as_slice()),
                Event::Pause(_) => None,
            })
            .collect()
    }
}

impl Link for MockLink {
    fn send(&mut self, frames: &[&[u8]]) -> Result<(), LinkError> {
        if self.fail_sends > 0 {
            self.fail_sends -= 1;
            let code = if self.fail_fatal { 19 } else { 105 }; // ENODEV, ENOBUFS (Linux)
            return Err(LinkError::Io {
                context: "mock send",
                source: io::Error::from_raw_os_error(code),
            });
        }
        for f in frames {
            self.events.push(Event::Frame(f.to_vec()));
            if let Some(r) = self.responder.as_mut() {
                self.inbox.extend(r(f));
            }
        }
        Ok(())
    }

    fn recv(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<Option<usize>, LinkError> {
        match self.inbox.pop_front() {
            Some(f) => {
                let n = f.len().min(buf.len());
                buf[..n].copy_from_slice(&f[..n]);
                Ok(Some(n))
            }
            None => Ok(None),
        }
    }

    fn pause(&mut self, d: Duration) {
        self.events.push(Event::Pause(d));
    }
}
