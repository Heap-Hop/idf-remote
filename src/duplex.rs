//! A bounded, single-owner stream scheduler. Caller alternates `tick` and a
//! bounded serial read, feeding received bytes even while a write is incomplete.
use crate::{
    application::{self, Output, Session},
    backend::SerialIo,
    mux,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    collections::VecDeque,
    io,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lane {
    Application,
    Input,
}
pub(crate) struct Completion {
    pub key: String,
    pub lane: Lane,
    pub result: Result<Value>,
}
struct Pending {
    key: String,
    deadline: Instant,
    kind: u8,
    request: u32,
}
struct Transfer {
    key: String,
    lane: Lane,
    deadline: Instant,
    frames: VecDeque<Vec<u8>>,
    offset: usize,
    result: Value,
}
#[derive(Default)]
pub(crate) struct Duplex {
    pub session: Option<Session>,
    pending: Option<Pending>,
    tx: VecDeque<Transfer>,
}
impl Duplex {
    pub fn has_pending(&self) -> bool {
        self.pending.is_some() || !self.tx.is_empty()
    }
    pub fn connecting(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|p| p.kind == mux::HELLO_ACK)
    }
    pub fn start_application(
        &mut self,
        key: String,
        command: Option<&application::Command>,
        timeout: Duration,
    ) -> Result<()> {
        ensure!(
            self.pending.is_none(),
            "application request already pending"
        );
        let (frame, kind) = if let Some(command) = command {
            (
                self.session
                    .as_mut()
                    .context("application not connected; use app-connect first")?
                    .prepare_request(command)?,
                mux::RESPONSE,
            )
        } else {
            ensure!(self.tx.is_empty(), "cannot negotiate during a write");
            let (session, hello) = Session::begin_connect();
            self.session = Some(session);
            (hello, mux::HELLO_ACK)
        };
        let bytes = frame.encode()?;
        let deadline = Instant::now() + timeout;
        self.pending = Some(Pending {
            key: key.clone(),
            deadline,
            kind,
            request: frame.request,
        });
        self.tx.push_back(Transfer {
            key,
            lane: Lane::Application,
            deadline,
            frames: VecDeque::from([bytes]),
            offset: 0,
            result: Value::Null,
        });
        Ok(())
    }
    pub fn start_input(
        &mut self,
        key: String,
        data: &[u8],
        timeout: Duration,
        result: Value,
    ) -> Result<()> {
        ensure!(
            !self.tx.iter().any(|t| t.lane == Lane::Input),
            "serial write already pending"
        );
        let frames = if let Some(session) = &self.session {
            session.prepare_console(data)?
        } else {
            data.chunks(256).map(<[u8]>::to_vec).collect()
        };
        self.tx.push_back(Transfer {
            key,
            lane: Lane::Input,
            deadline: Instant::now() + timeout,
            frames: frames.into(),
            offset: 0,
            result,
        });
        Ok(())
    }
    pub fn tick(&mut self, io: &mut dyn SerialIo) -> Result<Vec<Completion>> {
        // A partial frame cannot be safely skipped. Abort the transport on any
        // transmit timeout; never replay potentially delivered bytes.
        ensure!(
            !self.tx.iter().any(|t| Instant::now() >= t.deadline),
            "serial write timed out; outcome unknown (not retried)"
        );
        let mut done = vec![];
        if let Some(front) = self.tx.front_mut() {
            if let Some(frame) = front.frames.front() {
                let end = (front.offset + 256).min(frame.len());
                match io.write(&frame[front.offset..end]) {
                    Ok(0) => bail!("serial stream closed while writing; outcome unknown"),
                    Ok(n) => front.offset += n,
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::TimedOut
                                | io::ErrorKind::WouldBlock
                                | io::ErrorKind::Interrupted
                        ) =>
                    {
                        return Ok(self.expire());
                    }
                    Err(e) => return Err(e).context("write serial payload; outcome unknown"),
                }
                if front.offset == frame.len() {
                    front.frames.pop_front();
                    front.offset = 0;
                }
            }
            if front.frames.is_empty() {
                let transfer = self.tx.pop_front().unwrap();
                if transfer.lane == Lane::Input {
                    done.push(Completion {
                        key: transfer.key,
                        lane: transfer.lane,
                        result: Ok(transfer.result),
                    });
                }
            } else if front.offset == 0 {
                // Only switch senders between complete frames, never inside one.
                let transfer = self.tx.pop_front().unwrap();
                self.tx.push_back(transfer);
            }
        }
        // No flush/tcdrain. Success means all bytes were accepted by the driver.
        done.extend(self.expire());
        Ok(done)
    }
    fn expire(&mut self) -> Vec<Completion> {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| Instant::now() >= p.deadline)
        {
            let pending = self.pending.take().unwrap();
            if pending.kind == mux::HELLO_ACK {
                self.session = None;
            }
            return vec![Completion {
                key: pending.key,
                lane: Lane::Application,
                result: Err(anyhow::anyhow!(
                    "application response timed out; outcome unknown (not retried)"
                )),
            }];
        }
        vec![]
    }
    pub fn feed(&mut self, bytes: &[u8], emit: &mut impl FnMut(Output)) -> Result<Vec<Completion>> {
        let mut done = vec![];
        let expected = self
            .pending
            .as_ref()
            .filter(|p| Instant::now() < p.deadline)
            .map(|p| (p.kind, p.request));
        if let Some(session) = &mut self.session {
            if let Some(mut result) = session.process(bytes, expected, emit)? {
                let pending = self.pending.take().unwrap();
                if pending.kind == mux::HELLO_ACK {
                    result = result.and_then(|info| session.accept_hello(info));
                    if result.is_err() {
                        self.session = None;
                    }
                }
                done.push(Completion {
                    key: pending.key,
                    lane: Lane::Application,
                    result,
                });
            }
        } else {
            emit(Output::Console(bytes.to_vec()));
        }
        done.extend(self.expire());
        Ok(done)
    }
    pub fn abort(&mut self, reason: &str) -> Vec<Completion> {
        let mut done = vec![];
        if let Some(pending) = self.pending.take() {
            done.push(Completion {
                key: pending.key,
                lane: Lane::Application,
                result: Err(anyhow::anyhow!(reason.to_owned())),
            });
        }
        for transfer in self.tx.drain(..) {
            if transfer.lane == Lane::Input {
                done.push(Completion {
                    key: transfer.key,
                    lane: Lane::Input,
                    result: Err(anyhow::anyhow!(reason.to_owned())),
                });
            }
        }
        self.session = None;
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::{Decoded, Frame};
    use serde_json::json;
    use std::io::{Read, Write};

    // TX cannot advance again until RX is serviced. Short writes also ensure
    // switching senders in the middle of an escaped frame is detected.
    #[derive(Default)]
    struct Peer {
        blocked: bool,
        broken: bool,
        raw: bool,
        accepted: usize,
        decoder: mux::Decoder,
        frames: Vec<Frame>,
        rx: VecDeque<u8>,
    }
    impl Write for Peer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.broken {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            if self.blocked {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.blocked = true;
            let n = bytes.len().min(7);
            self.accepted += n;
            if self.raw {
                self.rx.extend(b"log\n");
            } else {
                for item in self.decoder.feed(&bytes[..n]) {
                    let Decoded::Frame(frame) = item else {
                        panic!("interleaved or invalid frame: {item:?}");
                    };
                    if frame.kind == mux::REQUEST {
                        self.rx.extend(
                            Frame {
                                kind: mux::RESPONSE,
                                payload: b"42".to_vec(),
                                ..frame.clone()
                            }
                            .encode()
                            .unwrap(),
                        );
                    } else {
                        self.rx.extend(frame.encode().unwrap());
                    }
                    self.frames.push(frame);
                }
            }
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("drain/flush can block receive")
        }
    }
    impl Read for Peer {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.blocked = false;
            let n = bytes.len().min(self.rx.len());
            if n == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            for byte in &mut bytes[..n] {
                *byte = self.rx.pop_front().unwrap();
            }
            Ok(n)
        }
    }
    fn connected() -> Duplex {
        let (mut session, _) = Session::begin_connect();
        session.accept_hello(json!({"protocol":1})).unwrap();
        Duplex {
            session: Some(session),
            ..Duplex::default()
        }
    }
    fn command() -> application::Command {
        application::Command {
            method: "status".into(),
            params: Value::Null,
        }
    }
    #[test]
    fn long_input_and_control_progress_without_starving_receive_or_mixing_frames() {
        let mut duplex = connected();
        let mut peer = Peer::default();
        let input = vec![0x7e; 4096]; // escaped payload needs many partial writes
        duplex
            .start_input(
                "input".into(),
                &input,
                Duration::from_secs(3),
                json!({"written":4096}),
            )
            .unwrap();
        assert!(duplex.tick(&mut peer).unwrap().is_empty());
        duplex
            .start_application("call".into(), Some(&command()), Duration::from_secs(3))
            .unwrap();
        assert!(
            duplex
                .start_application("second".into(), Some(&command()), Duration::from_secs(3))
                .is_err()
        );
        assert!(
            duplex
                .start_input("second".into(), b"x", Duration::from_secs(3), Value::Null)
                .is_err()
        );
        let mut console = vec![];
        let mut completions = vec![];
        for _ in 0..10000 {
            completions.extend(duplex.tick(&mut peer).unwrap());
            let mut bytes = [0; 97];
            if let Ok(n) = peer.read(&mut bytes) {
                completions.extend(
                    duplex
                        .feed(&bytes[..n], &mut |output| {
                            if let Output::Console(bytes) = output {
                                console.extend(bytes);
                            }
                        })
                        .unwrap(),
                );
            }
            if !duplex.has_pending() && peer.rx.is_empty() {
                break;
            }
        }
        assert!(!duplex.has_pending());
        assert_eq!(completions.len(), 2);
        assert_eq!(completions[0].key, "call"); // reply handled before long input finishes
        assert_eq!(completions[0].result.as_ref().unwrap(), &json!(42));
        assert_eq!(completions[1].key, "input");
        assert_eq!(peer.frames[0].kind, mux::CONSOLE);
        assert_eq!(peer.frames[1].kind, mux::REQUEST); // scheduled at first frame boundary
        assert_eq!(console, input);
    }
    #[test]
    fn raw_write_backpressure_requires_reads_and_does_not_flush() {
        let mut duplex = Duplex::default();
        let mut peer = Peer {
            raw: true,
            ..Peer::default()
        };
        duplex
            .start_input(
                "input".into(),
                &vec![b'a'; 4096],
                Duration::from_secs(3),
                Value::Null,
            )
            .unwrap();
        let mut received = 0;
        let mut done = vec![];
        for _ in 0..1000 {
            done.extend(duplex.tick(&mut peer).unwrap());
            // A second write without a read encounters WouldBlock, not failure.
            done.extend(duplex.tick(&mut peer).unwrap());
            let mut bytes = [0; 32];
            received += peer.read(&mut bytes).unwrap();
            if !duplex.has_pending() {
                break;
            }
        }
        assert_eq!(peer.accepted, 4096);
        assert!(received > 0);
        assert_eq!(done.len(), 1);
        assert!(done[0].result.is_ok());
    }
    #[test]
    fn fatal_write_keeps_both_operations_available_for_abort_even_after_response_timeout() {
        let mut duplex = connected();
        let mut peer = Peer::default();
        duplex
            .start_application("call".into(), Some(&command()), Duration::from_secs(3))
            .unwrap();
        while !duplex.tx.is_empty() {
            duplex.tick(&mut peer).unwrap();
            peer.blocked = false;
        }
        duplex
            .start_input("input".into(), b"x", Duration::from_secs(3), Value::Null)
            .unwrap();
        duplex.pending.as_mut().unwrap().deadline = Instant::now();
        peer.broken = true;
        assert!(duplex.tick(&mut peer).is_err());
        let done = duplex.abort("disconnected");
        assert_eq!(done.len(), 2);
        assert!(done.iter().all(|c| c.result.is_err()));
        assert!(duplex.abort("again").is_empty());
        assert!(!duplex.has_pending());
        assert!(duplex.session.is_none());
    }
    #[test]
    fn late_reply_times_out_once_without_replaying_or_cancelling_console() {
        let mut duplex = connected();
        let mut peer = Peer::default();
        duplex
            .start_application("call".into(), Some(&command()), Duration::from_secs(3))
            .unwrap();
        while !duplex.tx.is_empty() {
            duplex.tick(&mut peer).unwrap();
            peer.blocked = false;
        }
        duplex
            .start_input(
                "input".into(),
                b"hello",
                Duration::from_secs(3),
                Value::Null,
            )
            .unwrap();
        duplex.pending.as_mut().unwrap().deadline = Instant::now();
        let reply: Vec<_> = peer.rx.drain(..).collect();
        let done = duplex.feed(&reply, &mut |_| {}).unwrap();
        assert_eq!(done.len(), 1);
        assert!(
            done[0]
                .result
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("not retried")
        );
        assert!(duplex.feed(&reply, &mut |_| {}).unwrap().is_empty());
        assert!(duplex.has_pending());
        let remaining = duplex.abort("disconnect");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].key, "input");
        assert_eq!(
            peer.frames
                .iter()
                .filter(|f| f.kind == mux::REQUEST)
                .count(),
            1
        );
    }
}
