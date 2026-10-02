use super::{BoardInfo, DeviceSession, MonitorControl, Progress, prepare_monitor};
use crate::plan::PreparedPlan;
use anyhow::{Context, Result, bail, ensure};
use espflash::{
    connection::{Connection, ResetAfterOperation, ResetBeforeOperation},
    flasher::{FlashFrequency, FlashMode, FlashSize, Flasher},
    image_format::Segment,
    target::{Chip, ProgressCallbacks},
};
use serialport::{ClearBuffer, UsbPortInfo};
use std::{borrow::Cow, time::Duration};

pub(super) struct Session {
    connection: Option<Connection>,
    protocol_input: bool,
    cache_dir: Option<std::path::PathBuf>,
}

impl Session {
    pub(super) fn new(connection: Connection, cache_dir: Option<std::path::PathBuf>) -> Self {
        Self {
            connection: Some(connection),
            protocol_input: false,
            cache_dir,
        }
    }

    /// Recover the connection on every normal error path. A failure does not
    /// auto-reset a potentially partially programmed board into application code.
    fn with_flasher<T>(
        &mut self,
        chip: Option<Chip>,
        baud: u32,
        work: impl FnOnce(&mut Flasher) -> Result<T>,
    ) -> Result<T> {
        let connection = self
            .connection
            .take()
            .context("device session has no connection")?;
        self.protocol_input = true;
        let mut flasher =
            match Flasher::try_connect(connection, true, true, false, chip, Some(baud)) {
                Ok(flasher) => flasher,
                Err(error) => {
                    let (error, connection) = *error;
                    self.connection = Some(connection);
                    return Err(anyhow::Error::new(error).context("connect to ESP ROM bootloader"));
                }
            };
        let result = work(&mut flasher);
        self.connection = Some(flasher.into_connection());
        result
    }
}

impl DeviceSession for Session {
    fn probe(&mut self) -> Result<BoardInfo> {
        self.with_flasher(None, 115200, board_info)
    }

