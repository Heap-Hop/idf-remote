//! Application protocol sessions, independent of HTTP and native serial drivers.
//! The caller owns the byte stream and must keep polling it between requests.
use crate::{
    backend::SerialIo,
    mux::{self, Decoded, Frame},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}
impl Command {
    pub fn payload(&self) -> Result<Vec<u8>> {
        ensure!(
            !self.method.is_empty() && self.method.len() <= 64,
            "method must be 1..64 bytes"
        );
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= mux::MAX_PAYLOAD,
            "application request exceeds 1024 bytes"
        );
        Ok(bytes)
    }
}

#[derive(Debug)]
pub enum Output {
    Console(Vec<u8>),
    Event(Value),
    CorruptFrame,
}

pub struct Session {
    nonce: u64,
    next_request: u32,
    decoder: mux::Decoder,
    pub info: Value,
    valid: bool,
}

fn nonce() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let _ = NEXT.compare_exchange(0, seed.max(1), Ordering::Relaxed, Ordering::Relaxed);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn write_frame(io: &mut dyn SerialIo, frame: &Frame, deadline: Instant) -> Result<()> {
    let bytes = frame.encode()?;
    let mut offset = 0;
    while offset < bytes.len() {
        ensure!(
            Instant::now() < deadline,
            "application write timed out; outcome unknown"
        );
        match io.write(&bytes[offset..]) {
            Ok(0) => bail!("application stream closed while writing"),
            Ok(n) => offset += n,
            Err(e) if transient(&e) => std::thread::sleep(Duration::from_millis(1)),
            Err(e) => return Err(e.into()),
        }
    }
    // No tcdrain/flush: it can block indefinitely on a disappearing device.
    Ok(())
}
fn transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

impl Session {
    /// Explicit opt-in: this sends binary protocol bytes to compatible firmware.
    /// The transport must have a short bounded read/write timeout.
    pub fn connect(
        io: &mut dyn SerialIo,
        timeout: Duration,
        emit: &mut impl FnMut(Output),
    ) -> Result<Self> {
        let mut session = Self {
            nonce: nonce(),
            next_request: 1,
            decoder: mux::Decoder::default(),
            info: Value::Null,
            valid: false,
        };
        let deadline = Instant::now() + timeout;
        write_frame(
            io,
            &Frame {
                kind: mux::HELLO,
                session: session.nonce,
                request: 0,
                payload: vec![],
            },
            deadline,
        )?;
        session.info = session.wait(io, mux::HELLO_ACK, 0, deadline, emit)?;
        ensure!(
            session.info.get("protocol").and_then(Value::as_u64) == Some(1),
            "unsupported application protocol"
        );
        session.valid = true;
        Ok(session)
    }

    pub fn is_valid(&self) -> bool {
        self.valid
    }

    pub fn request(
        &mut self,
        io: &mut dyn SerialIo,
        command: &Command,
        timeout: Duration,
        emit: &mut impl FnMut(Output),
    ) -> Result<Value> {
        ensure!(self.valid, "application session lost; connect again");
        let payload = command.payload()?;
        let id = self.next_request;
        self.next_request = self
            .next_request
            .checked_add(1)
            .context("request ID exhausted; connect again")?;
        let deadline = Instant::now() + timeout;
        if let Err(error) = write_frame(
            io,
            &Frame {
                kind: mux::REQUEST,
                session: self.nonce,
                request: id,
                payload,
            },
            deadline,
        ) {
            self.valid = false;
            return Err(error);
        }
        self.wait(io, mux::RESPONSE, id, deadline, emit)
    }

