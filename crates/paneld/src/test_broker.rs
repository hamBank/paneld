//! A minimal fake MQTT 3.1.1 broker for tests (loopback only, no root, no
//! hardware), shared by the MQTT source and the Home Assistant entities.
//!
//! Spec: MQTT Version 3.1.1, OASIS Standard, 29 October 2014,
//! https://docs.oasis-open.org/mqtt/mqtt/v3.1.1/os/mqtt-v3.1.1-os.html
//! Covered: CONNECT (read, including the will) and CONNACK, SUBSCRIBE (read)
//! and SUBACK, PUBLISH at QoS 0 in both directions, PINGREQ/PINGRESP,
//! DISCONNECT, and a hard connection drop. Nothing else (no QoS 1/2, no
//! UNSUBSCRIBE).

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// §2.2.3 Remaining Length: 7 bits per byte, least significant group
/// first, bit 7 set when another byte follows.
fn varint(mut n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut b = (n % 128) as u8;
        n /= 128;
        if n > 0 {
            b |= 0x80;
        }
        out.push(b);
        if n == 0 {
            return out;
        }
    }
}

/// §2.2 fixed header: one byte (type in the high nibble, flags in the
/// low nibble), then the Remaining Length, then the body.
fn packet(first: u8, body: &[u8]) -> Vec<u8> {
    let mut p = vec![first];
    p.extend(varint(body.len()));
    p.extend_from_slice(body);
    p
}

/// §1.5.3 UTF-8 string: two-byte big-endian length, then the bytes.
fn mqtt_str(s: &str) -> Vec<u8> {
    let mut v = (s.len() as u16).to_be_bytes().to_vec();
    v.extend_from_slice(s.as_bytes());
    v
}

/// §3.3 PUBLISH, type 3: flags are DUP (bit 3), QoS (bits 2-1) and
/// RETAIN (bit 0). Variable header: topic name (§3.3.2), then a packet
/// identifier only when QoS > 0 (§3.3.2.2), so none here. Then the payload.
fn publish_packet(topic: &str, payload: &[u8], retain: bool) -> Vec<u8> {
    let mut body = mqtt_str(topic);
    body.extend_from_slice(payload);
    packet(0x30 | u8::from(retain), &body)
}

/// §3.2 CONNACK, type 2, remaining length 2: acknowledge flags (bit 0 =
/// session present, 0 here) then the return code (0 = accepted).
const CONNACK: [u8; 4] = [0x20, 0x02, 0x00, 0x00];

/// §3.9 SUBACK, type 9: the SUBSCRIBE's packet identifier, then one
/// return code per topic filter (0x00 = success at maximum QoS 0).
fn suback(id: [u8; 2], filters: usize) -> Vec<u8> {
    let mut body = id.to_vec();
    body.extend(std::iter::repeat_n(0x00, filters));
    packet(0x90, &body)
}

/// §3.13 PINGRESP, type 13, no body.
const PINGRESP: [u8; 2] = [0xD0, 0x00];

/// Read one packet: fixed header byte, Remaining Length (§2.2.3), body.
async fn read_packet(r: &mut OwnedReadHalf) -> std::io::Result<(u8, Vec<u8>)> {
    let first = r.read_u8().await?;
    let (mut len, mut mult) = (0usize, 1usize);
    loop {
        let b = r.read_u8().await?;
        len += usize::from(b & 0x7f) * mult;
        if b & 0x80 == 0 {
            break;
        }
        mult *= 128;
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).await?;
    Ok((first, body))
}

fn take_str(b: &[u8]) -> (String, &[u8]) {
    let n = usize::from(u16::from_be_bytes([b[0], b[1]]));
    (
        String::from_utf8(b[2..2 + n].to_vec()).unwrap(),
        &b[2 + n..],
    )
}

/// What the broker saw from the client.
#[derive(Debug)]
pub enum Seen {
    Connect {
        client_id: String,
        username: Option<String>,
        password: Option<String>,
        will: Option<Will>,
    },
    Subscribe(Vec<String>),
    /// A PUBLISH from the client (QoS 0 only).
    Publish {
        topic: String,
        payload: Vec<u8>,
        retain: bool,
    },
}

/// The Will of a CONNECT (§3.1.2.5 to §3.1.2.7).
#[derive(Debug, Clone, PartialEq)]
pub struct Will {
    pub topic: String,
    pub payload: Vec<u8>,
    pub retain: bool,
    pub qos: u8,
}

enum Cmd {
    Publish {
        topic: String,
        payload: Vec<u8>,
        retain: bool,
    },
    Drop,
}

pub struct FakeBroker {
    pub port: u16,
    cmds: mpsc::UnboundedSender<Cmd>,
    seen: mpsc::UnboundedReceiver<Seen>,
}

impl FakeBroker {
    pub async fn start() -> FakeBroker {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (cmds, cmd_rx) = mpsc::unbounded_channel();
        let (seen_tx, seen) = mpsc::unbounded_channel();
        tokio::spawn(serve(listener, cmd_rx, seen_tx));
        FakeBroker { port, cmds, seen }
    }