    fn flash(
        &mut self,
        plan: &PreparedPlan,
        baud: u32,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<BoardInfo> {
        let chip = validate(plan)?;
        ensure!(baud >= 115200, "flash baud must be at least 115200");
        self.with_flasher(Some(chip), baud, |flasher| {
            ensure!(!flasher.secure_download_mode(), "secure download mode is outside phase 1 support");
            let security = flasher.security_info().context("read security state before writing")?;
            ensure!(security.flags & 1 == 0 && security.flash_crypt_cnt.count_ones() % 2 == 0,
                "secure boot / flash encryption provisioning is not supported");
            let info = board_info(flasher)?;
            plan.validate_capacity(info.flash_size_bytes)?;
            if let Some(size) = configured_size(plan)? {
                ensure!(size.size() <= info.flash_size_bytes, "configured flash size exceeds detected device capacity");
            }
            validate_detected_size(plan, chip, info.flash_size_bytes)?;
            let segments: Vec<_> = plan.segments.iter().map(|s| Segment { addr: s.offset, data: Cow::Borrowed(s.data.as_slice()) }).collect();
            flasher.write_bins_to_flash(&segments, &mut Callback { emit: progress, offset: 0, total: 0 })
                .context("flash failed; device was not reset automatically and may contain a partial image")?;
            Ok(info)
        })
    }

    fn read_flash(
        &mut self,
        offset: u32,
        size: u32,
        baud: u32,
        output: &mut dyn std::io::Write,
    ) -> Result<BoardInfo> {
        ensure!(size > 0, "read size must be positive");
        ensure!(baud >= 115200, "read baud must be at least 115200");
        let temporary = match &self.cache_dir {
            Some(dir) => tempfile::NamedTempFile::new_in(dir)?,
            None => tempfile::NamedTempFile::new()?,
        }
        .into_temp_path();
        let path = temporary.to_path_buf();
        let info = self.with_flasher(None, baud, |flasher| {
            let info = board_info(flasher)?;
            ensure!(
                u64::from(offset) + u64::from(size) <= u64::from(info.flash_size_bytes),
                "read range exceeds detected device capacity"
            );
            flasher
                .read_flash(offset, size, 0x1000, 64, path)
                .context("read flash")?;
            Ok(info)
        })?;
        let mut input = std::fs::File::open(&temporary)?;
        std::io::copy(&mut input, output)?;
        Ok(info)
    }

    fn erase_flash(&mut self, baud: u32) -> Result<BoardInfo> {
        ensure!(baud >= 115200, "erase baud must be at least 115200");
        self.with_flasher(None, baud, |flasher| {
            ensure!(
                !flasher.secure_download_mode(),
                "secure download mode is outside phase 2 support"
            );
            let security = flasher
                .security_info()
                .context("read security state before erasing")?;
            ensure!(
                security.flags & 1 == 0 && security.flash_crypt_cnt.count_ones() % 2 == 0,
                "secure boot / flash encryption provisioning is not supported"
            );
            let info = board_info(flasher)?;
            flasher
                .erase_flash()
                .context("erase flash failed; device was not reset automatically")?;
            Ok(info)
        })
    }

    fn into_monitor(
        mut self: Box<Self>,
        baud: u32,
        reset: bool,
    ) -> Result<Box<dyn super::SerialIo>> {
        ensure!(baud > 0, "monitor baud must be positive");
        let connection = self
            .connection
            .take()
            .context("device session has no connection")?;
        let mut control = ConnectionControl(Some(connection));
        prepare_monitor(&mut control, baud, self.protocol_input, reset)?;
        let mut port = control
            .0
            .take()
            .context("missing monitor connection")?
            .into_serial();
        // The same worker reads and writes. Bound an idle read so new keyboard
        // input is serviced promptly; readiness still returns incoming bytes early.
        port.set_timeout(Duration::from_millis(2))?;
        Ok(Box::new(port))
    }
}

struct ConnectionControl(Option<Connection>);
impl ConnectionControl {
    fn with_port(
        &mut self,
        work: impl FnOnce(&mut espflash::connection::Port) -> Result<()>,
    ) -> Result<()> {
        let connection = self.0.take().context("missing connection")?;
        let pid = connection.usb_pid();
        let baud = connection.baud()?;
        let mut serial = connection.into_serial();
        let result = work(&mut serial);
        self.0 = Some(Connection::new(
            serial,
            UsbPortInfo {
                vid: 0,
                pid,
                serial_number: None,
                manufacturer: None,
                product: None,
            },
            ResetAfterOperation::NoResetNoStub,
            ResetBeforeOperation::DefaultReset,
            baud,
        ));
        result
    }
}
impl MonitorControl for ConnectionControl {
    fn set_baud(&mut self, baud: u32) -> Result<()> {
        self.0
            .as_mut()
            .context("missing connection")?
            .set_baud(baud)?;
        Ok(())
    }
    fn clear_protocol_input(&mut self) -> Result<()> {
        self.with_port(|serial| {
            serial.clear(ClearBuffer::Input)?;
            Ok(())
        })
    }
    fn release_boot_pin(&mut self) -> Result<()> {
        // A newly opened USB-UART handle can inherit/assert DTR independently
        // of the preceding ROM transaction. HardReset only pulses RTS for
        // bridges, so explicitly release BOOT before asking it to start the app.
        self.with_port(|serial| {
            serial.write_data_terminal_ready(false)?;
            Ok(())
        })
    }
    fn reset(&mut self) -> Result<()> {
        self.0.as_mut().context("missing connection")?.reset()?;
        Ok(())
    }
}

fn board_info(flasher: &mut Flasher) -> Result<BoardInfo> {
    let info = flasher.device_info()?;
    Ok(BoardInfo {
        chip: info.chip.to_string(),
        flash_size_bytes: info.flash_size.size(),
        revision: info.revision,
        mac_address: info.mac_address,
    })
}

fn configured_size(plan: &PreparedPlan) -> Result<Option<FlashSize>> {
    match plan.flash_settings.size.as_deref() {
        None | Some("keep" | "detect") => Ok(None),
        Some(size) => Ok(Some(size.parse().context("invalid flash size")?)),
    }
}

pub(super) fn validate_detected_size(plan: &PreparedPlan, chip: Chip, detected: u32) -> Result<()> {
    if plan.flash_settings.size.as_deref() == Some("detect") {
        let header_size = format!("{}MB", detected / (1024 * 1024))
            .parse::<FlashSize>()?
            .encode_flash_size()?;
        for segment in &plan.segments {
            if segment.offset == chip.boot_address() {
                ensure!(
                    segment.data[3] >> 4 == header_size,
                    "flash_size=detect requires changing this bootloader header; phase 1 preserves bytes. Rebuild for the detected {detected}-byte flash or explicitly choose keep"
                );
            }
        }
    }
    Ok(())
}

pub(super) fn validate(plan: &PreparedPlan) -> Result<Chip> {
    let chip: Chip = plan.chip.parse().context("unsupported ESP chip")?;
    let mode = match plan.flash_settings.mode.as_deref() {
        None | Some("keep") => None,
        Some(mode) => {
            let mode: FlashMode =
                serde_json::from_value(mode.into()).context("invalid flash mode")?;
            Some(match mode {
                FlashMode::Qio => 0,
                FlashMode::Qout => 1,
                FlashMode::Dio => 2,
                FlashMode::Dout => 3,
                _ => bail!("unsupported flash mode"),
            })
        }
    };
    let frequency = match plan.flash_settings.frequency.as_deref() {
        None | Some("keep") => None,
        Some(frequency) => {
            let mhz = frequency.to_ascii_lowercase();
            let mhz = mhz.trim_end_matches("hz").trim_end_matches('m');
            let frequency: FlashFrequency = serde_json::from_value(format!("{mhz}MHz").into())
                .context("invalid flash frequency")?;
            Some(frequency.encode_flash_frequency(chip)?)
        }
    };
    let size = configured_size(plan)?;
    if let Some(size) = size {
        plan.validate_capacity(size.size())?;
    }
    for segment in &plan.segments {
        if segment.offset != chip.boot_address() {
            continue;
        }
        let data = &segment.data;
        ensure!(
            data.len() >= 24 && data[0] == 0xe9,
            "invalid bootloader header at {:#x}",
            segment.offset
        );
        ensure!(
            u16::from_le_bytes([data[12], data[13]]) == chip.id(),
            "bootloader image chip does not match FlashPlan chip"
        );
        let matches = mode.is_none_or(|mode| mode == data[2])
            && frequency.is_none_or(|frequency| frequency == data[3] & 0x0f)
            && size
                .map(|size| size.encode_flash_size())
                .transpose()?
                .is_none_or(|size| size == data[3] >> 4);
        ensure!(
            matches,
            "bootloader header disagrees with flash_settings; phase 1 preserves image bytes (including their digest). Rebuild with matching settings or supply a plan using keep"
        );
    }
    Ok(chip)
}

struct Callback<'a> {
    emit: &'a mut dyn FnMut(Progress),
    offset: u32,
    total: usize,
}
impl ProgressCallbacks for Callback<'_> {
    fn init(&mut self, addr: u32, total: usize) {
        self.offset = addr;
        self.total = total;
        (self.emit)(Progress::SegmentStarted {
            offset: addr,
            total,
        });
    }
    fn update(&mut self, current: usize) {
        (self.emit)(Progress::SegmentProgress {
            offset: self.offset,
            written: current,
            total: self.total,
        });
    }
    fn verifying(&mut self) {
        (self.emit)(Progress::Verifying {
            offset: self.offset,
        });
    }
    fn finish(&mut self, _: bool) {
        (self.emit)(Progress::SegmentFinished {
            offset: self.offset,
        });
    }
}
