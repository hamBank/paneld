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

use thiserror::Error;

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

// ---------------------------------------------------------------------------
// 0x07 discovery request
// ---------------------------------------------------------------------------

/// Packet type byte of the discovery request.
pub const DISCOVERY_REQUEST_TYPE: u8 = 0x07;

/// Total on-wire length of a discovery request (14-byte header + 270 data
/// bytes). FPP: `CL_DISC_PACKET_SIZE` = 284. colorlight.rs: `14 + 270`.
/// hkubota: "Data length: 270". All three agree.
pub const DISCOVERY_REQUEST_LEN: usize = 284;

/// Absolute offset of the receiver index in a discovery request.
const DISCOVERY_RECEIVER_INDEX: usize = 16;

/// Discovery request (type `0x07`): "receiver number `receiver_index`, please
/// answer with a `0x08` frame". Every byte but the type and the index is zero.
///
/// FPP asks for index 0, then 1, 2, ... until nobody answers, which is how it
/// finds every card on a daisy chain.
///
/// # FPP vs colorlight.rs vs hkubota
///
/// colorlight.rs and hkubota describe a three-frame exchange: request, the
/// card's broadcast response, then an "ack" that is a `0x0700` frame with
/// `Data[2] = 1` (their `Data` starts after the EtherType, so that is
/// **absolute offset 16**). FPP documents offset 16 as the receiver index
/// ("0x00 first receiver, 0x01 second receiver, etc.") and writes it into
/// every request. Same byte, same frame type, same length. The "ack" is
/// simply the request for receiver index 1, and gbestwick's comment on
/// hkubota's post says the same ("controller number plus one"). So there is
/// **one** encoder: `encode_discovery_request(0)` is the request and
/// `encode_discovery_request(1)` is the ack / next query. Nothing else
/// differs.
pub fn encode_discovery_request(receiver_index: u8) -> [u8; DISCOVERY_REQUEST_LEN] {
    let mut f = [0u8; DISCOVERY_REQUEST_LEN];
    f[0..6].copy_from_slice(&DST_MAC);
    f[6..12].copy_from_slice(&SRC_MAC);
    f[12] = DISCOVERY_REQUEST_TYPE;
    // f[13] stays 0x00: FPP leaves it zero, colorlight.rs sets 0x00 and
    // hkubota's eth.type is 0x0700.
    f[DISCOVERY_RECEIVER_INDEX] = receiver_index;
    f
}

// ---------------------------------------------------------------------------
// 0x08 discovery response
// ---------------------------------------------------------------------------

/// Packet type byte of the discovery response.
pub const DISCOVERY_RESPONSE_TYPE: u8 = 0x08;

/// Full on-wire length of a discovery response. FPP: `CL_RESP_PACKET_SIZE`
/// = 1070; hkubota's capture is 1070 bytes (data length 1056 + 14).
/// [`DiscoveryResponse::decode`] does not need all of it.
pub const DISCOVERY_RESPONSE_LEN: usize = 1070;

/// Shortest buffer [`DiscoveryResponse::decode`] accepts: through the
/// receiver index at absolute offset 98.
pub const DISCOVERY_RESPONSE_MIN_LEN: usize = 99;

/// Value of the model byte (offset 13) on a 5A card. FPP: "0x05 - 5A card".
pub const MODEL_5A: u8 = 0x05;

const RESP_MODEL: usize = 13;
const RESP_FW_MAJOR: usize = 15;
const RESP_FW_MINOR: usize = 16;
const RESP_WIDTH: usize = 34;
const RESP_HEIGHT: usize = 36;
const RESP_PACKETS: usize = 51;
const RESP_UPTIME: usize = 59;
const RESP_RECEIVER_INDEX: usize = 98;

/// Firmware major version from which the display frame (`0x01`) and the
/// brightness frame (`0x0A`) must be sent twice.
pub const DOUBLE_SYNC_FIRMWARE_MAJOR: u8 = 13;

/// Whether a card running firmware `major` needs the `0x01` display frame and
/// the `0x0A` brightness frame sent **twice**, back to back. Firmware 13 and
/// later does; older firmware flickers if it gets two. Sending them once or
/// twice is the sink's job; this only answers the question.
///
/// Source: FPP's `ColorLight-5a-75.cpp` (`m_highestFirmwareVersion >= 13`),
/// FPP issue #1849 and commit `d49ad41`. No other reference knows about V13.
///
/// When several cards share an interface, FPP uses the highest major
/// version it saw, because it sends one stream to all of them.
pub const fn needs_double_sync(major: u8) -> bool {
    major >= DOUBLE_SYNC_FIRMWARE_MAJOR
}

