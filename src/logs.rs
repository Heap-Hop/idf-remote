use anyhow::{Result, bail, ensure};
use regex::Regex;
use serde::Serialize;
use std::{
    io::{self, Read},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_LINE_BYTES: usize = 16 * 1024;

#[derive(Debug, Serialize)]
pub struct LogRecord {
    pub seq: u64,
    pub host_time_ms: u128,
    pub text: String,
    pub fragment: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub esp_time_ms: Option<u64>,
}

#[derive(Default)]
pub struct LineDecoder {
    pending: Vec<u8>,
    seq: u64,
}

impl LineDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<LogRecord> {
        let mut records = Vec::new();
        for byte in bytes {
            if *byte == b'\n' {
                records.push(self.finish(false));
            } else {
                self.pending.push(*byte);
                if self.pending.len() == MAX_LINE_BYTES {
                    records.push(self.finish(true));
                }
            }
        }
        records
    }
    pub fn finish_pending(&mut self) -> Option<LogRecord> {
        if self.pending.is_empty() {
            None
        } else {
            Some(self.finish(true))
        }
    }
    pub fn pending_text(&self) -> String {
        clean_text(&self.pending)
    }
    fn finish(&mut self, fragment: bool) -> LogRecord {
        self.seq += 1;
        let text = clean_text(&std::mem::take(&mut self.pending));
        static ESP_LOG: OnceLock<Regex> = OnceLock::new();
        let captures = ESP_LOG
            .get_or_init(|| Regex::new(r"^([EWIDV]) \((\d+)\) ([^:]+): ?(.*)$").unwrap())
            .captures(&text);
        let level = captures.as_ref().map(|c| c[1].to_owned());
        let tag = captures.as_ref().map(|c| c[3].to_owned());
        let esp_time_ms = captures.as_ref().and_then(|c| c[2].parse().ok());
        LogRecord {
            seq: self.seq,
            host_time_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            text,
            fragment,
            level,
            tag,
            esp_time_ms,
        }
    }
}

fn clean_text(bytes: &[u8]) -> String {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    let text = String::from_utf8_lossy(bytes);
    ANSI.get_or_init(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").unwrap())
        .replace_all(text.trim_end_matches('\r'), "")
        .into_owned()
}

pub struct CaptureOptions {
    pub timeout: Option<Duration>,
    pub wait: Option<Regex>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CaptureOutcome {
    Matched { text: String },
    DurationElapsed,
    Interrupted,
}

/// Reader must have a finite timeout. Never flush here: startup bytes may
/// already be buffered by the OS when the serial handle reaches this function.
pub fn capture(
    reader: &mut dyn Read,
    options: &CaptureOptions,
    running: &AtomicBool,
    mut emit: impl FnMut(&[u8], &[LogRecord]) -> Result<()>,
) -> Result<CaptureOutcome> {
    ensure!(
        !options.timeout.is_some_and(|timeout| timeout.is_zero()),
        "capture timeout must be positive"
    );
    let start = Instant::now();
    let mut decoder = LineDecoder::default();
    let mut bytes = [0; 4096];
    let outcome = loop {
        if !running.load(Ordering::Relaxed) {
            break CaptureOutcome::Interrupted;
        }
        if options
            .timeout
            .is_some_and(|timeout| start.elapsed() >= timeout)
        {
            if let Some(pending) = decoder.finish_pending() {
                emit(&[], &[pending])?;
            }
            if options.wait.is_some() {
                bail!("timed out waiting for matching serial output");
            }
            break CaptureOutcome::DurationElapsed;
        }
        match reader.read(&mut bytes) {
            Ok(0) => bail!("serial stream closed before capture completed"),
            Ok(size) => {
                let records = decoder.feed(&bytes[..size]);
                emit(&bytes[..size], &records)?;
                if let Some(regex) = &options.wait {
                    let found = records
                        .iter()
                        .find(|record| regex.is_match(&record.text))
                        .map(|record| record.text.clone());
                    let pending = decoder.pending_text();
                    if let Some(text) = found.or_else(|| {
                        (!pending.is_empty() && regex.is_match(&pending)).then_some(pending)
                    }) {
                        break CaptureOutcome::Matched { text };
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => {
                return Err(anyhow::Error::new(error)
                    .context("read serial stream (device may have disconnected)"));
            }
        }
    };
    if let Some(pending) = decoder.finish_pending() {
        emit(&[], &[pending])?;
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    #[test]
    fn decodes_split_utf8_crlf_and_ansi_logs() {
        let bytes = "\x1b[0;32mI (12) wifi: 接続\x1b[0m\r\n".as_bytes();
        let mut decoder = LineDecoder::default();
        let mut records = Vec::new();
        for byte in bytes {
            records.extend(decoder.feed(&[*byte]));
        }
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].text, "I (12) wifi: 接続");
        assert_eq!(records[0].tag.as_deref(), Some("wifi"));
        assert_eq!(records[0].esp_time_ms, Some(12));
        assert_eq!(records[0].seq, 1);
    }
    #[test]
    fn bounds_unterminated_lines_and_keeps_unstructured_output() {
        let mut decoder = LineDecoder::default();
        let records = decoder.feed(&vec![b'x'; MAX_LINE_BYTES * 2 + 1]);
        assert_eq!(records.len(), 2);
        assert!(
            records
                .iter()
                .all(|r| r.fragment && r.text.len() == MAX_LINE_BYTES)
        );
        assert_eq!(decoder.pending.len(), 1);
        let records = decoder.feed(&[0xff, b'\n']);
        assert_eq!(records[0].text, "x�");
        assert!(records[0].level.is_none());
    }
    struct Chunks(VecDeque<Vec<u8>>);
    impl Read for Chunks {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if let Some(bytes) = self.0.pop_front() {
                buf[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            } else {
                Err(io::ErrorKind::TimedOut.into())
            }
        }
    }
    #[test]
    fn waits_across_reads_without_newline_and_retains_raw_bytes() {
        let mut reader = Chunks(VecDeque::from([
            b"boot\nMQ".to_vec(),
            b"TT connected".to_vec(),
        ]));
        let mut raw = Vec::new();
        let result = capture(
            &mut reader,
            &CaptureOptions {
                timeout: Some(Duration::from_secs(1)),
                wait: Some(Regex::new("MQTT connected").unwrap()),
            },
            &AtomicBool::new(true),
            |bytes, _| {
                raw.extend_from_slice(bytes);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            result,
            CaptureOutcome::Matched {
                text: "MQTT connected".into()
            }
        );
        assert_eq!(raw, b"boot\nMQTT connected");
    }
    #[test]
    fn a_new_capture_cannot_match_old_output() {
        let mut reader = Chunks(VecDeque::new());
        assert!(
            capture(
                &mut reader,
                &CaptureOptions {
                    timeout: Some(Duration::from_millis(1)),
                    wait: Some(Regex::new("connected").unwrap())
                },
                &AtomicBool::new(true),
                |_, _| Ok(())
            )
            .is_err()
        );
    }
    #[test]
    fn cancellation_and_stream_closure_are_distinct() {
        let options = CaptureOptions {
            timeout: None,
            wait: None,
        };
        assert_eq!(
            capture(
                &mut io::empty(),
                &options,
                &AtomicBool::new(false),
                |_, _| Ok(())
            )
            .unwrap(),
            CaptureOutcome::Interrupted
        );
        assert!(
            capture(
                &mut io::empty(),
                &options,
                &AtomicBool::new(true),
                |_, _| Ok(())
            )
            .is_err()
        );
    }
}
