use anyhow::{Context, Result, ensure};
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use idf_remote::{
    backend::{DeviceBackend, EspflashBackend},
    device::DeviceId,
    idf,
    logs::CaptureOptions,
    plan::{FlashPlan, PreparedPlan},
};
use regex::Regex;
use reqwest::blocking::multipart::{Form, Part};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs::OpenOptions,
    future::Future,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

#[derive(Parser)]
#[command(
    version,
    about = "Remote ESP-IDF flash and serial monitoring over HTTP"
)]
struct Cli {
    /// Machine-readable JSON / JSON Lines output.
    #[arg(long, global = true)]
    json: bool,
    /// HTTP server address; hardware commands always use this API.
    #[arg(long, global = true, default_value = "http://127.0.0.1:9876")]
    url: String,
    /// Bearer token file, for either serve or client commands.
    #[arg(long, global = true)]
    token_file: Option<PathBuf>,
    /// Stable retry key for one hardware operation; generated when omitted.
    #[arg(long, global = true)]
    request_key: Option<String>,
    /// Color serial monitor output: auto, always, or never.
    #[arg(long, global = true, value_enum, default_value_t = ColorChoice::Auto)]
    color: ColorChoice,
    /// Native serial path. Repeatable only when restricting `serve`.
    #[arg(long, global = true)]
    port: Vec<String>,
    /// Opaque device ID returned by `idf-remote devices`.
    #[arg(long, global = true, conflicts_with = "port")]
    device: Option<DeviceId>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve dynamically discovered USB devices or restrict to explicit ports.
    Serve {
        #[arg(long, default_value = "127.0.0.1:9876")]
        bind: std::net::SocketAddr,
    },
    /// Explicitly negotiate the optional firmware application protocol.
    AppConnect {
        #[arg(long, default_value_t = 115200)]
        monitor_baud: u32,
        #[arg(long, default_value_t = 2000)]
        timeout_ms: u64,
    },
    /// Call a method on connected application firmware (JSON parameters).
    AppCall {
        method: String,
        #[arg(long, default_value = "null")]
        params: String,
        #[arg(long, default_value_t = 115200)]
        monitor_baud: u32,
        #[arg(long, default_value_t = 2000)]
        timeout_ms: u64,
    },
    /// Query a previously submitted operation.
    Operation { id: String },
    /// List daemon-managed devices without opening or resetting them.
    Devices,
    /// Validate artifacts without hardware; print a normalized FlashPlan.
    #[command(group(ArgGroup::new("source").required(true).args(["plan", "build_dir"])))]
    Plan(Source),
    /// Actively identify a chip through the ROM loader, then reset it.
    Probe(Port),
    /// Write and verify all segments; reset with the same port at monitor baud.
    #[command(group(ArgGroup::new("source").required(true).args(["plan", "build_dir"])))]
    Flash {
        #[command(flatten)]
        port: Port,
        #[command(flatten)]
        source: Source,
        #[arg(long, default_value_t = 460800, value_parser = clap::value_parser!(u32).range(115200..))]
        flash_baud: u32,
        /// Continue receiving serial output after flashing.
        #[arg(long)]
        monitor: bool,
        #[command(flatten)]
        capture: Capture,
    },
    /// Erase the entire flash after an explicit destructive confirmation.
    EraseFlash {
        #[command(flatten)]
        port: Port,
        /// Must be exactly "erase-all-flash".
        #[arg(long = "confirm", value_parser = [idf_remote::wire::ERASE_FLASH_CONFIRMATION])]
        confirmation: String,
        #[arg(long, default_value_t = 460800, value_parser = clap::value_parser!(u32).range(115200..))]
        flash_baud: u32,
    },
    /// Read a bounded flash range and download it without overwriting a file.
    ReadFlash {
        #[command(flatten)]
        port: Port,
        #[arg(long, default_value = "0", value_parser = parse_u32_arg)]
        offset: u32,
        #[arg(long, value_parser = parse_u32_arg)]
        size: u32,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 460800, value_parser = clap::value_parser!(u32).range(115200..))]
        flash_baud: u32,
    },
    /// Write bounded bytes to the serial monitor without resetting the device.
    #[command(group(ArgGroup::new("payload").required(true).args(["text", "file"])))]
    SerialWrite {
        /// UTF-8 text to write exactly as provided by the shell.
        #[arg(long)]
        text: Option<String>,
        /// Binary file to write; limited to 64 KiB.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Append one LF byte to the selected payload.
        #[arg(long)]
        newline: bool,
        #[arg(long, default_value_t = 2_000, value_parser = clap::value_parser!(u64).range(1..=30_000))]
        write_timeout_ms: u64,
        #[command(flatten)]
        capture: Capture,
    },
    /// Reset the selected device, optionally capturing its startup output.
    Reset {
        #[command(flatten)]
        port: Port,
        #[arg(long)]
        monitor: bool,
        #[command(flatten)]
        capture: Capture,
    },
    /// Receive serial output; reset only when explicitly requested.
    Monitor {
        #[command(flatten)]
        port: Port,
        /// Reset the selected device before monitoring.
        #[arg(long)]
        reset: bool,
        /// Do not forward standard input to the device.
        #[arg(long)]
        no_input: bool,
        #[command(flatten)]
        capture: Capture,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Args)]
struct Port {
    /// Skip automatic bootloader reset; enter download mode manually first.
    #[arg(long)]
    no_reset_before: bool,
}

struct DeviceSelector {
    port: Option<String>,
    device: Option<DeviceId>,
}

#[derive(Args)]
#[group(skip)]
struct Source {
    /// JSON FlashPlan; artifact paths are relative to this file.
    #[arg(long)]
    plan: Option<PathBuf>,
    /// ESP-IDF build directory containing flasher_args.json.
    #[arg(long)]
    build_dir: Option<PathBuf>,
    /// Flash only these named images from flasher_args.json (repeatable).
    #[arg(long, requires = "build_dir")]
    image: Vec<String>,
    /// Compatibility alias for --image app.
    #[arg(long, requires = "build_dir", conflicts_with = "image")]
    app_only: bool,
}

#[derive(Args)]
struct Capture {
    #[arg(long, default_value_t = 115200, value_parser = clap::value_parser!(u32).range(1..))]
    monitor_baud: u32,
    /// Stop on a matching log line; implies --monitor for flash/reset.
    #[arg(long)]
    wait: Option<String>,
    /// Capture duration in seconds; wait defaults to 30 seconds.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    timeout: Option<u64>,
    /// Create a new file containing the exact serial bytes (never overwrite).
    #[arg(long)]
    raw_log: Option<PathBuf>,
}

struct CaptureOutput {
    options: CaptureOptions,
    raw: Option<std::fs::File>,
}

struct FlashProgressWriter<W> {
    writer: W,
    interactive: bool,
    line_active: bool,
    last_percent: Option<(u32, usize)>,
}

impl<W: Write> FlashProgressWriter<W> {
    fn new(writer: W, interactive: bool) -> Self {
        Self {
            writer,
            interactive,
            line_active: false,
            last_percent: None,
        }
    }