/// Why a received frame could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DecodeError {
    /// The buffer is too short to contain the field that was needed.
    #[error("frame too short: {len} bytes, need at least {min}")]
    TooShort { len: usize, min: usize },
    /// The packet type byte (offset 12) is not the one expected.
    #[error("wrong packet type 0x{found:02x}, expected 0x{expected:02x}")]
    WrongType { found: u8, expected: u8 },
}

/// A decoded discovery response (type `0x08`): what a card tells us about
/// itself. Offsets are absolute; see `PROTOCOL.md` for the evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryResponse {
    /// Card model byte at offset 13 (the second EtherType byte). `0x05` is a
    /// 5A card, see [`MODEL_5A`]. Other values are passed through.
    pub model: u8,
    /// Firmware major version, e.g. 10 for "10.16" or 13 for "13.x".
    pub firmware_major: u8,
    /// Firmware minor version.
    pub firmware_minor: u8,
    /// Configured width in pixels. FPP calls this "cabinet width" and warns
    /// that it doesn't always match what you'd expect when a card is
    /// offset; hkubota's single capture (128x64 on a 128x64 setup) matches.
    pub width: u16,
    /// Configured height in pixels. Same caveat as `width`.
    pub height: u16,
    /// Index of the answering receiver (0 for the first card on the chain).
    pub receiver_index: u8,
    /// Receive packet counter, a status value. Its meaning is a guess in
    /// every reference ("received packet count?").
    pub packets_received: u32,
    /// Card uptime in milliseconds.
    pub uptime_ms: u32,
}

impl DiscoveryResponse {
    /// Decode a received frame, starting at the destination MAC. Only the
    /// type byte is checked: the caller decides whether the MACs matter (FPP
    /// requires source `11:22:33:44:55:66`; the card broadcasts to
    /// `ff:ff:ff:ff:ff:ff`).
    ///
    /// Frames longer than [`DISCOVERY_RESPONSE_MIN_LEN`] are fine; the rest
    /// is ignored.
    pub fn decode(frame: &[u8]) -> Result<Self, DecodeError> {
        if frame.len() < ETH_HEADER_LEN {
            return Err(DecodeError::TooShort {
                len: frame.len(),
                min: DISCOVERY_RESPONSE_MIN_LEN,
            });
        }
        if frame[12] != DISCOVERY_RESPONSE_TYPE {
            return Err(DecodeError::WrongType {
                found: frame[12],
                expected: DISCOVERY_RESPONSE_TYPE,
            });
        }
        if frame.len() < DISCOVERY_RESPONSE_MIN_LEN {
            return Err(DecodeError::TooShort {
                len: frame.len(),
                min: DISCOVERY_RESPONSE_MIN_LEN,
            });
        }
        let be16 = |at: usize| u16::from_be_bytes([frame[at], frame[at + 1]]);
        let be32 = |at: usize| {
            u32::from_be_bytes([frame[at], frame[at + 1], frame[at + 2], frame[at + 3]])
        };
        Ok(DiscoveryResponse {
            model: frame[RESP_MODEL],
            firmware_major: frame[RESP_FW_MAJOR],
            firmware_minor: frame[RESP_FW_MINOR],
            width: be16(RESP_WIDTH),
            height: be16(RESP_HEIGHT),
            receiver_index: frame[RESP_RECEIVER_INDEX],
            packets_received: be32(RESP_PACKETS),
            uptime_ms: be32(RESP_UPTIME),
        })
    }

    /// [`needs_double_sync`] for this card's firmware.
    pub const fn needs_double_sync(&self) -> bool {
        needs_double_sync(self.firmware_major)
    }
}

// ---------------------------------------------------------------------------
// 0x0A brightness
// ---------------------------------------------------------------------------

/// Packet type byte of the brightness frame.
pub const BRIGHTNESS_FRAME_TYPE: u8 = 0x0A;

/// Total on-wire length of a brightness frame. FPP: `CL_BRIG_PACKET_SIZE` =
/// 77. PanelPlayer: 12-byte header + 65-byte array = 77. hkubota: "Data
/// length: 63" after the EtherType, 14 + 63 = 77.
pub const BRIGHTNESS_FRAME_LEN: usize = 77;