    /// Send a QoS 0 PUBLISH to the connected client.
    pub fn publish(&self, topic: &str, payload: &str, retain: bool) {
        self.cmds
            .send(Cmd::Publish {
                topic: topic.into(),
                payload: payload.as_bytes().to_vec(),
                retain,
            })
            .unwrap();
    }

    pub fn drop_connection(&self) {
        self.cmds.send(Cmd::Drop).unwrap();
    }

    pub async fn next_seen(&mut self) -> Seen {
        tokio::time::timeout(Duration::from_secs(5), self.seen.recv())
            .await
            .expect("broker saw nothing in 5 s")
            .expect("broker stopped")
    }

    /// Skip to the next SUBSCRIBE and return its topic filters.
    pub async fn next_subscribe(&mut self) -> Vec<String> {
        loop {
            if let Seen::Subscribe(t) = self.next_seen().await {
                return t;
            }
        }
    }
}

/// Accept one connection at a time; serve it until it drops.
async fn serve(
    listener: TcpListener,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    seen: mpsc::UnboundedSender<Seen>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let (rd, mut wr) = stream.into_split();
        // Read on a separate task: reading is not cancel-safe inside select.
        let (pk_tx, mut pk_rx) = mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            let mut rd = rd;
            while let Ok(p) = read_packet(&mut rd).await {
                if pk_tx.send(p).is_err() {
                    break;
                }
            }
        });
        loop {
            tokio::select! {
                pkt = pk_rx.recv() => {
                    let Some((first, body)) = pkt else { break };
                    match first >> 4 {
                        // §3.1 CONNECT. Variable header: protocol name
                        // (string), level (1 byte, 4 = 3.1.1), connect
                        // flags (§3.1.2.3: bit 7 user name, bit 6
                        // password, bit 2 will), keep alive (2 bytes).
                        // Payload (§3.1.3): client id, [will topic, will
                        // message], [user name], [password].
                        1 => {
                            let (proto, rest) = take_str(&body);
                            assert_eq!(proto, "MQTT");
                            assert_eq!(rest[0], 4, "protocol level 4 = MQTT 3.1.1");
                            // Flags: bit 5 will retain (§3.1.2.7), bits 4-3
                            // will QoS (§3.1.2.6), bit 2 will flag (§3.1.2.5).
                            // With the will flag set the payload carries
                            // will topic (string) and will message (a
                            // two-byte length and bytes, §3.1.3.3 / §3.1.3.4)
                            // after the client id.
                            let flags = rest[1];
                            let (client_id, rest) = take_str(&rest[4..]);
                            let (will, rest) = if flags & 0x04 != 0 {
                                let (topic, r) = take_str(rest);
                                let n = usize::from(u16::from_be_bytes([r[0], r[1]]));
                                let payload = r[2..2 + n].to_vec();
                                let will = Will {
                                    topic,
                                    payload,
                                    retain: flags & 0x20 != 0,
                                    qos: (flags >> 3) & 0x03,
                                };
                                (Some(will), &r[2 + n..])
                            } else {
                                (None, rest)
                            };
                            let (username, rest) = if flags & 0x80 != 0 {
                                let (u, r) = take_str(rest);
                                (Some(u), r)
                            } else {
                                (None, rest)
                            };
                            let password = (flags & 0x40 != 0).then(|| take_str(rest).0);
                            let _ = seen.send(Seen::Connect { client_id, username, password, will });
                            let _ = wr.write_all(&CONNACK).await;
                        }
                        // §3.8 SUBSCRIBE: fixed header flags must be
                        // 0b0010; variable header is the packet
                        // identifier; payload is pairs of topic filter
                        // (string) and requested QoS (1 byte).
                        8 => {
                            assert_eq!(first & 0x0f, 0x02);
                            let id = [body[0], body[1]];
                            let mut rest = &body[2..];
                            let mut filters = Vec::new();
                            while !rest.is_empty() {
                                let (f, r) = take_str(rest);
                                assert_eq!(r[0], 0, "QoS 0 requested");
                                filters.push(f);
                                rest = &r[1..];
                            }
                            let n = filters.len();
                            let _ = seen.send(Seen::Subscribe(filters));
                            let _ = wr.write_all(&suback(id, n)).await;
                        }
                        // §3.3 PUBLISH from the client: QoS from flag bits
                        // 2-1 (must be 0 here, so no packet identifier),
                        // RETAIN is bit 0; variable header is the topic.
                        3 => {
                            assert_eq!((first >> 1) & 0x03, 0, "QoS 0 publishes only");
                            let (topic, payload) = take_str(&body);
                            let _ = seen.send(Seen::Publish {
                                topic,
                                payload: payload.to_vec(),
                                retain: first & 1 != 0,
                            });
                        }
                        // §3.12 PINGREQ -> §3.13 PINGRESP.
                        12 => {
                            let _ = wr.write_all(&PINGRESP).await;
                        }
                        // §3.14 DISCONNECT.
                        14 => break,
                        _ => {}
                    }
                }
                cmd = cmds.recv() => match cmd {
                    Some(Cmd::Publish { topic, payload, retain }) => {
                        let _ = wr.write_all(&publish_packet(&topic, &payload, retain)).await;
                    }
                    Some(Cmd::Drop) | None => break,
                },
            }
        }
        reader.abort();
        // `wr` drops here, closing the connection.
    }
}