    fn event(&mut self, event: &idf_remote::wire::Event) -> Result<()> {
        if event.kind != "progress" {
            return Ok(());
        }
        let action = event.data["event"]
            .as_str()
            .context("progress event is missing its event type")?;
        let offset = event.data["offset"]
            .as_u64()
            .context("progress event is missing its offset")? as u32;
        match action {
            "segment_started" => {
                let total = event.data["total"]
                    .as_u64()
                    .context("segment_started is missing total")?
                    as usize;
                self.finish_line()?;
                if self.interactive {
                    write!(
                        self.writer,
                        "Writing at 0x{offset:08x}... (  0%) 0/{total} bytes"
                    )?;
                    self.writer.flush()?;
                    self.line_active = true;
                } else {
                    writeln!(self.writer, "Writing at 0x{offset:08x}... 0/{total} bytes")?;
                }
                self.last_percent = Some((offset, 0));
            }
            "segment_progress" => {
                let written = event.data["written"]
                    .as_u64()
                    .context("segment_progress is missing written")?
                    as usize;
                let total = event.data["total"]
                    .as_u64()
                    .context("segment_progress is missing total")?
                    as usize;
                let percent = written
                    .saturating_mul(100)
                    .checked_div(total)
                    .unwrap_or(100);
                if self.last_percent == Some((offset, percent)) {
                    return Ok(());
                }
                if self.interactive {
                    write!(
                        self.writer,
                        "\rWriting at 0x{offset:08x}... ({percent:3}%) {written}/{total} bytes"
                    )?;
                    self.writer.flush()?;
                    self.line_active = true;
                } else if percent == 100 || percent / 10 > self.last_percent.map_or(0, |v| v.1 / 10)
                {
                    writeln!(
                        self.writer,
                        "Writing at 0x{offset:08x}... ({percent:3}%) {written}/{total} bytes"
                    )?;
                }
                self.last_percent = Some((offset, percent));
            }
            "verifying" => {
                self.finish_line()?;
                writeln!(self.writer, "Verifying at 0x{offset:08x}...")?;
            }
            "segment_finished" => {
                self.finish_line()?;
                writeln!(self.writer, "Verified at 0x{offset:08x}.")?;
                self.last_percent = None;
            }
            other => anyhow::bail!("unsupported progress event: {other}"),
        }
        Ok(())
    }

    fn finish_line(&mut self) -> Result<()> {
        if self.line_active {
            writeln!(self.writer)?;
            self.line_active = false;
        }
        self.writer.flush()?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct CaptureMode {
    json_output: bool,
    color: bool,
    interactive: bool,
    monitor_baud: u32,
}

impl CaptureMode {
    fn after_operation(
        json_output: bool,
        color: bool,
        monitor_requested: bool,
        monitor_baud: u32,
    ) -> Self {
        Self {
            json_output,
            color,
            interactive: monitor_requested,
            monitor_baud,
        }
    }
}

const MONITOR_EXIT_BYTE: u8 = 0x1d;
const MONITOR_ACTIVE_POLL_INTERVAL: Duration = Duration::from_millis(5);

enum MonitorInput {
    Bytes(Vec<u8>),
    Exit,
    Error(String),
}

struct RawModeGuard(bool);

impl RawModeGuard {
    fn enter(enabled: bool) -> Result<Self> {
        if enabled {
            crossterm::terminal::enable_raw_mode().context("enable terminal raw mode")?;
        }
        Ok(Self(enabled))
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.0 {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

fn parse_u32_arg(value: &str) -> std::result::Result<u32, String> {
    let value = value.trim();
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16).map_err(|error| error.to_string())
    } else {
        value.parse::<u32>().map_err(|error| error.to_string())
    }
}

fn load_serial_payload(
    text: Option<String>,
    file: Option<PathBuf>,
    newline: bool,
) -> Result<Vec<u8>> {
    let mut data = if let Some(text) = text {
        text.into_bytes()
    } else {
        let path = file.context("provide --text or --file")?;
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("inspect serial payload {}", path.display()))?;
        ensure!(
            metadata.len() <= idf_remote::wire::MAX_SERIAL_WRITE_BYTES as u64,
            "serial-write payload exceeds 64 KiB"
        );
        std::fs::read(&path).with_context(|| format!("read serial payload {}", path.display()))?
    };
    if newline {
        data.push(b'\n');
    }
    ensure!(!data.is_empty(), "serial-write payload must not be empty");
    ensure!(
        data.len() <= idf_remote::wire::MAX_SERIAL_WRITE_BYTES,
        "serial-write payload exceeds 64 KiB"
    );
    Ok(data)
}

impl Capture {
    fn requested(&self) -> bool {
        self.wait.is_some() || self.timeout.is_some() || self.raw_log.is_some()
    }
    fn prepare(&self) -> Result<CaptureOutput> {
        let wait = self
            .wait
            .as_ref()
            .map(|pattern| Regex::new(pattern).context("invalid wait regex"))
            .transpose()?;
        if let Some(regex) = &wait {
            ensure!(
                !regex.is_match(""),
                "wait regex must not match empty output"
            );
        }
        let timeout = self
            .timeout
            .or_else(|| wait.as_ref().map(|_| 30))
            .map(Duration::from_secs);
        let raw = self
            .raw_log
            .as_ref()
            .map(|path| {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .with_context(|| format!("create raw log {}", path.display()))
            })
            .transpose()?;
        Ok(CaptureOutput {
            options: CaptureOptions { timeout, wait },
            raw,
        })
    }
}

fn load(source: &Source) -> Result<(FlashPlan, PreparedPlan, Vec<idf_remote::wire::FlashWarning>)> {
    let (mut plan, base, warnings) = if let Some(path) = &source.plan {
        (
            FlashPlan::load(path)?,
            path.parent().unwrap_or(Path::new(".")).to_path_buf(),
            Vec::new(),
        )
    } else {
        let base = source
            .build_dir
            .as_ref()
            .context("provide --plan or --build-dir")?;
        let selected_images = if source.app_only {
            vec!["app".to_owned()]
        } else {
            source.image.clone()
        };
        let imported = idf::import(&base.join("flasher_args.json"), &selected_images)?;
        for warning in &imported.warnings {
            let image = warning.image.as_deref().unwrap_or("unnamed image");
            eprintln!(
                "warning: skipped empty ESP-IDF image {image} at {:#x}: {}",
                warning.offset,
                base.join(&warning.file).display()
            );
        }
        let warnings = imported
            .warnings
            .into_iter()
            .map(
                |warning| idf_remote::wire::FlashWarning::EmptyIdfImageSkipped {
                    image: warning.image,
                    offset: warning.offset,
                },
            )
            .collect();
        (imported.plan, base.clone(), warnings)
    };
    let prepared = plan.prepare(&base)?;
    EspflashBackend::default().validate(&prepared)?;
    for segment in &mut plan.segments {
        segment.file = base.join(&segment.file).canonicalize()?;
    }
    Ok((plan, prepared, warnings))
}

fn write_json(writer: &mut impl Write, value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn write_devices(
    writer: &mut impl Write,
    response: &idf_remote::wire::DevicesResponse,
) -> Result<()> {
    if response.devices.is_empty() {
        writeln!(writer, "No devices found.")?;
        return Ok(());
    }
    let mut rows = vec![vec![
        "ID".to_owned(),
        "AVAILABILITY".to_owned(),
        "ACTIVITY".to_owned(),
        "ADDRESS".to_owned(),
        "NAME".to_owned(),
    ]];
    rows.extend(response.devices.iter().map(|device| {
        vec![
            device.id.to_string(),
            availability_label(device.status.availability).to_owned(),
            activity_label(device.status.activity).to_owned(),
            device
                .transport
                .address
                .as_deref()
                .unwrap_or("-")
                .to_owned(),
            device.display_name.clone(),
        ]
    }));
    let widths = (0..rows[0].len())
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    for row in rows {
        for (column, value) in row.iter().enumerate() {
            if column + 1 == row.len() {
                writeln!(writer, "{value}")?;
            } else {
                write!(
                    writer,
                    "{value}{}  ",
                    " ".repeat(widths[column] - value.chars().count())
                )?;
            }
        }
    }
    writer.flush()?;
    Ok(())
}

fn availability_label(value: idf_remote::device::DeviceAvailability) -> &'static str {
    use idf_remote::device::DeviceAvailability::*;
    match value {
        Available => "available",
        PermissionRequired => "permission_required",
        Disconnected => "disconnected",
        IdentityMismatch => "identity_mismatch",
        Ambiguous => "ambiguous",
    }
}

fn activity_label(value: idf_remote::device::DeviceActivity) -> &'static str {
    use idf_remote::device::DeviceActivity::*;
    match value {
        Idle => "idle",
        Busy => "busy",
        Monitoring => "monitoring",
        Reconnecting => "reconnecting",
        Error => "error",
    }
}

struct MonitorWriter<W: Write> {
    writer: W,
    color: bool,
    line_start: bool,
    active_color: bool,
}

impl<W: Write> Drop for MonitorWriter<W> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

impl<W: Write> MonitorWriter<W> {
    fn new(writer: W, color: bool) -> Self {
        Self {
            writer,
            color,
            line_start: true,
            active_color: false,
        }
    }

    fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        if !self.color {
            self.writer.write_all(bytes)?;
            self.writer.flush()?;
            for byte in bytes {
                self.line_start = *byte == b'\n';
            }
            return Ok(());
        }
        let mut rendered = Vec::with_capacity(bytes.len() + 32);
        for byte in bytes {
            if self.line_start {
                if let Some(code) = log_color(*byte) {
                    rendered.extend_from_slice(code);
                    self.active_color = true;
                }
                self.line_start = false;
            }
            if *byte == b'\n' {
                if self.active_color {
                    rendered.extend_from_slice(b"\x1b[0m");
                    self.active_color = false;
                }
                self.line_start = true;
            }
            rendered.push(*byte);
        }
        self.writer.write_all(&rendered)?;
        self.writer.flush()?;
        Ok(())
    }

    fn prepare_status_line(
        &mut self,
        status: &mut impl Write,
        shares_terminal: bool,
    ) -> Result<()> {
        self.finish()?;
        if shares_terminal && !self.line_start {
            // Raw terminal mode disables output newline translation, so an LF
            // alone would keep the next status message at the current column.
            status.write_all(b"\r\n")?;
            self.line_start = true;
        }
        status.flush()?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if self.active_color {
            self.writer.write_all(b"\x1b[0m")?;
            self.active_color = false;
        }
        self.writer.flush()?;
        Ok(())
    }
}

fn log_color(first_byte: u8) -> Option<&'static [u8]> {
    match first_byte {
        b'E' => Some(b"\x1b[0;31m"),
        b'W' => Some(b"\x1b[0;33m"),
        b'I' => Some(b"\x1b[0;32m"),
        b'D' => Some(b"\x1b[0;36m"),
        b'V' => Some(b"\x1b[0;90m"),
        _ => None,
    }
}

fn color_enabled(choice: ColorChoice, json_output: bool) -> bool {
    if json_output {
        return false;
    }
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
    }
}

fn monitor_input(
    running: Arc<AtomicBool>,
    exit_on_ctrl_bracket: bool,
) -> tokio::sync::mpsc::Receiver<MonitorInput> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    std::thread::spawn(move || {
        forward_monitor_input(io::stdin().lock(), &running, exit_on_ctrl_bracket, &tx);
    });
    rx
}

fn forward_monitor_input(
    mut reader: impl Read,
    running: &AtomicBool,
    exit_on_ctrl_bracket: bool,
    tx: &tokio::sync::mpsc::Sender<MonitorInput>,
) {
    let mut buffer = [0u8; 4096];
    while running.load(Ordering::Relaxed) {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => {
                let bytes = &buffer[..size];
                if exit_on_ctrl_bracket
                    && let Some(position) = bytes.iter().position(|byte| *byte == MONITOR_EXIT_BYTE)
                {
                    if position > 0
                        && tx
                            .blocking_send(MonitorInput::Bytes(bytes[..position].to_vec()))
                            .is_err()
                    {
                        break;
                    }
                    let _ = tx.blocking_send(MonitorInput::Exit);
                    break;
                }
                if tx
                    .blocking_send(MonitorInput::Bytes(bytes.to_vec()))
                    .is_err()
                {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = tx.blocking_send(MonitorInput::Error(error.to_string()));
                break;
            }
        }
    }
}