/// Brightness frame (type `0x0A`). FPP sends it before the row packets of
/// every frame (twice on firmware >= 13, see [`needs_double_sync`]).
///
/// Wire layout: offsets 13, 14, 15 are the three levels, offset 16 is a
/// constant `0xff`, everything else is zero.
///
/// # Channel order
///
/// The three bytes are documented as **R, G, B** (PanelPlayer's
/// `colorlight_send_brightness(red, green, blue)` puts them at `[1], [2], [3]`).
/// FPP's Wireshark dissector labels them B, G, R instead. We side with
/// PanelPlayer: the dissector gives the same B, G, R labels to the `0x01`
/// frame's offsets 38-40, and hkubota's colour-temperature captures
/// (2000 K = `ff 76 06`) prove R, G, B there. Nothing captured shows the
/// `0x0A` order directly, though, so only [`BrightnessFrame::uniform`]
/// (all three equal, what FPP sends) is confirmed by two independent
/// references. Prefer it until the order has been seen on hardware; the
/// per-channel fields rest on PanelPlayer alone.
///
/// hkubota notes that LEDVision's percentage to byte mapping here is not
/// linear (1% = 0x28, 50% = 0xc1, 100% = 0xff), unlike the `0x01` frame.
/// FPP just scales percent by 2.55. We don't pick a curve here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrightnessFrame {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl BrightnessFrame {
    /// The same level on all three channels, as FPP sends.
    pub const fn uniform(level: u8) -> Self {
        BrightnessFrame {
            red: level,
            green: level,
            blue: level,
        }
    }

    /// The complete Ethernet frame, ready for an `AF_PACKET` socket.
    pub fn encode(&self) -> [u8; BRIGHTNESS_FRAME_LEN] {
        let mut f = [0u8; BRIGHTNESS_FRAME_LEN];
        f[0..6].copy_from_slice(&DST_MAC);
        f[6..12].copy_from_slice(&SRC_MAC);
        f[12] = BRIGHTNESS_FRAME_TYPE;
        f[13] = self.red;
        f[14] = self.green;
        f[15] = self.blue;
        // Unknown meaning; constant 0xff in FPP, PanelPlayer and hkubota
        // ("XXXXff00...").
        f[16] = 0xff;
        f
    }
}

// ---------------------------------------------------------------------------
// 0x55 row data
// ---------------------------------------------------------------------------

/// Packet type byte of a row data frame.
pub const ROW_FRAME_TYPE: u8 = 0x55;

/// Length of a row frame's header: Ethernet header plus the 7 bytes of row,
/// offset, count and constants.
pub const ROW_HEADER_LEN: usize = 21;

/// Most pixels one row packet may carry. FPP `CL_MAX_PIXL_PER_PACKET` and
/// PanelPlayer `MAX_PIXELS` both say 497 (497 * 3 + 21 = 1512 bytes, an
/// Ethernet payload of 1498).
pub const MAX_PIXELS_PER_PACKET: usize = 497;

/// Bytes per pixel on the wire.
pub const BYTES_PER_PIXEL: usize = 3;

/// Longest possible row frame.
pub const MAX_ROW_FRAME_LEN: usize = ROW_HEADER_LEN + MAX_PIXELS_PER_PACKET * BYTES_PER_PIXEL;

/// Why a row could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EncodeError {
    /// The pixel slice length is not a multiple of 3.
    #[error("pixel data is {len} bytes, not a whole number of 3-byte pixels")]
    PartialPixel { len: usize },
    /// More than [`MAX_PIXELS_PER_PACKET`] pixels for a single packet.
    #[error("{count} pixels do not fit one packet (max {MAX_PIXELS_PER_PACKET})")]
    TooManyPixels { count: usize },
    /// `card_offset + width` runs past pixel 65535, so a packet's 16-bit
    /// pixel offset could not name it.
    #[error("card offset {card_offset} + width {width} exceeds 65536 pixels")]
    OffsetOverflow { card_offset: u16, width: usize },
    /// The output buffer is smaller than the frame.
    #[error("output buffer is {len} bytes, frame needs {needed}")]
    BufferTooSmall { len: usize, needed: usize },
}

/// Header of a row frame (Ethernet header, type `0x55`, row, offset, count,
/// `0x08 0x88`). The pixel bytes follow it on the wire.
///
/// `row` is the card's row number. Row, offset and count are 16-bit
/// big-endian. FPP and PanelPlayer write the full 16 bits of the row at
/// offsets 13-14; colorlight.rs and hkubota describe the same byte as
/// "EtherType `0x5500` or `0x5501`" (the high byte of the row lives in the
/// EtherType's second byte), which is the same thing for rows below 512.
pub fn encode_row_header(
    row: u16,
    pixel_offset: u16,
    pixel_count: u16,
) -> Result<[u8; ROW_HEADER_LEN], EncodeError> {
    if pixel_count as usize > MAX_PIXELS_PER_PACKET {
        return Err(EncodeError::TooManyPixels {
            count: pixel_count as usize,
        });
    }
    let mut h = [0u8; ROW_HEADER_LEN];
    h[0..6].copy_from_slice(&DST_MAC);
    h[6..12].copy_from_slice(&SRC_MAC);
    h[12] = ROW_FRAME_TYPE;
    h[13..15].copy_from_slice(&row.to_be_bytes());
    h[15..17].copy_from_slice(&pixel_offset.to_be_bytes());
    h[17..19].copy_from_slice(&pixel_count.to_be_bytes());
    // Unknown meaning. FPP, PanelPlayer and colorlight.rs all send 0x08 0x88.
    // hkubota's text says "0x80 (I got 0x88)", and colorlight.rs's doc
    // comment says "0x80 or 0x88" but its code sends 0x88.
    h[19] = 0x08;
    h[20] = 0x88;
    Ok(h)
}

