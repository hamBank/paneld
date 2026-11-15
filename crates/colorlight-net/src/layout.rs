//! What a discovered chain of receivers adds up to, and whether it fits the
//! card space the sink is about to send.
//!
//! Evidence and its limits are in `colorlight-proto/PROTOCOL.md`, "Multiple
//! receivers". The short version: pixel packets use one coordinate space for
//! the whole port, and the `0x08` reply gives each receiver's index,
//! firmware and a width/height pair, but **no source documents where in the
//! reply a receiver's x/y position is reported**. So nothing here computes a
//! bounding box of receiver regions; that needs confirmed offsets.

use colorlight_proto::{needs_double_sync, DiscoveryResponse};

/// A summary of the receivers [`crate::discover`] found.
///
/// The reported width/height of a receiver is not known to be its own cabinet
/// size: FPP's examples fit "the extent of the display up to and including
/// this receiver" (x offset + width, y offset + height) and its comment says
/// the values "don't seem totally correct" (PROTOCOL.md, "Multiple
/// receivers"). The largest value on each axis is the card space under that
/// reading, under "total display size", and for a single card under
/// "cabinet size", so it is what the summary offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Number of receivers that answered.
    pub receiver_count: usize,
    /// Per-axis maximum of the sizes the receivers report: the card space
    /// the port is configured for, on the reading described above (a
    /// heuristic, not confirmed). `None` with no receivers.
    pub reported_extent: Option<(u16, u16)>,
    /// Whether all receivers report the same size. Not an error: on the
    /// "extent" reading the first receivers of a chain report less.
    pub sizes_agree: bool,
    /// Highest firmware major on the chain (what the sink acts on).
    pub highest_firmware_major: Option<u8>,
    /// Indices of receivers on firmware that needs the doubled display frame
    /// (>= 13), and of those on older firmware. Both non-empty means mixed
    /// firmware.
    pub new_firmware: Vec<u8>,
    pub old_firmware: Vec<u8>,
}

impl Layout {
    pub fn from_receivers(receivers: &[DiscoveryResponse]) -> Layout {
        let extent = receivers.iter().fold(None, |acc: Option<(u16, u16)>, r| {
            Some(acc.map_or((r.width, r.height), |(w, h)| {
                (w.max(r.width), h.max(r.height))
            }))
        });
        let sizes_agree = receivers
            .windows(2)
            .all(|p| (p[0].width, p[0].height) == (p[1].width, p[1].height));
        let (new_firmware, old_firmware): (Vec<&DiscoveryResponse>, Vec<&DiscoveryResponse>) =
            receivers
                .iter()
                .partition(|r| needs_double_sync(r.firmware_major));
        Layout {
            receiver_count: receivers.len(),
            reported_extent: extent,
            sizes_agree,
            highest_firmware_major: receivers.iter().map(|r| r.firmware_major).max(),
            new_firmware: new_firmware.iter().map(|r| r.receiver_index).collect(),
            old_firmware: old_firmware.iter().map(|r| r.receiver_index).collect(),
        }
    }

    /// Whether the chain mixes firmware >= 13 with older firmware, which
    /// FPP and DESIGN.md 4.2 don't support (the sync count can't suit both).
    pub fn mixed_firmware(&self) -> bool {
        !self.new_firmware.is_empty() && !self.old_firmware.is_empty()
    }
}

/// A problem [`check_layout`] found. All are advisory: the receivers' size
/// fields are unreliable (FPP) and the sink keeps running.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Warning {
    /// The largest size the receivers report differs from the card space.
    #[error(
        "receivers report a card space of {}x{} but paneld sends {}x{}; check the LEDVision \
         settings if the picture is wrong",
        reported.0, reported.1, card_space.0, card_space.1
    )]
    SizeMismatch {
        reported: (u16, u16),
        card_space: (u32, u32),
    },
    /// Firmware >= 13 and older firmware on one chain.
    #[error(
        "receivers {new:?} run firmware >= 13 and receivers {old:?} older; paneld picks one sync \
         count for the whole port, so flash the chain to one version"
    )]
    MixedFirmware { new: Vec<u8>, old: Vec<u8> },
}