fn generated_request_key(label: &str) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    format!(
        "cli-{label}-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time is after Unix epoch")
            .as_nanos(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

struct Http {
    client: reqwest::blocking::Client,
    url: String,
    token: Option<String>,
    request_key: String,
}
impl Http {
    fn new(url: String, token: Option<String>, request_key: Option<String>) -> Result<Self> {
        ensure!(
            url.starts_with("http://"),
            "this local-development client requires an http:// URL"
        );
        let request_key = request_key.unwrap_or_else(|| generated_request_key("operation"));
        idf_remote::wire::validate_idempotency_key(&request_key)?;
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(30))
                .build()?,
            url: url.trim_end_matches('/').into(),
            token,
            request_key,
        })
    }
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::blocking::RequestBuilder {
        let request = self.client.request(method, format!("{}{path}", self.url));
        if let Some(token) = &self.token {
            request.bearer_auth(token)
        } else {
            request
        }
    }
    fn decode<T: serde::de::DeserializeOwned>(response: reqwest::blocking::Response) -> Result<T> {
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("HTTP {status}: {}", response.text()?);
        }
        Ok(response.json()?)
    }
    fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        Self::decode(
            self.request(reqwest::Method::GET, path)
                .send()
                .context("connect to idf-remote; start idf-remote serve first")?,
        )
    }
    fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl serde::Serialize,
    ) -> Result<T> {
        self.post_with_key(path, body, &self.request_key)
    }
    fn post_with_key<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl serde::Serialize,
        request_key: &str,
    ) -> Result<T> {
        let body = serde_json::to_vec(body)?;
        let mut last_error = None;
        for attempt in 0..2 {
            match self
                .request(reqwest::Method::POST, path)
                .header("idempotency-key", request_key)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone())
                .send()
            {
                Ok(response) => return Self::decode(response),
                Err(error) => {
                    last_error = Some(error);
                    if attempt == 0 {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        }
        Err(anyhow::anyhow!(
            "POST failed after retry; operation may still be running. Retry with --request-key {}: {}",
            request_key,
            last_error.expect("two attempts record an error")
        ))
    }
    fn flash_form(
        manifest: &idf_remote::wire::FlashUploadManifest,
        plan: &FlashPlan,
    ) -> Result<Form> {
        let mut form = Form::new().text("metadata", serde_json::to_string(manifest)?);
        for metadata in &manifest.segments {
            let source = plan
                .segments
                .iter()
                .find(|segment| segment.offset == metadata.offset)
                .with_context(|| {
                    format!("missing client artifact for offset {:#x}", metadata.offset)
                })?;
            let actual_size = std::fs::metadata(&source.file)
                .with_context(|| format!("inspect artifact {}", source.file.display()))?
                .len();
            ensure!(
                actual_size == metadata.size,
                "artifact changed after preflight: {}",
                source.file.display()
            );
            let file = std::fs::File::open(&source.file)
                .with_context(|| format!("open artifact {}", source.file.display()))?;
            let part = Part::reader_with_length(file, metadata.size)
                .file_name(metadata.part.clone())
                .mime_str("application/octet-stream")?;
            form = form.part(metadata.part.clone(), part);
        }
        Ok(form)
    }
    fn post_flash<T: serde::de::DeserializeOwned>(
        &self,
        manifest: &idf_remote::wire::FlashUploadManifest,
        plan: &FlashPlan,
    ) -> Result<T> {
        let mut last_error = None;
        for attempt in 0..2 {
            let form = Self::flash_form(manifest, plan)?;
            match self
                .request(reqwest::Method::POST, "/v1/flash")
                .header("idempotency-key", &self.request_key)
                .timeout(Duration::from_secs(120))
                .multipart(form)
                .send()
            {
                Ok(response) => return Self::decode(response),
                Err(error) => {
                    last_error = Some(error);
                    if attempt == 0 {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        }
        Err(anyhow::anyhow!(
            "flash upload failed after retry; operation may still be running. Retry with --request-key {}: {}",
            self.request_key,
            last_error.expect("two attempts record an error")
        ))
    }
    fn resolve_device(&self, selector: &DeviceSelector) -> Result<DeviceId> {
        let response: idf_remote::wire::DevicesResponse = self.get("/v1/devices")?;
        resolve_device_from_response(&response, selector)
    }
    fn finish(
        &self,
        operation: idf_remote::wire::Operation,
        json_output: bool,
        running: &AtomicBool,
    ) -> Result<idf_remote::wire::Operation> {
        if json_output {
            write_json(
                &mut io::stdout().lock(),
                &json!({"event":"operation_submitted","operation":operation}),
            )?;
        } else {
            writeln!(io::stderr().lock(), "Operation {}", operation.id)?;
        }
        self.wait_for_operation(operation, running)
    }

    fn finish_flash(
        &self,
        operation: idf_remote::wire::Operation,
        json_output: bool,
        running: &AtomicBool,
    ) -> Result<(idf_remote::wire::Operation, idf_remote::wire::Cursor)> {
        if json_output {
            write_json(
                &mut io::stdout().lock(),
                &json!({"event":"operation_submitted","operation":operation}),
            )?;
        } else {
            writeln!(io::stderr().lock(), "Operation {}", operation.id)?;
        }
        let stderr = io::stderr();
        let interactive = stderr.is_terminal();
        let mut progress = FlashProgressWriter::new(stderr.lock(), interactive);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let client = MonitorHttp::new(self)?;
            wait_for_flash_operation(&client, operation, json_output, running, &mut progress).await
        })
    }

    fn wait_for_operation(
        &self,
        mut operation: idf_remote::wire::Operation,
        running: &AtomicBool,
    ) -> Result<idf_remote::wire::Operation> {
        let deadline = std::time::Instant::now() + Duration::from_secs(600);
        while operation.status == "running" {
            ensure!(
                running.load(Ordering::Relaxed),
                "client interrupted; operation {} continues on the server",
                operation.id
            );
            ensure!(
                std::time::Instant::now() < deadline,
                "operation polling timed out; query operation {} to resume",
                operation.id
            );
            std::thread::sleep(Duration::from_millis(100));
            operation = self.get(&format!("/v1/operations/{}", operation.id))?;
        }
        ensure!(
            operation.status == "succeeded",
            "operation {} failed: {}",
            operation.id,
            operation.error.as_deref().unwrap_or("unknown error")
        );
        Ok(operation)
    }

    fn download_artifact(
        &self,
        operation: &idf_remote::wire::Operation,
        output_path: &Path,
    ) -> Result<()> {
        let metadata = operation
            .artifact
            .as_ref()
            .context("operation completed without an artifact")?;
        let mut response = self
            .request(
                reqwest::Method::GET,
                &format!("/v1/operations/{}/artifact", operation.id),
            )
            .send()?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("HTTP {status}: {}", response.text()?);
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output_path)
            .with_context(|| format!("create output {}", output_path.display()))?;
        let transfer = (|| -> Result<()> {
            let mut hasher = Sha256::new();
            let mut size = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let read = response.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                output.write_all(&buffer[..read])?;
                hasher.update(&buffer[..read]);
                size += read as u64;
            }
            output.flush()?;
            ensure!(size == metadata.size, "downloaded artifact size mismatch");
            let digest: String = hasher
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            ensure!(
                digest == metadata.sha256,
                "downloaded artifact SHA256 mismatch"
            );
            Ok(())
        })();
        drop(output);
        if let Err(error) = transfer {
            let _ = std::fs::remove_file(output_path);
            return Err(error);
        }
        Ok(())
    }
}

fn resolve_device_from_response(
    response: &idf_remote::wire::DevicesResponse,
    selector: &DeviceSelector,
) -> Result<DeviceId> {
    if let Some(device_id) = &selector.device {
        ensure!(
            response
                .devices
                .iter()
                .any(|device| &device.id == device_id),
            "server does not expose device {device_id}"
        );
        return Ok(device_id.clone());
    }
    if let Some(port) = &selector.port {
        return device_id_for_port(response, port);
    }
    ensure!(
        response.devices.len() == 1,
        "server exposes {} devices; select one with --device or --port",
        response.devices.len()
    );
    Ok(response.devices[0].id.clone())
}

fn device_id_for_port(
    response: &idf_remote::wire::DevicesResponse,
    port: &str,
) -> Result<DeviceId> {
    let matches: Vec<_> = response
        .devices
        .iter()
        .filter(|device| device.transport.address.as_deref() == Some(port))
        .collect();
    ensure!(
        matches.len() == 1,
        "server does not expose exactly one device at port {port}"
    );
    Ok(matches[0].id.clone())
}