    pub fn console_input(
        &mut self,
        io: &mut dyn SerialIo,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<()> {
        ensure!(self.valid, "application session lost; connect again");
        let deadline = Instant::now() + timeout;
        for chunk in bytes.chunks(mux::MAX_PAYLOAD) {
            if let Err(e) = write_frame(
                io,
                &Frame {
                    kind: mux::CONSOLE,
                    session: self.nonce,
                    request: 0,
                    payload: chunk.to_vec(),
                },
                deadline,
            ) {
                self.valid = false;
                return Err(e);
            }
        }
        Ok(())
    }

    pub fn feed(&mut self, bytes: &[u8], emit: &mut impl FnMut(Output)) -> Result<()> {
        self.process(bytes, None, emit).map(|_| ())
    }

    fn process(
        &mut self,
        bytes: &[u8],
        expected: Option<(u8, u32)>,
        emit: &mut impl FnMut(Output),
    ) -> Result<Option<Result<Value>>> {
        let mut reply = None;
        let mut reset = false;
        for decoded in self.decoder.feed(bytes) {
            match decoded {
                Decoded::Raw(data) => {
                    reset |= self.valid;
                    emit(Output::Console(data));
                }
                Decoded::Corrupt => emit(Output::CorruptFrame),
                Decoded::Frame(frame) if frame.session == self.nonce => {
                    if expected.is_some_and(|(kind, id)| {
                        frame.request == id && (frame.kind == kind || frame.kind == mux::ERROR)
                    }) {
                        let value: Value = serde_json::from_slice(&frame.payload)
                            .context("invalid application response JSON")?;
                        reply = Some(if frame.kind == mux::ERROR {
                            Err(anyhow::anyhow!("application error: {value}"))
                        } else {
                            Ok(value)
                        });
                    } else if frame.kind == mux::CONSOLE {
                        emit(Output::Console(frame.payload));
                    } else if frame.kind == mux::EVENT {
                        if let Ok(value) = serde_json::from_slice(&frame.payload) {
                            emit(Output::Event(value));
                        } else {
                            emit(Output::CorruptFrame);
                        }
                    }
                }
                Decoded::Frame(_) => {} // old session/late replies never match a new request
            }
        }
        if reset {
            self.valid = false;
            bail!("application session ended by unframed output; connect again");
        }
        Ok(reply)
    }

    fn wait(
        &mut self,
        io: &mut dyn SerialIo,
        kind: u8,
        request: u32,
        deadline: Instant,
        emit: &mut impl FnMut(Output),
    ) -> Result<Value> {
        let mut bytes = [0; 4096];
        while Instant::now() < deadline {
            match io.read(&mut bytes) {
                Ok(0) => {
                    self.valid = false;
                    bail!("application disconnected; outcome unknown");
                }
                Ok(n) => {
                    if let Some(reply) = self.process(&bytes[..n], Some((kind, request)), emit)? {
                        return reply;
                    }
                }
                Err(e) if transient(&e) => {}
                Err(e) => {
                    self.valid = false;
                    return Err(e).context("application disconnected; outcome unknown");
                }
            }
        }
        bail!("application response timed out; outcome unknown (not retried)")
    }
}

/// Useful to expose a protocol warning without leaking transport framing into logs.
pub fn corruption_event() -> Value {
    json!({"reason": "invalid or oversized mux frame"})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        io::{Read, Write},
    };
    struct Peer {
        decoder: mux::Decoder,
        rx: VecDeque<u8>,
        hold: bool,
        held: Option<Frame>,
        reset: bool,
        calls: usize,
    }
    impl Peer {
        fn new() -> Self {
            Self {
                decoder: mux::Decoder::default(),
                rx: VecDeque::new(),
                hold: false,
                held: None,
                reset: false,
                calls: 0,
            }
        }
        fn send(&mut self, frame: Frame) {
            self.rx.extend(frame.encode().unwrap());
        }
    }
    impl Write for Peer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let n = bytes.len().min(7); // partial writes, fragmented input
            for item in self.decoder.feed(&bytes[..n]) {
                if let Decoded::Frame(frame) = item {
                    if frame.kind == mux::HELLO {
                        self.send(Frame {
                            kind: mux::HELLO_ACK,
                            payload: br#"{"protocol":1,"application":"fake"}"#.to_vec(),
                            ..frame
                        });
                    } else if frame.kind == mux::REQUEST {
                        self.calls += 1;
                        let response = Frame {
                            kind: mux::RESPONSE,
                            payload: serde_json::to_vec(&json!({"call":self.calls})).unwrap(),
                            ..frame.clone()
                        };
                        if self.hold {
                            self.held = Some(response);
                            continue;
                        }
                        if let Some(old) = self.held.take() {
                            self.send(old);
                        }
                        if self.reset {
                            self.rx.extend(b"ESP-ROM:reset\n");
                        }
                        self.send(Frame {
                            kind: mux::CONSOLE,
                            request: 0,
                            payload: b"hello log\n".to_vec(),
                            ..frame.clone()
                        });
                        self.send(Frame {
                            kind: mux::EVENT,
                            request: 0,
                            payload: br#"{"tick":1}"#.to_vec(),
                            ..frame
                        });
                        self.send(response);
                    }
                }
            }
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("must not drain USB")
        }
    }
    impl Read for Peer {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.rx.is_empty() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let n = buf.len().min(self.rx.len()).min(13);
            for byte in &mut buf[..n] {
                *byte = self.rx.pop_front().unwrap();
            }
            Ok(n)
        }
    }
    fn command() -> Command {
        Command {
            method: "echo".into(),
            params: json!({"x":1}),
        }
    }
    #[test]
    fn request_delivers_logs_events_and_never_replays_a_timeout() {
        let mut peer = Peer::new();
        let mut output = Vec::new();
        let mut session = Session::connect(&mut peer, Duration::from_millis(100), &mut |e| {
            output.push(e)
        })
        .unwrap();
        peer.hold = true;
        assert!(
            session
                .request(&mut peer, &command(), Duration::from_millis(2), &mut |_| {})
                .unwrap_err()
                .to_string()
                .contains("outcome unknown")
        );
        assert_eq!(peer.calls, 1);
        peer.hold = false;
        assert_eq!(
            session
                .request(
                    &mut peer,
                    &command(),
                    Duration::from_millis(100),
                    &mut |e| output.push(e)
                )
                .unwrap(),
            json!({"call":2})
        );
        assert!(
            output
                .iter()
                .any(|e| matches!(e,Output::Console(v) if v==b"hello log\n"))
        );
        assert!(
            output
                .iter()
                .any(|e| matches!(e,Output::Event(v) if v==&json!({"tick":1})))
        );
        assert_eq!(peer.calls, 2);
    }
    #[test]
    fn reset_invalidates_old_session_and_requires_explicit_connect() {
        let mut peer = Peer::new();
        let mut session =
            Session::connect(&mut peer, Duration::from_millis(100), &mut |_| {}).unwrap();
        peer.reset = true;
        assert!(
            session
                .request(
                    &mut peer,
                    &command(),
                    Duration::from_millis(100),
                    &mut |_| {}
                )
                .is_err()
        );
        assert!(!session.is_valid());
        assert!(
            session
                .request(
                    &mut peer,
                    &command(),
                    Duration::from_millis(100),
                    &mut |_| {}
                )
                .is_err()
        );
        assert_eq!(peer.calls, 1);
    }
}