/// One row packet: a header and a borrowed slice of pixel bytes. Send them
/// as two `iovec`s (what FPP and PanelPlayer do), or call
/// [`RowPacket::write_to`] to get one contiguous frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowPacket<'a> {
    header: [u8; ROW_HEADER_LEN],
    pixels: &'a [u8],
}

impl<'a> RowPacket<'a> {
    /// Ethernet header plus row header, [`ROW_HEADER_LEN`] bytes.
    pub fn header(&self) -> &[u8; ROW_HEADER_LEN] {
        &self.header
    }

    /// The pixel bytes of this packet, in the caller's channel order.
    pub fn pixels(&self) -> &'a [u8] {
        self.pixels
    }

    /// Number of pixels in this packet (1 to [`MAX_PIXELS_PER_PACKET`]).
    pub fn pixel_count(&self) -> usize {
        self.pixels.len() / BYTES_PER_PIXEL
    }

    /// Total on-wire length.
    pub fn frame_len(&self) -> usize {
        ROW_HEADER_LEN + self.pixels.len()
    }

    /// Copy the whole frame into `out`; returns the bytes written.
    pub fn write_to(&self, out: &mut [u8]) -> Result<usize, EncodeError> {
        let needed = self.frame_len();
        if out.len() < needed {
            return Err(EncodeError::BufferTooSmall {
                len: out.len(),
                needed,
            });
        }
        out[..ROW_HEADER_LEN].copy_from_slice(&self.header);
        out[ROW_HEADER_LEN..needed].copy_from_slice(self.pixels);
        Ok(needed)
    }
}

/// Iterator over the packets of one row, made by [`row_packets`].
#[derive(Debug, Clone)]
pub struct RowPackets<'a> {
    row: u16,
    /// Pixel offset of the next packet. `row_packets` guarantees that every
    /// packet's start fits in 16 bits.
    next_offset: u32,
    rest: &'a [u8],
}

impl<'a> Iterator for RowPackets<'a> {
    type Item = RowPacket<'a>;

    fn next(&mut self) -> Option<RowPacket<'a>> {
        if self.rest.is_empty() {
            return None;
        }
        let take = self.rest.len().min(MAX_PIXELS_PER_PACKET * BYTES_PER_PIXEL);
        let (pixels, rest) = self.rest.split_at(take);
        let count = (take / BYTES_PER_PIXEL) as u16;
        let header = encode_row_header(self.row, self.next_offset as u16, count)
            .expect("pixel count is capped at 497");
        self.next_offset += count as u32;
        self.rest = rest;
        Some(RowPacket { header, pixels })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = (self.rest.len() / BYTES_PER_PIXEL).div_ceil(MAX_PIXELS_PER_PACKET);
        (n, Some(n))
    }
}

impl ExactSizeIterator for RowPackets<'_> {}