fn run(cli: Cli) -> Result<()> {
    let running = Arc::new(AtomicBool::new(true));
    let signal = running.clone();
    ctrlc::set_handler(move || signal.store(false, Ordering::Relaxed))?;
    let token = cli
        .token_file
        .as_ref()
        .map(|path| -> Result<_> {
            let token = std::fs::read_to_string(path)?.trim().to_owned();
            ensure!(!token.is_empty(), "token file is empty");
            Ok(token)
        })
        .transpose()?;
    if let Command::Serve { bind } = &cli.command {
        ensure!(
            cli.device.is_none(),
            "--device cannot restrict serve; use one or more --port options"
        );
        ensure!(
            bind.ip().is_loopback() || token.as_ref().is_some_and(|token| !token.is_empty()),
            "non-loopback bind requires --token-file"
        );
        let backend = Arc::new(EspflashBackend::default());
        if cli.port.is_empty() {
            return idf_remote::server::serve_dynamic(*bind, backend, token, running);
        }
        let devices = backend.devices_for_addresses(&cli.port)?;
        return idf_remote::server::serve_many(*bind, backend, devices, token, running);
    }
    ensure!(
        cli.port.len() <= 1,
        "client commands accept at most one --port"
    );
    let selector = DeviceSelector {
        port: cli.port.first().cloned(),
        device: cli.device.clone(),
    };
    let hardware_command = matches!(
        &cli.command,
        Command::AppConnect { .. }
            | Command::AppCall { .. }
            | Command::Probe(_)
            | Command::Flash { .. }
            | Command::EraseFlash { .. }
            | Command::ReadFlash { .. }
            | Command::SerialWrite { .. }
            | Command::Reset { .. }
            | Command::Monitor { .. }
    );
    ensure!(
        hardware_command || (selector.port.is_none() && selector.device.is_none()),
        "--port and --device apply only to hardware commands"
    );
    let http = Http::new(cli.url, token, cli.request_key)?;
    let monitor_color = color_enabled(cli.color, cli.json);
    match cli.command {
        Command::Serve { .. } => unreachable!(),
        Command::Operation { id } => {
            let operation: idf_remote::wire::Operation =
                http.get(&format!("/v1/operations/{id}"))?;
            write_json(&mut io::stdout().lock(), &operation)?;
        }
        Command::AppConnect {
            monitor_baud,
            timeout_ms,
        } => {
            let request = idf_remote::wire::ApplicationRequest {
                device_id: http.resolve_device(&selector)?,
                monitor_baud,
                timeout_ms,
                command: None,
            };
            request.validate()?;
            let operation =
                http.finish(http.post("/v1/application", &request)?, cli.json, &running)?;
            write_json(&mut io::stdout().lock(), &operation.result)?;
        }
        Command::AppCall {
            method,
            params,
            monitor_baud,
            timeout_ms,
        } => {
            let request = idf_remote::wire::ApplicationRequest {
                device_id: http.resolve_device(&selector)?,
                monitor_baud,
                timeout_ms,
                command: Some(idf_remote::application::Command {
                    method,
                    params: serde_json::from_str(&params).context("params must be JSON")?,
                }),
            };
            request.validate()?;
            let operation =
                http.finish(http.post("/v1/application", &request)?, cli.json, &running)?;
            write_json(&mut io::stdout().lock(), &operation.result)?;
        }
        Command::Devices => {
            let response: idf_remote::wire::DevicesResponse = http.get("/v1/devices")?;
            if cli.json {
                write_json(&mut io::stdout().lock(), &response)?;
            } else {
                write_devices(&mut io::stdout().lock(), &response)?;
            }
        }
        Command::Plan(source) => {
            let (plan, _, _) = load(&source)?;
            writeln!(
                io::stdout().lock(),
                "{}",
                serde_json::to_string_pretty(&plan)?
            )?;
        }
        Command::Probe(port) => {
            let device_id = http.resolve_device(&selector)?;
            let request = idf_remote::wire::DeviceRequest {
                device_id,
                no_reset_before: port.no_reset_before,
                monitor_baud: 115200,
            };
            let operation = http.finish(http.post("/v1/probe", &request)?, cli.json, &running)?;
            write_json(&mut io::stdout().lock(), &operation)?;
        }
        Command::Flash {
            port,
            source,
            flash_baud,
            monitor,
            capture,
        } => {
            let (plan, prepared, warnings) = load(&source)?;
            let output = capture.prepare()?;
            let device_id = http.resolve_device(&selector)?;
            let upload = idf_remote::wire::FlashUploadManifest::from_plan(
                device_id,
                &prepared,
                flash_baud,
                capture.monitor_baud,
                port.no_reset_before,
            )?
            .with_warnings(warnings);
            drop(prepared);
            let submitted: idf_remote::wire::Operation = http.post_flash(&upload, &plan)?;
            let capture_cursor = submitted.start_cursor.clone();
            let (operation, progress_cursor) = http.finish_flash(submitted, cli.json, &running)?;
            if monitor || capture.requested() {
                run_capture(
                    &http,
                    operation.device_id.clone(),
                    if cli.json {
                        progress_cursor
                    } else {
                        capture_cursor
                    },
                    output,
                    CaptureMode::after_operation(
                        cli.json,
                        monitor_color,
                        monitor,
                        capture.monitor_baud,
                    ),
                    &running,
                )?;
            } else {
                write_json(&mut io::stdout().lock(), &operation)?;
            }
        }
        Command::ReadFlash {
            port,
            offset,
            size,
            output,
            flash_baud,
        } => {
            let mut request = idf_remote::wire::ReadFlashRequest {
                device_id: DeviceId::new("unresolved").expect("fixed device ID is valid"),
                offset,
                size,
                flash_baud,
                monitor_baud: 115200,
                no_reset_before: port.no_reset_before,
            };
            request.validate()?;
            ensure!(
                !output.exists(),
                "output already exists: {}",
                output.display()
            );
            request.device_id = http.resolve_device(&selector)?;
            let operation =
                http.finish(http.post("/v1/read-flash", &request)?, cli.json, &running)?;
            http.download_artifact(&operation, &output)?;
            if cli.json {
                write_json(
                    &mut io::stdout().lock(),
                    &json!({"event":"artifact_saved", "path":output, "operation":operation}),
                )?;
            } else {
                writeln!(io::stderr().lock(), "Saved {}", output.display())?;
            }
        }
        Command::EraseFlash {
            port,
            confirmation,
            flash_baud,
        } => {
            let mut request = idf_remote::wire::EraseFlashRequest {
                device_id: DeviceId::new("unresolved").expect("fixed device ID is valid"),
                confirmation,
                flash_baud,
                no_reset_before: port.no_reset_before,
            };
            request.validate()?;
            request.device_id = http.resolve_device(&selector)?;
            let operation =
                http.finish(http.post("/v1/erase-flash", &request)?, cli.json, &running)?;
            write_json(&mut io::stdout().lock(), &operation)?;
        }
        Command::SerialWrite {
            text,
            file,
            newline,
            write_timeout_ms,
            capture,
        } => {
            let data = load_serial_payload(text, file, newline)?;
            let output = capture.prepare()?;
            let device_id = http.resolve_device(&selector)?;
            let request = idf_remote::wire::SerialWriteRequest::from_bytes(
                device_id,
                &data,
                capture.monitor_baud,
                write_timeout_ms,
            )?;
            let operation =
                http.finish(http.post("/v1/serial-write", &request)?, cli.json, &running)?;
            if capture.requested() {
                run_capture(
                    &http,
                    operation.device_id.clone(),
                    operation.start_cursor,
                    output,
                    CaptureMode {
                        json_output: cli.json,
                        color: monitor_color,
                        interactive: false,
                        monitor_baud: capture.monitor_baud,
                    },
                    &running,
                )?;
            } else {
                write_json(&mut io::stdout().lock(), &operation)?;
            }
        }
        Command::Reset {
            port,
            monitor,
            capture,
        } => {
            let output = capture.prepare()?;
            let device_id = http.resolve_device(&selector)?;
            let request = idf_remote::wire::DeviceRequest {
                device_id,
                monitor_baud: capture.monitor_baud,
                no_reset_before: port.no_reset_before,
            };
            let operation = http.finish(http.post("/v1/reset", &request)?, cli.json, &running)?;
            if monitor || capture.requested() {
                run_capture(
                    &http,
                    operation.device_id.clone(),
                    operation.start_cursor,
                    output,
                    CaptureMode::after_operation(
                        cli.json,
                        monitor_color,
                        monitor,
                        capture.monitor_baud,
                    ),
                    &running,
                )?;
            } else {
                write_json(&mut io::stdout().lock(), &operation)?;
            }
        }
        Command::Monitor {
            port,
            reset,
            no_input,
            capture,
        } => {
            let output = capture.prepare()?;
            let device_id = http.resolve_device(&selector)?;
            let request = idf_remote::wire::DeviceRequest {
                device_id,
                monitor_baud: capture.monitor_baud,
                no_reset_before: port.no_reset_before,
            };
            let operation = http.finish(
                http.post(if reset { "/v1/reset" } else { "/v1/monitor" }, &request)?,
                cli.json,
                &running,
            )?;
            run_capture(
                &http,
                operation.device_id.clone(),
                operation.start_cursor,
                output,
                CaptureMode {
                    json_output: cli.json,
                    color: monitor_color,
                    interactive: !no_input,
                    monitor_baud: capture.monitor_baud,
                },
                &running,
            )?;
        }
    }
    Ok(())
}

