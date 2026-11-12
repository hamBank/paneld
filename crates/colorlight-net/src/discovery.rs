//! Card discovery, following FPP's approach (PROTOCOL.md, "0x07 Discovery
//! request"): ask receiver 0, then 1, 2, ... and stop at the first index
//! that doesn't answer.
//!
//! FPP's retry policy: index 0 gets up to three attempts, the last one
//! sending the request three times in a row (a card that has just been
//! powered or is busy sometimes misses a single request). We give later
//! indices two attempts, so one lost frame doesn't hide the rest of a chain.

use std::time::{Duration, Instant};

use colorlight_proto::{
    encode_discovery_request, is_card_discovery_reply, DiscoveryResponse, DISCOVERY_RESPONSE_LEN,
};

use crate::{Link, LinkError};

/// Most receivers asked for (FPP's limit; the index is one byte on the wire).
pub const MAX_RECEIVERS: u8 = 32;

const ATTEMPTS_FIRST: u32 = 3;
const ATTEMPTS_NEXT: u32 = 2;

/// Find the cards on `link`. `timeout` is how long to wait for an answer
/// after each request, so with no card present this takes about
/// `3 * timeout`. Replies are returned in receiver-index order; an empty
/// list means nobody answered.
///
/// A reply counts for index `n` only if its own receiver-index field (offset
/// 98) says `n`. That discards the duplicate answers that the triple request
/// provokes, which would otherwise look like a card at the next index.
pub fn discover<L: Link + ?Sized>(
    link: &mut L,
    timeout: Duration,
) -> Result<Vec<DiscoveryResponse>, LinkError> {
    let mut found = Vec::new();
    for index in 0..MAX_RECEIVERS {
        let attempts = if index == 0 {
            ATTEMPTS_FIRST
        } else {
            ATTEMPTS_NEXT
        };
        let request = encode_discovery_request(index);
        let mut reply = None;
        for attempt in 0..attempts {
            let copies = if index == 0 && attempt + 1 == attempts {
                3
            } else {
                1
            };
            let frames = [&request[..]; 3];
            link.send(&frames[..copies])?;
            reply = wait_for(link, index, timeout)?;
            if reply.is_some() {
                break;
            }
        }
        match reply {
            Some(r) => found.push(r),
            None => break,
        }
    }
    Ok(found)
}

