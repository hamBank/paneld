# Colorlight 5A-75B protocol

Confirmed packet layouts, with sources. Only what is listed here may be encoded in `colorlight-proto`. Every entry needs at least two agreeing references and a byte-exact test. Where a byte rests on one reference, the entry says so and "Still uncertain" lists it.

All sources were read as raw text (raw GitHub files, the blog's HTML with tags stripped), not from summaries. The haraldkubota/colorlight repo was not used: its README only links to the blog post, and the GitHub API was unavailable.

## Sources

| Key | Reference | Licence |
|---|---|---|
| HK | hkubota, "Winter project: Colorlight 5A-75B protocol", 2022-01-31 (includes LEDVision captures): https://hkubota.wordpress.com/2022/01/31/winter-project-colorlight-5a-75b-protocol/ | blog post |
| FPP | FalconChristmas/fpp `src/channeloutput/ColorLight-5a-75.{cpp,h}` (master, read 2026-11-12), V13 fix in commit `d49ad41`, issue https://github.com/FalconChristmas/fpp/issues/1849 | GPL-2.0: facts only, no code copied |
| PP | ZoidTechnology/PanelPlayer and the ivancmz fork, `source/colorlight.c`: https://github.com/ivancmz/PanelPlayer | see repo |
| CLRS | asamonik/colorlight.rs `src/lib.rs`: https://github.com/asamonik/colorlight.rs | see repo |
| DISS | FPP's Wireshark dissector, https://github.com/FalconChristmas/fpp/blob/master/docs/ColorLight_Wireshark.lua (read 2026-11-12). Part of FPP, so GPL-2.0: facts only. Its field labels are unverified, see "Disagreements". | GPL-2.0 |
| GB | gbestwick's comment (2023-09-16) under the HK post | blog comment |

## Offset conventions

The references count offsets from different points. This document and the code use **absolute** offsets from the start of the Ethernet frame.

| Reference | Index base | Absolute = |
|---|---|---|
| HK "Data[n]", CLRS `frame[14 + n]` | after the EtherType | n + 14 |
| FPP `data[n]` | second EtherType byte (`CL_PACKET_DATA_OFFSET` = 13) | n + 13 |
| PP `packet[n]` | first EtherType byte (packet type) | n + 12 |
| DISS `buffer(data_offset + n)` | `data_offset` = 13, `buffer` starts at the destination MAC | n + 13 |

Worked example: the firmware major version is FPP `data[2]` (2 + 13), HK `Data[1]` (1 + 14) and CLRS `data[1]` with `data = &packet[14..]` (1 + 14). That is offset 15 in all three.

## Ethernet framing (all sent packets)

| Offset | Value | Sources |
|---|---|---|
| 0–5 | dst MAC `11:22:33:44:55:66` | HK, FPP, PP, CLRS |
| 6–11 | src MAC `22:22:33:44:55:66` | HK, FPP, PP, CLRS |
| 12 | packet type | HK, FPP, PP, CLRS |
| 13 | first payload byte, sent in the EtherType field | HK, FPP, PP, CLRS |

## 0x01 Display frame (EtherType `0x0107`)

Latches the rows sent so far onto the LEDs. Total length 112 bytes.

| Offset | Value | Meaning | Sources |
|---|---|---|---|
| 12 | `0x01` | type | all |
| 13 | `0x07` | sender kind: `0x07` = PC/network card (LEDVision). FPP: S2 hardware sender sends `0x00`. | HK, FPP, PP code, CLRS |
| 35 | 0–255 | brightness, linear | HK (Data[21]), FPP (data[22]), PP (packet[23]), CLRS |
| 36 | `0x05` | unknown, constant. FPP: S2 sends `0x00`. | HK, FPP, PP, CLRS |
| 38, 39, 40 | 0–255 each | R, G, B channel brightness (colour temperature) | HK, PP, CLRS. FPP sends brightness in all three |
| other | `0x00` | | HK capture |

**EtherType `0x0101`:** PanelPlayer's README says the protocol uses "EtherType `0x0101`", but PanelPlayer's code sends `0x01, 0x07`. No reference sends `0x0101`. We follow the code and the captures (`0x0107`). See the doc comment on `DisplayFrame`.

**Channel order:** R, G, B. HK's 2000 K capture is `ff 76 06` (warm white = red high, blue low), and PP and CLRS name them `r, g, b`. FPP's Wireshark dissector (DISS, read directly) does label offsets 38, 39, 40 as B, G, R. That contradicts HK's capture (blue-high would not be 2000 K) and PP and CLRS, so we treat the dissector's labels as wrong; the same labels appear for `0x0A`, see below.

**Firmware ≥ 13:** send the display frame twice, back to back, and once on older firmware (FPP, issue #1849). This is the sink's job, not the encoder's. See `DESIGN.md` §4.2.

Test: `display_frame_matches_ledvision_capture` (HK hex dump at 75% brightness, byte-for-byte).

## 0x07 Discovery request (EtherType `0x0700`)

Asks receiver number N to describe itself. Total length 284 bytes. Encoder: `encode_discovery_request(index)`.

| Offset | Value | Meaning | Sources |
|---|---|---|---|
| 12 | `0x07` | type | HK, FPP, CLRS, DISS |
| 13 | `0x00` | zero | HK (eth.type `0x0700`), FPP (never set), CLRS |
| 16 | 0, 1, 2, ... | receiver index (0 = first card) | FPP (`data[3]`, `discoverBuffer[16] = r`), DISS (`data_offset + 3`), HK/CLRS "ack" (`Data[2]` = 1, see below), GB |
| all others | `0x00` | | HK ("Data: 00000000...00"), FPP, CLRS |
| length | 284 | | FPP `CL_DISC_PACKET_SIZE`, CLRS `14 + 270`, HK "Data length: 270" |

**FPP vs colorlight.rs:** colorlight.rs (and HK) describe a three-frame exchange: request, broadcast response, then an "ack" that is a `0x0700` frame with `Data[2] = 1`. That is offset 16, the same byte FPP uses as the receiver index, in a frame of the same length. The "ack" is the request for receiver 1, and GB's comment on HK says "controller number plus one". So there is one encoder and no separate ack. FPP's loop sends index 0, then 1, 2, ... until no card answers (it retries index 0 twice, and on the last try sends it three times; that is sink policy).

Tests: `discovery_request_*`. No hex dump of a request exists, so the byte-exact test is written from the layout above, and the colorlight.rs ack is reproduced from its own `frame[14 + 2] = 1`.

## 0x08 Discovery response (EtherType `0x0805`)

Sent by the card, broadcast (dst `ff:ff:ff:ff:ff:ff`, src `11:22:33:44:55:66`; FPP filters on that source MAC, `CARD_MAC`, and on type `0x08`: `is_card_discovery_reply`). Full length 1070 bytes.

**EtherType to receive on:** `0x0805` (`DISCOVERY_RESPONSE_ETHERTYPE`). Sources: FPP opens its receive socket with `htons(0x0805)`, CLRS names it `DETECT_RECEIVER_RSP = 0x0805`, and HK's capture has `08 05` at offset 12. The second byte is the model (see below), so this value is specific to 5A cards; no reference documents other models' replies. `colorlight-net` binds its raw socket to this EtherType. Decoder: `DiscoveryResponse::decode`, which needs 99 bytes (through offset 98) and checks only the type byte.

| Offset | Size | Meaning | Sources |
|---|---|---|---|
| 12 | 1 | `0x08` type | HK, FPP, CLRS, DISS |
| 13 | 1 | model, `0x05` = 5A | FPP (`data[0]`), HK dump (`08 05`), CLRS (response EtherType `0x0805`) |
| 15 | 1 | firmware major | FPP (`data[2]`), HK (`Data[1]`), CLRS, DISS |
| 16 | 1 | firmware minor | FPP (`data[3]`), HK (`Data[2]`), CLRS, DISS |
| 34-35 | u16 BE | configured width | FPP (`data[21..22]`), HK (`Data[20..21]`), CLRS |
| 36-37 | u16 BE | configured height | FPP (`data[23..24]`), HK (`Data[22..23]`), CLRS |
| 51-54 | u32 BE | received packet counter (meaning is a guess) | FPP (`data[38..41]`), DISS, HK (`Data[39,40]`: only the low 16 bits, "at least 16 bit") |
| 59-62 | u32 BE | uptime in ms | FPP (`data[46..49]`), DISS, HK (`Data[45..48]`) |
| 98 | 1 | receiver index | FPP (`data[85]`), DISS (`data_offset + 85`), GB ("offset 0x62 in packet type 0x0805") |

Fixture: HK's capture (firmware 10.16, 128x64) decodes to model 5, firmware 10.16, 128x64, receiver 0, 3240 packets, uptime 525728 ms. The model, version, size and receiver-index offsets all land on the expected bytes (`discovery_response_decodes_hkubota_capture`; only the first 192 of 1070 bytes are printed in the post). A second test builds a frame from the offsets above with distinct values in every field.

**Receiver index:** HK's text says "Data[62]: Controller number", which would be offset 76, and that byte is `0xba` in HK's own dump for controller 0. GB's comment gives "0x62" and the dump has `00` there. HK evidently copied the hex number as decimal. FPP and DISS independently give offset 98 (= 0x62). We use 98.

**Model byte:** HK labels `Data[0]` "Receiver card version (5A)" and CLRS says `Data[0] = 0x5A`, but in HK's own dump the byte after `08` is `05`, and `04` follows at offset 14. FPP says "0x05 - 5A card". We expose offset 13 raw and name `0x05` as `MODEL_5A`. Offset 14 (`0x04` in the capture) is unknown and not decoded.

**Firmware rule:** `needs_double_sync(major)` is `major >= 13`. Source: FPP only (`m_highestFirmwareVersion >= 13`; issue #1849, commit `d49ad41`). FPP takes the highest major version seen on the interface.

## 0x0A Brightness (EtherType `0x0AXX`)

77 bytes. Encoder: `BrightnessFrame::encode`.

| Offset | Value | Meaning | Sources |
|---|---|---|---|
| 12 | `0x0A` | type | HK, FPP, PP, DISS |
| 13, 14, 15 | 0-255 each | brightness, R, G, B (see below) | FPP (`data[0..2]`, all equal), PP (`packet[1..3]`), HK ("XXXXff00..": the three levels are the EtherType's low byte and the next two) |
| 16 | `0xff` | unknown, constant | FPP (`data[3]`), PP (`packet[4]`), HK (the `ff` after the XXs) |
| other | `0x00` | | HK, FPP, PP |
| length | 77 | | FPP `CL_BRIG_PACKET_SIZE`, PP (12 + 65), HK (14 + 63) |

**Channel order.** PP names the three bytes red, green, blue. DISS labels them B, G, R. FPP's own code sends the same value in all three, so it doesn't help. No capture shows unequal values. Decision: R, G, B, because (a) PP's order is explicit, and (b) DISS gives the same B, G, R labels to the `0x01` frame's offsets 38-40, and HK's colour temperature captures (2000 K = `ff 76 06`, 8000 K = `ce d8 ff`) show those bytes are R, G, B (a warm white must be red-high). The dissector's labels are wrong there, so it isn't good evidence here. **This is still single-source (PP) for unequal values**; `BrightnessFrame::uniform` is the confirmed case and what FPP sends.

HK: LEDVision's percentage to byte mapping is not linear (1% = 0x28, 2% = 0x35, 5% = 0x4d, 25% = 0x92, 50% = 0xc1, 75% = 0xe3, 100% = 0xff), unlike the linear `0x01` brightness. FPP uses `percent * 2.55`. The encoder takes raw bytes.

FPP notes brightness "may only work on newer firmware. Doesn't seem to work on v2.x and v3.x". The `0x01` frame also carries brightness.

Tests: `brightness_frame_*`. No hex dump of this packet exists; the byte-exact test is built from HK's layout and checked against FPP's and PP's.

## 0x55 Row data (EtherType `0x55RR`)

One packet per run of at most 497 pixels of one row. Encoders: `encode_row_header`, `row_packets` (splitter), `RowPacket::write_to`.

| Offset | Value | Meaning | Sources |
|---|---|---|---|
| 12 | `0x55` | type | HK, FPP, PP, CLRS, DISS |
| 13-14 | u16 BE | row number | FPP (`data[0..1]`), PP (`packet[1..2]`), DISS; HK and CLRS: "EtherType `0x5500` + row MSB; `Data[0]` = row LSB", same bytes |
| 15-16 | u16 BE | pixel offset of this packet's first pixel | FPP (`data[2..3]`), PP, HK (`Data[1..2]`), CLRS, DISS |
| 17-18 | u16 BE | pixel count in this packet | FPP (`data[4..5]`), PP, HK (`Data[3..4]`), CLRS, DISS |
| 19 | `0x08` | unknown, constant | FPP, PP, HK, CLRS |
| 20 | `0x88` | unknown, constant | FPP, PP, CLRS code, HK ("I got 0x88") |
| 21... | 3 bytes per pixel | pixel data | all |
| length | 21 + 3n | | FPP, PP, HK ("row length * 3 + 7" after the EtherType) |
| max n | 497 | pixels per packet (1512 bytes, 1498 payload) | FPP `CL_MAX_PIXL_PER_PACKET`, PP `MAX_PIXELS` |

colorlight.rs differs from the others only in how it uses the fields: it hardcodes pixel offset 0 and puts the row MSB in the EtherType (0x5500 below row 256, 0x5501 from 256), so it can't split rows or address rows above 511. HK says `Data[6]` is `0x80` ("I got 0x88"), and colorlight.rs's doc comment repeats "0x80 or 0x88", but its code sends 0x88 and so does everything else. We send 0x88.

`row_packets(row, card_offset, pixels)` adds `card_offset` to each packet's pixel offset, and fails if `card_offset + width > 65536` so no offset can wrap. The vertical offset is the caller's: pass the card's row number as `row`.

### Pixel byte order

**The protocol does not fix it; it depends on the panel.** The card passes the three bytes through to the panel's colour lines, and LED panels are wired differently.

- HK: "Pixel format: BGR (for my panel)", "RGB order pixel data (resp. BGR for me)".
- CLRS: documents BGR (`row_data_bgr`), copies bytes unchanged.
- FPP: its header comment says "RGB order pixel data", the default order is RGB, and each panel has its own `colorOrder` setting that is applied while building the frame.
- PP: copies the caller's bytes unchanged, no order stated.

So BGR is correct for HK's panel and an assumption in CLRS; RGB is FPP's default. DESIGN.md's "BGR" was wrong as a universal statement and is corrected. `colorlight-proto` treats pixel bytes as opaque: it never reorders them. The sink needs a per-output colour order setting, and the right value for our two 172x86 panels has to be found on hardware (open question in DESIGN.md section 15).

## Frame order for the sink

Guidance from FPP (`PrepData` and `SendData`), not enforced by this crate:

1. `0x0A` brightness (twice on firmware >= 13).
2. `0x55` row packets, all rows, in order. LEDVision doubles each row packet on V13; FPP found that unnecessary and doesn't.
3. A gap: wait until the rows have left the socket, then FPP's default **500 µs** (a setting, clamped to 0-20000 µs). The card latches on the `0x01` frame, and one that arrives right behind the last row can latch before that row is stored, leaving it partly lit.
4. `0x01` display frame (twice, back to back, on firmware >= 13).

FPP skips the `0x01` if the rows didn't all go out, so the card keeps showing the last complete frame.

## Disagreements between sources, and what we chose

| Topic | Disagreement | Choice |
|---|---|---|
| Display frame EtherType | PP README says `0x0101`; PP code and all others `0x0107` | `0x0107` |
| Discovery "ack" | colorlight.rs/HK: separate `0x0700` ack with `Data[2] = 1`; FPP: receiver index at the same byte | one encoder, index parameter |
| Receiver index offset | HK text "Data[62]" (offset 76, which is `0xba` in HK's own dump); GB "0x62"; FPP/DISS 85 + 13 | 98 |
| Model byte | HK/CLRS "Data[0] = 5A"/`0x5A`; HK's dump has `05`; FPP "0x05" | offset 13, `0x05` |
| `0x0A` and `0x01` channel labels | DISS: B, G, R; PP, CLRS, HK captures: R, G, B | R, G, B (see above) |
| Row packet constant | HK/CLRS comment `0x80`; all code and HK's capture `0x88` | `0x88` |
| Pixel order | HK/CLRS BGR; FPP header RGB | not fixed; the caller's choice |

## Still uncertain

- The meaning of the constants: `0x08 0x88` in `0x55`, `0xff` at offset 16 in `0x0A`, `0x05` at offset 36 in `0x01`. Sent as every reference sends them.
- `0x0A` unequal channel values and their order: single source (PP), not seen on hardware.
- `0x08` width/height: FPP warns the "cabinet width/height" fields "don't seem totally correct" when a card is offset. Only HK's 128x64 capture, where it matches, supports them. Treat them as a sanity check, not a source of truth.
- `0x08` packet counter: its meaning is a guess in all references.
- `0x08` offset 14 (`0x04` in the capture) and everything not listed above (e.g. bytes 0x6c-0x6e, `0f 04 08` in the capture) are undecoded.
- The firmware >= 13 rule (double `0x01` and `0x0A`) is FPP-only and not checked on hardware.
- Behaviour of firmware between 11 and 13, and on 5A-75E cards, is not documented in any source.
- `0x02` (receiver layout), `0x10` to `0x76` (config writes) and `0x11` appear in FPP's header comment and the dissector but are not implemented; the daemon only sends pixels (the card is configured with LEDVision).

Why split the protocol out: it's testable without hardware or root, and reusable. `colorlight.rs` (crates.io `colorlight` 0.1.0) is a reference, not a dependency. Vendor ideas, own the code.