fn run_capture(
    http: &Http,
    device_id: DeviceId,
    cursor: idf_remote::wire::Cursor,
    mut output: CaptureOutput,
    mode: CaptureMode,
    running: &Arc<AtomicBool>,
) -> Result<()> {
    let started = std::time::Instant::now();
    let terminal_input = mode.interactive && io::stdin().is_terminal();
    if terminal_input && !mode.json_output {
        writeln!(
            io::stderr().lock(),
            "Interactive monitor: keys are sent immediately; Ctrl-] exits, Ctrl-C is sent to the device."
        )?;
    }
    let _raw_mode = RawModeGuard::enter(terminal_input)?;
    let mut stdout = MonitorWriter::new(io::stdout().lock(), mode.color);
    let input = mode
        .interactive
        .then(|| monitor_input(running.clone(), terminal_input));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let client = MonitorHttp::new(http)?;
        capture_loop(
            &client,
            device_id,
            cursor,
            &mut output,
            mode,
            running,
            input,
            &mut stdout,
            started,
        )
        .await
    })
}

// HTTP reads and writes are independent futures. Neither an idle event wait nor
// a slow operation response may block receiving keys or displaying serial output.
struct MonitorHttp {
    client: reqwest::Client,
    url: String,
    token: Option<String>,
}

impl MonitorHttp {
    fn new(http: &Http) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(30))
                .build()?,
            url: http.url.clone(),
            token: http.token.clone(),
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let request = self.client.request(method, format!("{}{path}", self.url));
        if let Some(token) = &self.token {
            request.bearer_auth(token)
        } else {
            request
        }
    }

    async fn decode<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T> {
        let status = response.status();
        ensure!(
            status.is_success(),
            "HTTP {status}: {}",
            response.text().await?
        );
        Ok(response.json().await?)
    }

    async fn events(
        &self,
        device_id: DeviceId,
        cursor: idf_remote::wire::Cursor,
    ) -> Result<idf_remote::wire::EventBatch> {
        Self::decode(
            self.request(reqwest::Method::GET, "/v1/events")
                .query(&idf_remote::wire::EventQuery::new(device_id, cursor))
                .query(&[("wait_ms", 1000)])
                .send()
                .await?,
        )
        .await
    }

    async fn events_now(
        &self,
        device_id: DeviceId,
        cursor: idf_remote::wire::Cursor,
    ) -> Result<idf_remote::wire::EventBatch> {
        Self::decode(
            self.request(reqwest::Method::GET, "/v1/events")
                .query(&idf_remote::wire::EventQuery::new(device_id, cursor))
                .query(&[("wait_ms", 0)])
                .send()
                .await?,
        )
        .await
    }

    async fn operation(&self, id: &str) -> Result<idf_remote::wire::Operation> {
        Self::decode(
            self.request(reqwest::Method::GET, &format!("/v1/operations/{id}"))
                .send()
                .await?,
        )
        .await
    }

    async fn write(&self, device_id: DeviceId, bytes: Vec<u8>, baud: u32) -> Result<()> {
        let request =
            idf_remote::wire::SerialWriteRequest::from_bytes(device_id, &bytes, baud, 2000)?;
        let key = generated_request_key("monitor-input");
        let mut attempt = 0;
        let response = loop {
            match self
                .request(reqwest::Method::POST, "/v1/serial-write")
                .header("idempotency-key", &key)
                .json(&request)
                .send()
                .await
            {
                Ok(response) => break response,
                Err(_) if attempt == 0 => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => anyhow::bail!("monitor input POST failed; retry key {key}: {error}"),
            }
        };
        let mut operation: idf_remote::wire::Operation = Self::decode(response).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while operation.status == "running" {
            ensure!(
                tokio::time::Instant::now() < deadline,
                "monitor input operation {} timed out",
                operation.id
            );
            tokio::time::sleep(MONITOR_ACTIVE_POLL_INTERVAL).await;
            operation = Self::decode(
                self.request(
                    reqwest::Method::GET,
                    &format!("/v1/operations/{}", operation.id),
                )
                .send()
                .await?,
            )
            .await?;
        }
        ensure!(
            operation.status == "succeeded",
            "operation {} failed: {}",
            operation.id,
            operation.error.as_deref().unwrap_or("unknown error")
        );
        Ok(())
    }
}

async fn write_flash_events<W: Write>(
    batch: idf_remote::wire::EventBatch,
    json_output: bool,
    progress: &mut FlashProgressWriter<W>,
) -> Result<idf_remote::wire::Cursor> {
    for event in batch.events {
        if json_output {
            write_json(&mut io::stdout().lock(), &event)?;
        } else {
            progress.event(&event)?;
        }
    }
    Ok(batch.cursor)
}

async fn wait_for_flash_operation<W: Write>(
    http: &MonitorHttp,
    mut operation: idf_remote::wire::Operation,
    json_output: bool,
    running: &AtomicBool,
    progress: &mut FlashProgressWriter<W>,
) -> Result<(idf_remote::wire::Operation, idf_remote::wire::Cursor)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    let mut cursor = operation.start_cursor.clone();
    let mut events = Box::pin(http.events(operation.device_id.clone(), cursor.clone()));
    let mut poll = tokio::time::interval(Duration::from_millis(100));
    let mut cancel = tokio::time::interval(Duration::from_millis(20));
    while operation.status == "running" {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "operation polling timed out; query operation {} to resume",
            operation.id
        );
        tokio::select! {
            batch = &mut events => {
                cursor = write_flash_events(batch?, json_output, progress).await?;
                events = Box::pin(http.events(operation.device_id.clone(), cursor.clone()));
            }
            _ = poll.tick() => {
                operation = http.operation(&operation.id).await?;
            }
            _ = cancel.tick() => {
                ensure!(
                    running.load(Ordering::Relaxed),
                    "client interrupted; operation {} continues on the server",
                    operation.id
                );
            }
        }
    }
    // The worker updates the operation and emits its final events under the same
    // lock. Drain them without waiting so the last 100%/verify update is visible.
    let batch = http
        .events_now(operation.device_id.clone(), cursor.clone())
        .await?;
    cursor = write_flash_events(batch, json_output, progress).await?;
    progress.finish_line()?;
    ensure!(
        operation.status == "succeeded",
        "operation {} failed: {}",
        operation.id,
        operation.error.as_deref().unwrap_or("unknown error")
    );
    Ok((operation, cursor))
}