/// Wait up to `timeout` for the reply of receiver `index`.
fn wait_for<L: Link + ?Sized>(
    link: &mut L,
    index: u8,
    timeout: Duration,
) -> Result<Option<DiscoveryResponse>, LinkError> {
    let deadline = Instant::now() + timeout;
    let mut buf = vec![0u8; DISCOVERY_RESPONSE_LEN + 64];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let Some(n) = link.recv(&mut buf, left)? else {
            return Ok(None);
        };
        let frame = &buf[..n];
        if is_card_discovery_reply(frame) {
            match DiscoveryResponse::decode(frame) {
                Ok(r) if r.receiver_index == index => return Ok(Some(r)),
                Ok(r) => tracing::debug!(
                    wanted = index,
                    got = r.receiver_index,
                    "ignoring discovery reply for another receiver"
                ),
                Err(e) => tracing::debug!(error = %e, "ignoring undecodable discovery reply"),
            }
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{Event, MockLink};
    use colorlight_proto::{DISCOVERY_REQUEST_LEN, MODEL_5A};

    /// First 192 bytes of the 0x0805 response in hkubota's capture
    /// (firmware 10.16, 128x64, receiver 0). Same bytes as the
    /// `colorlight-proto` test fixture; source:
    /// https://hkubota.wordpress.com/2022/01/31/winter-project-colorlight-5a-75b-protocol/
    #[rustfmt::skip]
    const HKUBOTA_RESPONSE: [u8; 192] = [
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x08, 0x05, 0x04, 0x0a,
        0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x80, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0xff,
        0xff, 0xff, 0xff, 0x00, 0x00, 0x0c, 0xa8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x05, 0xa0, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xba, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0f, 0x04, 0x08, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0x28, 0x00, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x0c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    /// The capture, answering as receiver `index` with firmware `major`.
    fn reply_as(index: u8, major: u8) -> Vec<u8> {
        let mut f = HKUBOTA_RESPONSE.to_vec();
        f[98] = index;
        f[15] = major;
        f
    }

    /// A fake chain of `majors.len()` cards. Counts requests per index.
    fn chain(majors: Vec<u8>) -> MockLink {
        MockLink::with_responder(move |frame| {
            if frame.len() == DISCOVERY_REQUEST_LEN && frame[12] == 0x07 {
                let i = frame[16];
                if let Some(&m) = majors.get(usize::from(i)) {
                    return vec![reply_as(i, m)];
                }
            }
            vec![]
        })
    }

    fn requests(link: &MockLink) -> Vec<u8> {
        link.frames().iter().map(|f| f[16]).collect()
    }

    const T: Duration = Duration::from_millis(5);

    #[test]
    fn replays_the_hkubota_capture() {
        let mut link = MockLink::with_responder(|_| vec![HKUBOTA_RESPONSE.to_vec()]);
        let found = discover(&mut link, T).unwrap();
        // The capture answers every request as receiver 0, so only index 0
        // is accepted and index 1 finds nothing.
        assert_eq!(found.len(), 1);
        let r = found[0];
        assert_eq!(r.model, MODEL_5A);
        assert_eq!((r.firmware_major, r.firmware_minor), (10, 16));
        assert_eq!((r.width, r.height), (128, 64));
        assert_eq!(r.receiver_index, 0);
        assert!(!r.needs_double_sync());
        // One request for index 0 (answered at once), then two attempts at
        // index 1 (the stale index-0 duplicates are ignored).
        assert_eq!(requests(&link), vec![0, 1, 1]);
    }

    #[test]
    fn walks_a_chain_until_the_first_silence() {
        let mut link = chain(vec![11, 13, 13]);
        let found = discover(&mut link, T).unwrap();
        let majors: Vec<_> = found.iter().map(|r| r.firmware_major).collect();
        assert_eq!(majors, vec![11, 13, 13]);
        let idx: Vec<_> = found.iter().map(|r| r.receiver_index).collect();
        assert_eq!(idx, vec![0, 1, 2]);
        // 0, 1, 2 answered first time; index 3 is tried twice.
        assert_eq!(requests(&link), vec![0, 1, 2, 3, 3]);
    }

    #[test]
    fn no_card_means_empty_after_fpp_style_retries() {
        let mut link = MockLink::new();
        assert!(discover(&mut link, T).unwrap().is_empty());
        // Index 0: attempt 1, attempt 2, then the last attempt sends 3 copies.
        assert_eq!(requests(&link), vec![0, 0, 0, 0, 0]);
        assert!(link
            .events
            .iter()
            .all(|e| matches!(e, Event::Frame(f) if f.len() == DISCOVERY_REQUEST_LEN)));
    }

    #[test]
    fn a_lost_first_request_is_retried() {
        let mut seen = 0;
        let mut link = MockLink::with_responder(move |frame| {
            if frame[16] == 0 {
                seen += 1;
                if seen == 1 {
                    return vec![]; // dropped
                }
                return vec![reply_as(0, 11)];
            }
            vec![]
        });
        let found = discover(&mut link, T).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].firmware_major, 11);
        assert_eq!(requests(&link), vec![0, 0, 1, 1]);
    }

    #[test]
    fn foreign_frames_are_ignored() {
        let mut from_elsewhere = reply_as(0, 11);
        from_elsewhere[6] = 0xaa; // not the card's MAC
        let mut not_a_reply = reply_as(0, 11);
        not_a_reply[12] = 0x55;
        let truncated = reply_as(0, 11)[..50].to_vec();
        let mut link = MockLink::with_responder(move |frame| {
            if frame[16] == 0 {
                vec![
                    from_elsewhere.clone(),
                    not_a_reply.clone(),
                    truncated.clone(),
                    reply_as(0, 13),
                ]
            } else {
                vec![]
            }
        });
        let found = discover(&mut link, T).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].firmware_major, 13);
        assert!(found[0].needs_double_sync());
    }

    #[test]
    fn send_errors_are_propagated() {
        let mut link = MockLink::new();
        link.fail_next_sends(1, true);
        let e = discover(&mut link, T).unwrap_err();
        assert!(e.is_fatal());
    }
}
