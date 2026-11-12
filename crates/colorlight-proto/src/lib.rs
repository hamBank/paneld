//! Packet encoding for Colorlight 5A-75B receiver cards. Pure functions, no
//! I/O, `no_std`.
//!
//! Every layout here is recorded with its sources in `PROTOCOL.md` and pinned
//! by byte-exact tests. Offsets are **absolute** (from the first byte of the
//! Ethernet frame) because the references disagree on where they start
//! counting; `PROTOCOL.md` has a conversion table.
//!
//! Frames are raw Ethernet II. The card uses the 2-byte EtherType field as
//! *packet type + first payload byte*, so e.g. the display frame's "EtherType"
//! `0x0107` is type `0x01` followed by payload byte `0x07`.

#![no_std]

/// Destination MAC every reference sends to.
pub const DST_MAC: [u8; 6] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
/// Source MAC every reference sends from (and the card replies from).
pub const SRC_MAC: [u8; 6] = [0x22, 0x22, 0x33, 0x44, 0x55, 0x66];

/// Length of the Ethernet header (dst MAC, src MAC, EtherType).
pub const ETH_HEADER_LEN: usize = 14;

/// Packet type byte of the display frame.
pub const DISPLAY_FRAME_TYPE: u8 = 0x01;

/// Second EtherType byte of the display frame. See [`DisplayFrame`].
pub const DISPLAY_FRAME_SENDER_PC: u8 = 0x07;

/// Total on-wire length of a display frame (14-byte header + 98 data bytes).
pub const DISPLAY_FRAME_LEN: usize = 112;

const BRIGHTNESS: usize = 35;
const MAGIC_05: usize = 36;
const CHANNEL_R: usize = 38;
const CHANNEL_G: usize = 39;
const CHANNEL_B: usize = 40;

/// Display frame (type `0x01`): latch the rows sent since the last display
/// frame onto the LEDs, at the given brightness. Sent once per video frame
/// after all row packets (twice on firmware ≥ 13, which is the caller's job;
/// see DESIGN.md §4.2).
///
/// # The `0x07` after the type byte
///
/// We send `0x01, 0x07` (EtherType `0x0107`), the value LEDVision sends from
/// a PC/network card. FPP's `ColorLight-5a-75.cpp` sends the same, as do
/// PanelPlayer's `colorlight.c`, colorlight.rs and hkubota's captures.
///
/// **PanelPlayer's README is the odd one out:** it says the protocol uses
/// "EtherType `0x0101`". Its own code does not do that: `colorlight.c`
/// builds the packet as `{[0] = 0x01, [1] = 0x07, ...}`, i.e. `0x0107`, in
/// both ZoidTechnology/PanelPlayer and the ivancmz fork. Treat the README
/// figure as a documentation error, not a protocol variant.
///
/// The one real variant on record: FPP notes that Colorlight's S2 hardware
/// sender puts `0x00` here (and `0x00` instead of `0x05` at byte 36), and
/// that receivers seem to accept both. We don't emulate the S2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayFrame {
    /// Overall brightness, linear 0–255.
    pub brightness: u8,
    /// Per-channel brightness, used for colour temperature. `0xff` each for
    /// neutral (6500 K per hkubota's LEDVision captures).
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl DisplayFrame {
    /// Neutral colour temperature at the given brightness, the same as
    /// FPP sends (it puts `brightness` in all four fields).
    pub const fn neutral(brightness: u8) -> Self {
        DisplayFrame {
            brightness,
            red: brightness,
            green: brightness,
            blue: brightness,
        }
    }

    /// The complete Ethernet frame, ready for an `AF_PACKET` socket.
    pub fn encode(&self) -> [u8; DISPLAY_FRAME_LEN] {
        let mut f = [0u8; DISPLAY_FRAME_LEN];
        f[0..6].copy_from_slice(&DST_MAC);
        f[6..12].copy_from_slice(&SRC_MAC);
        f[12] = DISPLAY_FRAME_TYPE;
        f[13] = DISPLAY_FRAME_SENDER_PC;
        f[BRIGHTNESS] = self.brightness;
        // Unknown meaning; constant 0x05 from LEDVision, FPP, PanelPlayer
        // and colorlight.rs.
        f[MAGIC_05] = 0x05;
        f[CHANNEL_R] = self.red;
        f[CHANNEL_G] = self.green;
        f[CHANNEL_B] = self.blue;
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LEDVision display frame at 75% brightness, from the hex dump in
    /// hkubota, "Winter project: Colorlight 5A-75B protocol" (2022-01-31):
    /// https://hkubota.wordpress.com/2022/01/31/winter-project-colorlight-5a-75b-protocol/
    #[rustfmt::skip]
    const HKUBOTA_DISPLAY_75PCT: [u8; DISPLAY_FRAME_LEN] = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x22, 0x22, 0x33, 0x44, 0x55, 0x66, 0x01, 0x07, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0xbf, 0x05, 0x00, 0xbf, 0xbf, 0xbf, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn display_frame_matches_ledvision_capture() {
        assert_eq!(DisplayFrame::neutral(0xbf).encode(), HKUBOTA_DISPLAY_75PCT);
    }

    #[test]
    fn display_frame_ethertype_is_0x0107() {
        let f = DisplayFrame::neutral(0xff).encode();
        assert_eq!(u16::from_be_bytes([f[12], f[13]]), 0x0107);
    }

    /// Channel order is R, G, B: hkubota's 2000 K capture is ff 76 06
    /// (warm = red high, blue low), and PanelPlayer/colorlight.rs agree.
    #[test]
    fn display_frame_channel_order_is_rgb() {
        let f = DisplayFrame {
            brightness: 0xff,
            red: 0xff,
            green: 0x76,
            blue: 0x06,
        }
        .encode();
        assert_eq!(&f[35..41], &[0xff, 0x05, 0x00, 0xff, 0x76, 0x06]);
    }

    #[test]
    fn display_frame_length_is_14_plus_98() {
        assert_eq!(DISPLAY_FRAME_LEN, ETH_HEADER_LEN + 98);
    }
}