#[allow(clippy::too_many_arguments)]
async fn capture_loop<W: Write>(
    http: &MonitorHttp,
    device_id: DeviceId,
    mut cursor: idf_remote::wire::Cursor,
    output: &mut CaptureOutput,
    mode: CaptureMode,
    running: &AtomicBool,
    mut input: Option<tokio::sync::mpsc::Receiver<MonitorInput>>,
    stdout: &mut MonitorWriter<W>,
    started: std::time::Instant,
) -> Result<()> {
    use base64::Engine;
    let mut pending_input = VecDeque::new();
    let mut writing: Option<Pin<Box<dyn Future<Output = Result<()>>>>> = None;
    let mut events = Box::pin(http.events(device_id.clone(), cursor.clone()));
    let mut exit_requested = false;
    let mut check_cancel = tokio::time::interval(Duration::from_millis(20));
    loop {
        ensure!(running.load(Ordering::Relaxed), "capture interrupted");
        if writing.is_none() && !pending_input.is_empty() {
            let size = pending_input
                .len()
                .min(idf_remote::wire::MAX_SERIAL_WRITE_BYTES);
            let bytes = pending_input.drain(..size).collect();
            writing = Some(Box::pin(http.write(
                device_id.clone(),
                bytes,
                mode.monitor_baud,
            )));
        }
        if exit_requested && pending_input.is_empty() && writing.is_none() {
            stdout.finish()?;
            return Ok(());
        }
        if output
            .options
            .timeout
            .is_some_and(|timeout| started.elapsed() >= timeout)
        {
            ensure!(
                output.options.wait.is_none(),
                "timed out waiting for matching serial output"
            );
            stdout.finish()?;
            return Ok(());
        }
        let batch = tokio::select! {
            data = async { input.as_mut().unwrap().recv().await },
                if input.is_some() && !exit_requested
                    && pending_input.len() < idf_remote::wire::MAX_SERIAL_WRITE_BYTES => {
                match data {
                    Some(MonitorInput::Bytes(data)) => pending_input.extend(data),
                    Some(MonitorInput::Exit) => exit_requested = true,
                    Some(MonitorInput::Error(error)) => anyhow::bail!("read monitor input: {error}"),
                    None => input = None,
                }
                continue;
            }
            result = async { writing.as_mut().unwrap().await }, if writing.is_some() => {
                result?;
                writing = None;
                continue;
            }
            result = &mut events => result?,
            _ = check_cancel.tick() => continue,
        };
        let mut matched = None;
        for event in batch.events {
            if event.kind == "raw" {
                let bytes = base64::prelude::BASE64_STANDARD
                    .decode(event.data["base64"].as_str().context("invalid raw event")?)?;
                if let Some(raw) = &mut output.raw {
                    raw.write_all(&bytes)?;
                    raw.flush()?;
                }
                if !mode.json_output {
                    stdout.write_raw(&bytes)?;
                }
            }
            if mode.json_output {
                write_json(&mut stdout.writer, &event)?;
            } else if matches!(
                event.kind.as_str(),
                "disconnected" | "reconnecting" | "reconnected"
            ) {
                let mut stderr = io::stderr().lock();
                stdout.prepare_status_line(
                    &mut stderr,
                    io::stdout().is_terminal() && io::stderr().is_terminal(),
                )?;
                write!(stderr, "[idf-remote] {}: {}", event.kind, event.data)?;
                stderr.write_all(b"\r\n")?;
                stderr.flush()?;
            }
            if event.kind == "log"
                && let Some(pattern) = &output.options.wait
                && let Some(text) = event.data["text"]
                    .as_str()
                    .filter(|text| pattern.is_match(text))
            {
                matched = Some(text.to_owned());
            }
        }
        cursor = batch.cursor;
        if let Some(text) = matched {
            if mode.json_output {
                write_json(
                    &mut stdout.writer,
                    &json!({"status":"matched", "text":text, "cursor":cursor}),
                )?;
            } else {
                writeln!(io::stderr().lock(), "Matched: {text}")?;
            }
            stdout.finish()?;
            return Ok(());
        }
        events = Box::pin(http.events(device_id.clone(), cursor.clone()));
    }
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let json_output = cli.json;
    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            if json_output {
                let _ = write_json(
                    &mut io::stderr().lock(),
                    &json!({"error": format!("{error:#}")}),
                );
            } else {
                let _ = writeln!(io::stderr().lock(), "error: {error:#}");
            }
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use idf_remote::{
        device::{DeviceCapability, DeviceDescriptor, DeviceStatus, TransportDescriptor},
        wire::DevicesResponse,
    };
    use std::collections::BTreeMap;

    fn response(address: Option<&str>) -> DevicesResponse {
        DevicesResponse {
            devices: vec![DeviceDescriptor {
                id: DeviceId::new("dev_test").unwrap(),
                display_name: "Test device".into(),
                transport: TransportDescriptor {
                    kind: "desktop_serial".into(),
                    address: address.map(str::to_owned),
                    metadata: BTreeMap::new(),
                },
                status: DeviceStatus::AVAILABLE_IDLE,
                capabilities: vec![DeviceCapability::Flash],
            }],
            cursors: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn desktop_port_is_resolved_to_opaque_server_id() {
        assert_eq!(
            device_id_for_port(&response(Some("COM7")), "COM7").unwrap(),
            DeviceId::new("dev_test").unwrap()
        );
        assert!(device_id_for_port(&response(None), "COM7").is_err());
        assert!(device_id_for_port(&response(Some("COM8")), "COM7").is_err());

        let automatic = DeviceSelector {
            port: None,
            device: None,
        };
        assert_eq!(
            resolve_device_from_response(&response(Some("COM7")), &automatic).unwrap(),
            DeviceId::new("dev_test").unwrap()
        );
        let empty = DevicesResponse {
            devices: Vec::new(),
            cursors: std::collections::HashMap::new(),
        };
        assert!(resolve_device_from_response(&empty, &automatic).is_err());
        let mut multiple = response(Some("COM7"));
        let mut second = multiple.devices[0].clone();
        second.id = DeviceId::new("dev_second").unwrap();
        second.transport.address = Some("COM8".into());
        multiple.devices.push(second);
        let error = resolve_device_from_response(&multiple, &automatic)
            .unwrap_err()
            .to_string();
        assert!(error.contains("server exposes 2 devices"), "{error}");
    }

    #[test]
    fn devices_default_output_is_a_readable_table() {
        let mut output = Vec::new();
        write_devices(&mut output, &response(Some("COM7"))).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("ID"), "{output}");
        assert!(output.contains("AVAILABILITY"), "{output}");
        assert!(output.contains("dev_test"), "{output}");
        assert!(output.contains("available"), "{output}");
        assert!(output.contains("COM7"), "{output}");
        assert!(!output.contains('{'), "{output}");
    }

    #[test]
    fn monitor_writer_adds_level_colors_without_changing_plain_lines() {
        let mut output = Vec::new();
        {
            let mut writer = MonitorWriter::new(&mut output, true);
            writer
                .write_raw(b"I (1) app: ready\nplain output\nE (2) app: failed")
                .unwrap();
        }
        assert_eq!(
            output,
            b"\x1b[0;32mI (1) app: ready\x1b[0m\nplain output\n\x1b[0;31mE (2) app: failed\x1b[0m"
        );
    }

    #[test]
    fn monitor_lifecycle_messages_start_on_a_fresh_raw_terminal_line() {
        let mut output = MonitorWriter::new(Vec::new(), false);
        output.write_raw(b"device> ").unwrap();
        let mut status = Vec::new();
        output.prepare_status_line(&mut status, true).unwrap();
        assert_eq!(output.writer, b"device> ");
        assert_eq!(status, b"\r\n");

        let mut complete = MonitorWriter::new(Vec::new(), false);
        complete.write_raw(b"ready\r\n").unwrap();
        let mut status = Vec::new();
        complete.prepare_status_line(&mut status, true).unwrap();
        assert_eq!(complete.writer, b"ready\r\n");
        assert!(status.is_empty());
    }

    #[test]
    fn requested_post_operation_monitor_is_interactive() {
        assert!(CaptureMode::after_operation(false, true, true, 115200).interactive);
        assert!(!CaptureMode::after_operation(false, true, false, 115200).interactive);
    }

    #[test]
    fn flash_progress_renders_write_and_verify_phases() {
        let mut output = FlashProgressWriter::new(Vec::new(), false);
        for (seq, data) in [
            json!({"event":"segment_started", "offset":0x20000, "total":100}),
            json!({"event":"segment_progress", "offset":0x20000, "written":50, "total":100}),
            json!({"event":"verifying", "offset":0x20000}),
            json!({"event":"segment_finished", "offset":0x20000}),
        ]
        .into_iter()
        .enumerate()
        {
            output
                .event(&idf_remote::wire::Event {
                    seq: seq as u64,
                    kind: "progress".into(),
                    data,
                })
                .unwrap();
        }
        let output = String::from_utf8(output.writer).unwrap();
        assert!(output.contains("Writing at 0x00020000... 0/100 bytes"));
        assert!(output.contains("( 50%) 50/100 bytes"));
        assert!(output.contains("Verifying at 0x00020000..."));
        assert!(output.contains("Verified at 0x00020000."));
    }

    #[test]
    fn monitor_input_forwards_stdin_bytes_without_rewriting_them() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        forward_monitor_input(
            io::Cursor::new(b"status\n"),
            &AtomicBool::new(true),
            false,
            &tx,
        );
        let MonitorInput::Bytes(bytes) = rx.blocking_recv().unwrap() else {
            panic!("expected monitor bytes");
        };
        assert_eq!(bytes, b"status\n");
    }

    #[tokio::test]
    async fn monitor_reads_output_while_input_post_is_still_pending() {
        use axum::{
            Json, Router,
            routing::{get, post},
        };
        use std::sync::Arc;
        let event_waiting = Arc::new(tokio::sync::Notify::new());
        let posted = Arc::new(tokio::sync::Notify::new());
        let release_post = Arc::new(tokio::sync::Notify::new());
        let app = Router::new()
            .route(
                "/v1/events",
                get({
                    let event_waiting = event_waiting.clone();
                    let posted = posted.clone();
                    move || {
                        let event_waiting = event_waiting.clone();
                        let posted = posted.clone();
                        async move {
                            event_waiting.notify_one();
                            posted.notified().await;
                            Json(json!({"cursor":{"epoch":"test","after":2},
                            "device_status":{"availability":"available","activity":"monitoring"},
                            "events":[
                                {"seq":1,"kind":"raw","data":{"base64":"ZWNobw=="}},
                                {"seq":2,"kind":"log","data":{"text":"echo"}}
                            ]}))
                        }
                    }
                }),
            )
            .route(
                "/v1/serial-write",
                post({
                    let posted = posted.clone();
                    let release_post = release_post.clone();
                    move |Json(request): Json<idf_remote::wire::SerialWriteRequest>| {
                        let posted = posted.clone();
                        let release_post = release_post.clone();
                        async move {
                            assert_eq!(request.decode().unwrap(), b"\t\x03");
                            posted.notify_one();
                            release_post.notified().await;
                            Json(json!({}))
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = MonitorHttp {
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            url,
            token: None,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let feed = async {
            // First hold an empty log request open, then type into that session.
            event_waiting.notified().await;
            tx.send(MonitorInput::Bytes(b"\t\x03".to_vec()))
                .await
                .unwrap();
        };
        let mut output = CaptureOutput {
            options: CaptureOptions {
                wait: Some(Regex::new("echo").unwrap()),
                timeout: None,
            },
            raw: None,
        };
        let mut stdout = MonitorWriter::new(Vec::new(), false);
        let running = AtomicBool::new(true);
        let capture = capture_loop(
            &http,
            DeviceId::new("dev_test").unwrap(),
            idf_remote::wire::Cursor {
                epoch: "test".into(),
                after: 0,
            },
            &mut output,
            CaptureMode {
                json_output: false,
                color: false,
                interactive: true,
                monitor_baud: 115200,
            },
            &running,
            Some(rx),
            &mut stdout,
            std::time::Instant::now(),
        );
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(capture, feed)
        })
        .await;
        server.abort();
        result
            .expect("pending event and POST requests must not block each other")
            .0
            .unwrap();
        assert_eq!(stdout.writer, b"echo");
    }

    #[test]
    fn raw_monitor_uses_ctrl_bracket_as_a_local_exit_key() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        forward_monitor_input(
            io::Cursor::new(b"abc\x7f\x03\t\x1dafter"),
            &AtomicBool::new(true),
            true,
            &tx,
        );
        let MonitorInput::Bytes(bytes) = rx.blocking_recv().unwrap() else {
            panic!("expected bytes before exit");
        };
        assert_eq!(bytes, b"abc\x7f\x03\t");
        assert!(matches!(rx.blocking_recv().unwrap(), MonitorInput::Exit));
    }

    #[test]
    fn cli_accepts_device_ids_and_multiple_serve_ports() {
        let cli = Cli::try_parse_from(["idf-remote", "probe", "--device", "dev_test"]).unwrap();
        assert_eq!(cli.device, Some(DeviceId::new("dev_test").unwrap()));
        let Command::Probe(_) = cli.command else {
            panic!("expected probe command");
        };

        assert!(
            Cli::try_parse_from([
                "idf-remote",
                "probe",
                "--device",
                "dev_test",
                "--port",
                "COM7",
            ])
            .is_err()
        );
        let cli = Cli::try_parse_from(["idf-remote", "serve", "--port", "COM7", "--port", "COM8"])
            .unwrap();
        let Command::Serve { .. } = cli.command else {
            panic!("expected serve command");
        };
        assert_eq!(cli.port, ["COM7", "COM8"]);

        let cli = Cli::try_parse_from(["idf-remote", "serve"]).unwrap();
        let Command::Serve { .. } = cli.command else {
            panic!("expected serve command");
        };
        assert!(cli.port.is_empty());

        assert!(Cli::try_parse_from(["idf-remote", "flash", "--build-dir", "build"]).is_ok());

        let cli = Cli::try_parse_from(["idf-remote", "--port", "COM7", "monitor"]).unwrap();
        assert_eq!(cli.port, ["COM7"]);
        let cli = Cli::try_parse_from(["idf-remote", "monitor", "--port", "COM7"]).unwrap();
        assert_eq!(cli.port, ["COM7"]);

        let cli = Cli::try_parse_from([
            "idf-remote",
            "monitor",
            "--device",
            "dev_test",
            "--no-input",
            "--color",
            "always",
        ])
        .unwrap();
        let Command::Monitor { no_input, .. } = cli.command else {
            panic!("expected monitor command");
        };
        assert!(no_input);
        assert_eq!(cli.device, Some(DeviceId::new("dev_test").unwrap()));
        assert_eq!(cli.color, ColorChoice::Always);
    }
}