/// Compare what the receivers report with the `card_space` (width, height)
/// the sink sends, and look for a mixed-firmware chain. An empty list of
/// receivers gives no warnings: the caller already knows nobody answered.
pub fn check_layout(card_space: (u32, u32), receivers: &[DiscoveryResponse]) -> Vec<Warning> {
    let layout = Layout::from_receivers(receivers);
    let mut out = Vec::new();
    if let Some(reported) = layout.reported_extent {
        if (u32::from(reported.0), u32::from(reported.1)) != card_space {
            out.push(Warning::SizeMismatch {
                reported,
                card_space,
            });
        }
    }
    if layout.mixed_firmware() {
        out.push(Warning::MixedFirmware {
            new: layout.new_firmware,
            old: layout.old_firmware,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rx(index: u8, major: u8, w: u16, h: u16) -> DiscoveryResponse {
        DiscoveryResponse {
            model: colorlight_proto::MODEL_5A,
            firmware_major: major,
            firmware_minor: 0,
            width: w,
            height: h,
            receiver_index: index,
            packets_received: 0,
            uptime_ms: 0,
        }
    }

    #[test]
    fn no_receivers_no_warnings() {
        assert!(check_layout((64, 32), &[]).is_empty());
        let l = Layout::from_receivers(&[]);
        assert_eq!(l.receiver_count, 0);
        assert_eq!(l.reported_extent, None);
        assert!(!l.mixed_firmware());
    }

    #[test]
    fn one_matching_receiver_is_clean() {
        assert!(check_layout((128, 64), &[rx(0, 11, 128, 64)]).is_empty());
    }

    #[test]
    fn single_size_mismatch() {
        let w = check_layout((172, 172), &[rx(0, 11, 128, 64)]);
        assert_eq!(
            w,
            vec![Warning::SizeMismatch {
                reported: (128, 64),
                card_space: (172, 172)
            }]
        );
        assert!(w[0].to_string().contains("128x64"));
    }

    #[test]
    fn chain_reporting_the_same_total_is_clean() {
        let rs = [rx(0, 11, 64, 32), rx(1, 11, 64, 32), rx(2, 11, 64, 32)];
        assert!(check_layout((64, 32), &rs).is_empty());
        let l = Layout::from_receivers(&rs);
        assert_eq!(l.receiver_count, 3);
        assert_eq!(l.reported_extent, Some((64, 32)));
        assert!(l.sizes_agree);
    }

    /// FPP's example: receiver 0 reports 32x16, receiver 1 (offset 32,16)
    /// reports 64x32. The extent is the card space; no warning.
    #[test]
    fn growing_sizes_down_a_chain_give_their_maximum() {
        let rs = [rx(0, 11, 32, 16), rx(1, 11, 64, 32)];
        assert!(check_layout((64, 32), &rs).is_empty());
        let l = Layout::from_receivers(&rs);
        assert_eq!(l.reported_extent, Some((64, 32)));
        assert!(!l.sizes_agree);
        let w = check_layout((32, 32), &rs);
        assert_eq!(
            w,
            vec![Warning::SizeMismatch {
                reported: (64, 32),
                card_space: (32, 32)
            }]
        );
    }

    #[test]
    fn extent_is_taken_per_axis() {
        let rs = [rx(0, 11, 64, 16), rx(1, 11, 32, 32)];
        assert_eq!(Layout::from_receivers(&rs).reported_extent, Some((64, 32)));
    }

    #[test]
    fn mixed_firmware_names_the_receivers() {
        let rs = [rx(0, 11, 64, 32), rx(1, 13, 64, 32), rx(2, 12, 64, 32)];
        let w = check_layout((64, 32), &rs);
        assert_eq!(
            w,
            vec![Warning::MixedFirmware {
                new: vec![1],
                old: vec![0, 2]
            }]
        );
        let l = Layout::from_receivers(&rs);
        assert_eq!(l.highest_firmware_major, Some(13));
    }

    #[test]
    fn uniform_new_firmware_is_not_mixed() {
        let rs = [rx(0, 13, 64, 32), rx(1, 14, 64, 32)];
        assert!(check_layout((64, 32), &rs).is_empty());
    }
}