/// Split one row of pixels into packets of at most 497 pixels, in order,
/// without allocating.
///
/// * `row`: the card's row number for this row (the caller adds any vertical
///   offset).
/// * `card_offset`: the pixel offset of the row's first pixel on the card,
///   i.e. the region's x position. Each packet's pixel offset is
///   `card_offset + x`.
/// * `pixels`: the row, 3 bytes per pixel, already in the order the panel
///   wants (see `PROTOCOL.md`, "Pixel byte order").
///
/// A row of width 0 yields no packets. Fails if `pixels` is not a whole
/// number of pixels, or if `card_offset + width` exceeds 65536.
pub fn row_packets(
    row: u16,
    card_offset: u16,
    pixels: &[u8],
) -> Result<RowPackets<'_>, EncodeError> {
    if pixels.len() % BYTES_PER_PIXEL != 0 {
        return Err(EncodeError::PartialPixel { len: pixels.len() });
    }
    let width = pixels.len() / BYTES_PER_PIXEL;
    if card_offset as usize + width > 1 << 16 {
        return Err(EncodeError::OffsetOverflow { card_offset, width });
    }
    Ok(RowPackets {
        row,
        next_offset: card_offset as u32,
        rest: pixels,
    })
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

    // ---- 0x07 discovery request -----------------------------------------

    /// No capture of a request exists as a hex dump. hkubota gives "eth.type
    /// 0x0700, data length 270, data all zero"; colorlight.rs builds exactly
    /// that (14 + 270 bytes); FPP builds it with the receiver index at
    /// offset 16. This test spells the layout out by hand.
    #[test]
    fn discovery_request_for_receiver_0() {
        let f = encode_discovery_request(0);
        assert_eq!(f.len(), 284);
        assert_eq!(&f[0..6], &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        assert_eq!(&f[6..12], &[0x22, 0x22, 0x33, 0x44, 0x55, 0x66]);
        assert_eq!(&f[12..14], &[0x07, 0x00]);
        assert!(f[14..].iter().all(|&b| b == 0));
    }

    /// colorlight.rs's "ack" (`0x0700`, `Data[2] = 1`, base 14 so offset 16)
    /// is the request for receiver 1.
    #[test]
    fn discovery_request_for_receiver_1_is_the_colorlight_rs_ack() {
        let f = encode_discovery_request(1);
        assert_eq!(f[16], 1);
        let mut expected = [0u8; 284];
        expected[0..6].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        expected[6..12].copy_from_slice(&[0x22, 0x22, 0x33, 0x44, 0x55, 0x66]);
        expected[12] = 0x07;
        expected[14 + 2] = 1; // colorlight.rs: frame[14 + 2] = 1
        assert_eq!(f, expected);
    }

    #[test]
    fn discovery_request_index_is_only_difference() {
        let a = encode_discovery_request(0);
        let b = encode_discovery_request(7);
        let diffs = a.iter().zip(b.iter()).filter(|(x, y)| x != y).count();
        assert_eq!(diffs, 1);
        assert_eq!(b[16], 7);
    }

    // ---- 0x08 discovery response ----------------------------------------

    /// First 192 bytes of the receiver response captured from a 5A-75B
    /// running firmware 10.16 configured as 128x64, from hkubota's post
    /// (same URL as above). The real frame is 1070 bytes; the rest is not
    /// printed there.
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

    #[test]
    fn discovery_response_decodes_hkubota_capture() {
        let r = DiscoveryResponse::decode(&HKUBOTA_RESPONSE).unwrap();
        assert_eq!(
            r,
            DiscoveryResponse {
                model: MODEL_5A,
                firmware_major: 10,
                firmware_minor: 16,
                width: 128,
                height: 64,
                receiver_index: 0,
                packets_received: 0x0ca8,
                uptime_ms: 0x0008_05a0,
            }
        );
        assert!(!r.needs_double_sync());
    }

    /// Build a response from the documented offsets only, with values that
    /// differ in every byte so a wrong offset can't hide.
    fn synthetic_response() -> [u8; DISCOVERY_RESPONSE_LEN] {
        let mut f = [0u8; DISCOVERY_RESPONSE_LEN];
        f[0..6].copy_from_slice(&[0xff; 6]);
        f[6..12].copy_from_slice(&DST_MAC);
        f[12] = 0x08;
        f[13] = 0x05; // model, FPP data[0]
        f[15] = 13; // major, FPP data[2], hkubota Data[1]
        f[16] = 4; // minor, FPP data[3], hkubota Data[2]
        f[34..36].copy_from_slice(&[0x01, 0xac]); // width 428, FPP data[21..22]
        f[36..38].copy_from_slice(&[0x00, 0x56]); // height 86, FPP data[23..24]
        f[51..55].copy_from_slice(&[0x11, 0x22, 0x33, 0x44]); // FPP data[38..41]
        f[59..63].copy_from_slice(&[0xa1, 0xb2, 0xc3, 0xd4]); // FPP data[46..49]
        f[98] = 3; // receiver index, FPP data[85], gbestwick 0x62
        f
    }

    #[test]
    fn discovery_response_decodes_documented_offsets() {
        let r = DiscoveryResponse::decode(&synthetic_response()).unwrap();
        assert_eq!(r.model, 0x05);
        assert_eq!(r.firmware_major, 13);
        assert_eq!(r.firmware_minor, 4);
        assert_eq!(r.width, 428);
        assert_eq!(r.height, 86);
        assert_eq!(r.receiver_index, 3);
        assert_eq!(r.packets_received, 0x1122_3344);
        assert_eq!(r.uptime_ms, 0xa1b2_c3d4);
        assert!(r.needs_double_sync());
    }

    #[test]
    fn discovery_response_decodes_minimum_length_buffer() {
        let full = synthetic_response();
        let r = DiscoveryResponse::decode(&full[..DISCOVERY_RESPONSE_MIN_LEN]).unwrap();
        assert_eq!(r.receiver_index, 3);
    }

    #[test]
    fn discovery_response_rejects_short_buffers() {
        let full = synthetic_response();
        for len in 0..DISCOVERY_RESPONSE_MIN_LEN {
            assert_eq!(
                DiscoveryResponse::decode(&full[..len]),
                Err(DecodeError::TooShort {
                    len,
                    min: DISCOVERY_RESPONSE_MIN_LEN
                }),
                "len {len}"
            );
        }
    }

    #[test]
    fn discovery_response_rejects_other_types() {
        let mut f = synthetic_response();
        for ty in [0x00u8, 0x01, 0x07, 0x09, 0x55] {
            f[12] = ty;
            assert_eq!(
                DiscoveryResponse::decode(&f),
                Err(DecodeError::WrongType {
                    found: ty,
                    expected: 0x08
                })
            );
        }
        // A request is not a response.
        assert!(matches!(
            DiscoveryResponse::decode(&encode_discovery_request(0)),
            Err(DecodeError::WrongType { found: 0x07, .. })
        ));
    }

    #[test]
    fn decode_errors_have_messages() {
        use core::fmt::Write;
        struct Buf([u8; 96], usize);
        impl Write for Buf {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let b = s.as_bytes();
                self.0[self.1..self.1 + b.len()].copy_from_slice(b);
                self.1 += b.len();
                Ok(())
            }
        }
        let mut b = Buf([0; 96], 0);
        write!(
            b,
            "{}",
            DecodeError::WrongType {
                found: 7,
                expected: 8
            }
        )
        .unwrap();
        assert_eq!(&b.0[..b.1], b"wrong packet type 0x07, expected 0x08");
    }

    #[test]
    fn double_sync_starts_at_firmware_13() {
        assert!(!needs_double_sync(0));
        assert!(!needs_double_sync(11));
        assert!(!needs_double_sync(12));
        assert!(needs_double_sync(13));
        assert!(needs_double_sync(14));
        assert!(needs_double_sync(255));
    }

    // ---- 0x0A brightness -------------------------------------------------

    /// No hex dump of a `0x0A` frame exists. hkubota: "eth.type 0x0aXX,
    /// data length 63, data XXXXff00...00" (XX = brightness); FPP and
    /// PanelPlayer agree. 50% on LEDVision's curve is 0xc1 (hkubota).
    #[test]
    fn brightness_frame_matches_documented_layout() {
        let f = BrightnessFrame::uniform(0xc1).encode();
        assert_eq!(f.len(), 77);
        assert_eq!(
            &f[..20],
            &[
                0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x22, 0x22, 0x33, 0x44, 0x55, 0x66, //
                0x0a, 0xc1, 0xc1, 0xc1, 0xff, 0x00, 0x00, 0x00
            ]
        );
        assert!(f[17..].iter().all(|&b| b == 0));
    }

    #[test]
    fn brightness_frame_per_channel_order_is_rgb() {
        let f = BrightnessFrame {
            red: 1,
            green: 2,
            blue: 3,
        }
        .encode();
        assert_eq!(&f[12..17], &[0x0a, 1, 2, 3, 0xff]);
    }

    /// PanelPlayer's packet array has 65 bytes starting at the type byte,
    /// after a 12-byte MAC header; the total must match FPP's 77.
    #[test]
    fn brightness_frame_length_agrees_across_sources() {
        assert_eq!(BRIGHTNESS_FRAME_LEN, 12 + 65);
        assert_eq!(BRIGHTNESS_FRAME_LEN, 13 + 64); // FPP
        assert_eq!(BRIGHTNESS_FRAME_LEN, 14 + 63); // hkubota
    }

    // ---- 0x55 row data ---------------------------------------------------

    /// No hex dump of a row frame exists. Built by hand from the layout FPP
    /// and PanelPlayer share: type 0x55, row, offset, count (all u16 BE),
    /// 0x08 0x88, pixels. Row 300 = 0x012c, offset 1000 = 0x03e8.
    #[test]
    fn row_header_matches_documented_layout() {
        let h = encode_row_header(300, 1000, 2).unwrap();
        assert_eq!(
            h,
            [
                0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x22, 0x22, 0x33, 0x44, 0x55, 0x66, //
                0x55, 0x01, 0x2c, 0x03, 0xe8, 0x00, 0x02, 0x08, 0x88
            ]
        );
    }

    /// colorlight.rs and hkubota put the row's high byte in the EtherType:
    /// 0x5500 below row 256, 0x5501 from 256. Same bytes as FPP's 16-bit row.
    #[test]
    fn row_ethertype_is_0x5500_plus_row_msb() {
        let h = encode_row_header(255, 0, 1).unwrap();
        assert_eq!(u16::from_be_bytes([h[12], h[13]]), 0x5500);
        assert_eq!(h[14], 255);
        let h = encode_row_header(256, 0, 1).unwrap();
        assert_eq!(u16::from_be_bytes([h[12], h[13]]), 0x5501);
        assert_eq!(h[14], 0);
        let h = encode_row_header(0xfffe, 0, 1).unwrap();
        assert_eq!(&h[13..15], &[0xff, 0xfe]);
    }

    #[test]
    fn row_header_rejects_more_than_497_pixels() {
        assert!(encode_row_header(0, 0, 497).is_ok());
        assert_eq!(
            encode_row_header(0, 0, 498),
            Err(EncodeError::TooManyPixels { count: 498 })
        );
    }

    #[test]
    fn row_frame_length_matches_hkubota() {
        // "Data length: 391 byte (row length * 3 + 7)" for a 128 pixel row,
        // counted after the 14-byte Ethernet header.
        let px = [0u8; 128 * 3];
        let p = row_packets(0, 0, &px).unwrap().next().unwrap();
        assert_eq!(p.frame_len(), 14 + 391);
        assert_eq!(MAX_ROW_FRAME_LEN, 1512);
    }

    #[test]
    fn row_packet_write_to_is_header_then_pixels() {
        let px = [10, 20, 30, 40, 50, 60];
        let p = row_packets(5, 7, &px).unwrap().next().unwrap();
        let mut out = [0xeeu8; 40];
        assert_eq!(p.write_to(&mut out), Ok(27));
        assert_eq!(&out[..21], &encode_row_header(5, 7, 2).unwrap());
        assert_eq!(&out[21..27], &px);
        assert_eq!(out[27], 0xee, "must not write past the frame");
        assert_eq!(
            p.write_to(&mut out[..26]),
            Err(EncodeError::BufferTooSmall {
                len: 26,
                needed: 27
            })
        );
    }

    #[test]
    fn row_pixels_are_passed_through_unchanged() {
        // The protocol layer is byte-opaque: it never reorders channels.
        let px = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let p = row_packets(0, 0, &px).unwrap().next().unwrap();
        assert_eq!(p.pixels(), &px);
    }

    #[test]
    fn row_packets_reject_partial_pixels() {
        for len in [1usize, 2, 4, 5, 1490, 1492] {
            let px = [0u8; 1500];
            assert_eq!(
                row_packets(0, 0, &px[..len]).err(),
                Some(EncodeError::PartialPixel { len })
            );
        }
    }

    #[test]
    fn row_of_width_0_has_no_packets() {
        let mut it = row_packets(0, 0, &[]).unwrap();
        assert_eq!(it.len(), 0);
        assert!(it.next().is_none());
    }

    /// Reference model of FPP's splitting loop (offset/bytesInPacket
    /// arithmetic), written from its documented behaviour.
    fn fpp_split(width: usize) -> impl Iterator<Item = (usize, usize)> {
        let mut offset_px = 0usize;
        core::iter::from_fn(move || {
            if offset_px >= width {
                return None;
            }
            let n = (width - offset_px).min(497);
            let r = (offset_px, n);
            offset_px += n;
            Some(r)
        })
    }

    /// Pixel bytes that differ per position, so ordering mistakes show up.
    fn fill(buf: &mut [u8]) {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i as u32).wrapping_mul(31).wrapping_add(i as u32 >> 8) as u8;
        }
    }

    /// Check the contract for one (width, card_offset): packets cover the row
    /// exactly once, in order, none empty or over 497, headers decode back,
    /// and the split equals the FPP reference model.
    fn check_split(width: usize, card_offset: u16, buf: &[u8]) {
        let px = &buf[..width * 3];
        let it = row_packets(9, card_offset, px).unwrap();
        let expected_packets = width.div_ceil(497);
        assert_eq!(it.len(), expected_packets, "w {width}");

        let mut covered = 0usize; // pixels
        let mut count = 0usize;
        let mut reference = fpp_split(width);
        for p in it {
            let h = p.header();
            assert_eq!(
                &h[0..12],
                &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x22, 0x22, 0x33, 0x44, 0x55, 0x66]
            );
            assert_eq!(h[12], 0x55);
            assert_eq!(u16::from_be_bytes([h[13], h[14]]), 9);
            let off = u16::from_be_bytes([h[15], h[16]]) as usize;
            let cnt = u16::from_be_bytes([h[17], h[18]]) as usize;
            assert_eq!(&h[19..21], &[0x08, 0x88]);

            assert!((1..=497).contains(&cnt), "w {width}: count {cnt}");
            assert_eq!(cnt, p.pixel_count());
            assert_eq!(p.pixels().len(), cnt * 3);
            assert_eq!(
                off,
                card_offset as usize + covered,
                "w {width}: gap or overlap"
            );
            assert_eq!(p.pixels(), &px[covered * 3..(covered + cnt) * 3]);
            assert_eq!(reference.next(), Some((covered, cnt)));

            covered += cnt;
            count += 1;
        }
        assert_eq!(covered, width, "w {width}: not fully covered");
        assert_eq!(count, expected_packets);
        assert_eq!(reference.next(), None);
    }

    #[test]
    fn row_splitting_named_widths() {
        let mut buf = [0u8; 1500 * 3];
        fill(&mut buf);
        for width in [0usize, 1, 496, 497, 498, 994, 995, 1500] {
            for off in [0u16, 1, 172, 497] {
                check_split(width, off, &buf);
            }
        }
    }

    #[test]
    fn row_splitting_is_exact_at_boundaries() {
        let mut buf = [0u8; 1500 * 3];
        fill(&mut buf);
        let sizes = |w: usize| {
            let mut v = [0usize; 4];
            for (i, p) in row_packets(0, 0, &buf[..w * 3]).unwrap().enumerate() {
                v[i] = p.pixel_count();
            }
            v
        };
        assert_eq!(sizes(496), [496, 0, 0, 0]);
        assert_eq!(sizes(497), [497, 0, 0, 0]);
        assert_eq!(sizes(498), [497, 1, 0, 0]);
        assert_eq!(sizes(994), [497, 497, 0, 0]);
        assert_eq!(sizes(995), [497, 497, 1, 0]);
        assert_eq!(sizes(1500), [497, 497, 497, 9]);
    }

    /// Every width from 0 to 1500, with offsets from 0 up to the u16 limit.
    #[test]
    fn row_splitting_exhaustive_widths_and_offsets() {
        let mut buf = [0u8; 1500 * 3];
        fill(&mut buf);
        for width in 0..=1500usize {
            // Highest legal offset for this width, and the ones just below.
            let max = (65536 - width) as u32;
            for off in [
                0u32,
                1,
                496,
                497,
                498,
                40000,
                max.saturating_sub(2),
                max.saturating_sub(1),
                max,
            ] {
                if off <= max && off <= u16::MAX as u32 {
                    check_split(width, off as u16, &buf);
                }
            }
        }
    }

    #[test]
    fn row_offsets_near_u16_limit() {
        let mut buf = [0u8; 1500 * 3];
        fill(&mut buf);

        // Last pixel is exactly pixel 65535.
        let it = row_packets(0, 65535, &buf[..3]).unwrap();
        let p = it.last().unwrap();
        assert_eq!(&p.header()[15..17], &[0xff, 0xff]);

        // 1500 wide ending exactly at 65536: allowed, offsets never wrap.
        let off = 65536 - 1500;
        let last = row_packets(0, off as u16, &buf).unwrap().last().unwrap();
        let h = last.header();
        let o = u16::from_be_bytes([h[15], h[16]]) as usize;
        assert_eq!(o + last.pixel_count(), 65536);

        // One pixel further would wrap.
        assert_eq!(
            row_packets(0, off as u16 + 1, &buf).err(),
            Some(EncodeError::OffsetOverflow {
                card_offset: off as u16 + 1,
                width: 1500
            })
        );
        assert_eq!(
            row_packets(0, 65535, &buf[..6]).err(),
            Some(EncodeError::OffsetOverflow {
                card_offset: 65535,
                width: 2
            })
        );
        // A width-0 row never overflows.
        assert!(row_packets(0, 65535, &[]).is_ok());
    }

    /// 497 pixels is the largest packet: 1512 bytes, payload 1498 <= 1500.
    #[test]
    fn full_packet_fits_an_ethernet_mtu() {
        let buf = [0xabu8; 497 * 3];
        let p = row_packets(0, 0, &buf).unwrap().next().unwrap();
        assert_eq!(p.frame_len(), 1512);
        assert!(p.frame_len() - ETH_HEADER_LEN <= 1500);
        assert_eq!(&p.header()[17..19], &[0x01, 0xf1]);
    }

    #[test]
    fn card_offset_shifts_every_packet() {
        let mut buf = [0u8; 1000 * 3];
        fill(&mut buf);
        let mut it = row_packets(2, 172, &buf).unwrap();
        let a = it.next().unwrap();
        let b = it.next().unwrap();
        let c = it.next().unwrap();
        assert!(it.next().is_none());
        assert_eq!(&a.header()[15..19], &[0x00, 172, 0x01, 0xf1]);
        // 172 + 497 = 669 = 0x029d
        assert_eq!(&b.header()[15..19], &[0x02, 0x9d, 0x01, 0xf1]);
        // 172 + 994 = 1166 = 0x048e, and 1000 - 994 = 6 pixels left.
        assert_eq!(&c.header()[15..19], &[0x04, 0x8e, 0x00, 0x06]);
    }
}
