//! Experimental, bounded serial framing. No serial, HTTP or runtime dependencies.
use anyhow::{Result, ensure};

pub const MAX_PAYLOAD: usize = 1024;
const MAX_BODY: usize = 14 + MAX_PAYLOAD + 4;
pub const HELLO: u8 = 1;
pub const HELLO_ACK: u8 = 2;
pub const CONSOLE: u8 = 3;
pub const REQUEST: u8 = 4;
pub const RESPONSE: u8 = 5;
pub const EVENT: u8 = 6;
pub const ERROR: u8 = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub session: u64,
    pub request: u32,
    pub payload: Vec<u8>,
}

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

impl Frame {
    pub fn encode(&self) -> Result<Vec<u8>> {
        ensure!(
            self.payload.len() <= MAX_PAYLOAD,
            "mux payload exceeds 1024 bytes"
        );
        ensure!((HELLO..=ERROR).contains(&self.kind), "invalid frame kind");
        ensure!(self.session != 0, "zero session nonce");
        let mut body = vec![1, self.kind];
        body.extend(self.session.to_le_bytes());
        body.extend(self.request.to_le_bytes());
        body.extend(&self.payload);
        body.extend(crc32(&body).to_le_bytes());
        let mut wire = vec![0x7e];
        for b in body {
            if matches!(b, 0x7e | 0x7d) {
                wire.extend([0x7d, b ^ 0x20]);
            } else {
                wire.push(b);
            }
        }
        wire.push(0x7e);
        Ok(wire)
    }

    fn decode(body: &[u8]) -> Option<Self> {
        if body.len() < 18 || body[0] != 1 || !(HELLO..=ERROR).contains(&body[1]) {
            return None;
        }
        let end = body.len() - 4;
        if crc32(&body[..end]) != u32::from_le_bytes(body[end..].try_into().ok()?) {
            return None;
        }
        let session = u64::from_le_bytes(body[2..10].try_into().ok()?);
        (session != 0).then(|| Self {
            kind: body[1],
            session,
            request: u32::from_le_bytes(body[10..14].try_into().unwrap()),
            payload: body[14..end].to_vec(),
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decoded {
    Frame(Frame),
    Raw(Vec<u8>),
    Corrupt,
}

#[derive(Default)]
pub struct Decoder {
    in_frame: bool,
    escaped: bool,
    overflow: bool,
    body: Vec<u8>,
}
impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Decoded> {
        let mut result = Vec::new();
        let mut raw = Vec::new();
        for &b in bytes {
            if b == 0x7e {
                if !raw.is_empty() {
                    result.push(Decoded::Raw(std::mem::take(&mut raw)));
                }
                if self.in_frame {
                    if self.overflow || self.escaped {
                        result.push(Decoded::Corrupt);
                    } else if !self.body.is_empty() {
                        result.push(
                            Frame::decode(&self.body).map_or(Decoded::Corrupt, Decoded::Frame),
                        );
                    }
                    self.body.clear();
                    self.escaped = false;
                    self.overflow = false;
                    self.in_frame = true;
                } else {
                    self.in_frame = true;
                }
            } else if !self.in_frame
                || (self.body.is_empty() && !self.escaped && !self.overflow && b != 1)
            {
                self.in_frame = false;
                raw.push(b);
            } else if !self.overflow {
                if self.escaped {
                    self.body.push(b ^ 0x20);
                    self.escaped = false;
                } else if b == 0x7d {
                    self.escaped = true;
                } else {
                    self.body.push(b);
                }
                if self.body.len() > MAX_BODY {
                    self.body.clear();
                    self.overflow = true;
                }
            }
        }
        if !raw.is_empty() {
            result.push(Decoded::Raw(raw));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Frame {
        Frame {
            kind: REQUEST,
            session: 42,
            request: 7,
            payload: (0..=255).collect(),
        }
    }
    #[test]
    fn crc_reference_and_every_fragment_boundary() {
        assert_eq!(crc32(b"123456789"), 0xcbf43926);
        let wire = sample().encode().unwrap();
        for split in 0..=wire.len() {
            let mut decoder = Decoder::default();
            let mut frames = decoder.feed(&wire[..split]);
            frames.extend(decoder.feed(&wire[split..]));
            assert_eq!(frames, [Decoded::Frame(sample())]);
        }
    }
    #[test]
    fn rejects_corruption_and_recovers_after_overflow() {
        let good = sample().encode().unwrap();
        let mut bad = good.clone();
        bad[4] ^= 1;
        let mut decoder = Decoder::default();
        assert_eq!(decoder.feed(&bad), [Decoded::Corrupt]);
        let mut big = vec![0x7e];
        big.extend(vec![1; MAX_BODY * 3]);
        big.push(0x7e);
        assert_eq!(decoder.feed(&big), [Decoded::Corrupt]);
        assert_eq!(decoder.feed(&good), [Decoded::Frame(sample())]);
    }
    #[test]
    fn separates_boot_text_and_adjacent_frames() {
        let mut bytes = b"boot\n".to_vec();
        bytes.extend(sample().encode().unwrap());
        bytes.extend(sample().encode().unwrap());
        bytes.extend(b"reset\n");
        assert_eq!(
            Decoder::default().feed(&bytes),
            [
                Decoded::Raw(b"boot\n".to_vec()),
                Decoded::Frame(sample()),
                Decoded::Frame(sample()),
                Decoded::Raw(b"reset\n".to_vec())
            ]
        );
    }
}
