# Colorlight 5A-75B protocol

Confirmed packet layouts, with sources. Only what is listed here may be encoded in `colorlight-proto`. Every entry needs at least two agreeing references and a byte-exact test.

## Sources

| Key | Reference | Licence |
|---|---|---|
| HK | hkubota, "Winter project: Colorlight 5A-75B protocol", 2022-01-31 (includes LEDVision captures): https://hkubota.wordpress.com/2022/01/31/winter-project-colorlight-5a-75b-protocol/ | blog post |
| FPP | FalconChristmas/fpp `src/channeloutput/ColorLight-5a-75.{cpp,h}` (master, read 2026-11-12), V13 fix in commit `d49ad41`, issue https://github.com/FalconChristmas/fpp/issues/1849 | GPL-2.0: facts only, no code copied |
| PP | ZoidTechnology/PanelPlayer and the ivancmz fork, `source/colorlight.c`: https://github.com/ivancmz/PanelPlayer | see repo |
| CLRS | asamonik/colorlight.rs `src/lib.rs`: https://github.com/asamonik/colorlight.rs | see repo |

## Offset conventions

The references count offsets from different points. This document and the code use **absolute** offsets from the start of the Ethernet frame.

| Reference | Index base | Absolute = |
|---|---|---|
| HK "Data[n]", CLRS `frame[14 + n]` | after the EtherType | n + 14 |
| FPP `data[n]` | second EtherType byte (`CL_PACKET_DATA_OFFSET` = 13) | n + 13 |
| PP `packet[n]` | first EtherType byte (packet type) | n + 12 |

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

**Channel order:** R, G, B. HK's 2000 K capture is `ff 76 06` (warm white = red high, blue low), and PP and CLRS name them `r, g, b`. A third-hand summary of FPP's Wireshark dissector labelled them B, G, R. That isn't verified, and it disagrees with three sources.

**Firmware ≥ 13:** send the display frame twice, back to back, and once on older firmware (FPP, issue #1849). This is the sink's job, not the encoder's. See `DESIGN.md` §4.2.

Test: `display_frame_matches_ledvision_capture` (HK hex dump at 75% brightness, byte-for-byte).

## Not yet confirmed

`0x07`/`0x08` discovery, `0x0A` brightness and `0x55` row data. They're described in `DESIGN.md` §4.2 from FPP; add each here with sources and tests before encoding it.
