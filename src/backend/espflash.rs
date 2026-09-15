use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    sync::Mutex,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use espflash::{
    connection::{Connection, ResetAfterOperation, ResetBeforeOperation},
    flasher::{FlashFrequency, FlashMode, FlashSize, Flasher},
    image_format::Segment,
    target::{Chip, ProgressCallbacks},
};
use serialport::{ClearBuffer, SerialPort, SerialPortType, UsbPortInfo};
use sha2::{Digest, Sha256};

use super::{BoardInfo, DeviceBackend, DeviceSession, MonitorControl, Progress, prepare_monitor};
use crate::{
    device::{
        DeviceAvailability, DeviceCapability, DeviceDescriptor, DeviceId, DeviceStatus,
        TransportDescriptor,
    },
    plan::PreparedPlan,
};

const MAX_DISCOVERED_DEVICES: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
enum DesktopPhysicalIdentity {
    Usb {
        vid: u16,
        pid: u16,
        serial_number: Option<String>,
        manufacturer: Option<String>,
        product: Option<String>,
    },
    AddressBound {
        kind: String,
        address: String,
    },
}

impl DesktopPhysicalIdentity {
    fn from_usb(info: &UsbPortInfo) -> Self {
        Self::Usb {
            vid: info.vid,
            pid: info.pid,
            serial_number: info.serial_number.clone(),
            manufacturer: info.manufacturer.clone(),
            product: info.product.clone(),
        }
    }

    fn is_strong_match(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Usb {
                    vid: left_vid,
                    pid: left_pid,
                    serial_number: Some(left_serial),
                    manufacturer: left_manufacturer,
                    product: left_product,
                },
                Self::Usb {
                    vid: right_vid,
                    pid: right_pid,
                    serial_number: Some(right_serial),
                    manufacturer: right_manufacturer,
                    product: right_product,
                },
            ) if !left_serial.is_empty() && !right_serial.is_empty() => {
                left_vid == right_vid
                    && left_pid == right_pid
                    && left_serial == right_serial
                    && left_manufacturer == right_manufacturer
                    && left_product == right_product
            }
            _ => false,
        }
    }
}

#[derive(Clone)]
struct ScannedDesktopDevice {
    descriptor: DeviceDescriptor,
    port: String,
    usb: UsbPortInfo,
    identity: DesktopPhysicalIdentity,
}

#[derive(Clone)]
struct DesktopDeviceRecord {
    descriptor: DeviceDescriptor,
    port: String,
    usb: UsbPortInfo,
    identity: DesktopPhysicalIdentity,
    locator_generation: u64,
}

#[derive(Default)]
pub struct EspflashBackend {
    records: Mutex<HashMap<DeviceId, DesktopDeviceRecord>>,
}

impl EspflashBackend {
    fn scan_devices() -> Result<Vec<ScannedDesktopDevice>> {
        let mut devices = Vec::new();
        for port in serialport::available_ports()? {
            let address = port.port_name;
            let mut metadata = BTreeMap::new();
            let (identity_key, display_name, usb, identity) = match port.port_type {
                SerialPortType::UsbPort(info) => {
                    metadata.insert("port_type".into(), "usb".into());
                    if reopen_after_boot_entry(&info) {
                        metadata.insert("reopen_after_boot_entry".into(), "true".into());
                    }
                    metadata.insert(
                        "identity_strength".into(),
                        if info
                            .serial_number
                            .as_deref()
                            .is_some_and(|serial| !serial.is_empty())
                        {
                            "strong"
                        } else {
                            "weak"
                        }
                        .into(),
                    );
                    metadata.insert("vid".into(), format!("{:04x}", info.vid));
                    metadata.insert("pid".into(), format!("{:04x}", info.pid));
                    if let Some(serial) = &info.serial_number {
                        metadata.insert("serial_number".into(), serial.clone());
                    }
                    if let Some(product) = &info.product {
                        metadata.insert("product".into(), product.clone());
                    }
                    if let Some(manufacturer) = &info.manufacturer {
                        metadata.insert("manufacturer".into(), manufacturer.clone());
                    }
                    let key = usb_identity_key(
                        info.vid,
                        info.pid,
                        info.serial_number.as_deref(),
                        &address,
                    );
                    let name = info.product.as_deref().unwrap_or("USB serial device");
                    let suffix = info.serial_number.as_deref().unwrap_or(&address);
                    let identity = DesktopPhysicalIdentity::from_usb(&info);
                    (key, format!("{name} ({suffix})"), info, identity)
                }
                SerialPortType::BluetoothPort => {
                    metadata.insert("port_type".into(), "bluetooth".into());
                    (
                        format!("bluetooth:{address}"),
                        format!("Bluetooth serial ({address})"),
                        unknown_usb(),
                        DesktopPhysicalIdentity::AddressBound {
                            kind: "bluetooth".into(),
                            address: address.clone(),
                        },
                    )
                }
                SerialPortType::PciPort => {
                    metadata.insert("port_type".into(), "pci".into());
                    (
                        format!("pci:{address}"),
                        format!("PCI serial ({address})"),
                        unknown_usb(),
                        DesktopPhysicalIdentity::AddressBound {
                            kind: "pci".into(),
                            address: address.clone(),
                        },
                    )
                }
                SerialPortType::Unknown => {
                    metadata.insert("port_type".into(), "unknown".into());
                    (
                        format!("unknown:{address}"),
                        format!("Serial device ({address})"),
                        unknown_usb(),
                        DesktopPhysicalIdentity::AddressBound {
                            kind: "unknown".into(),
                            address: address.clone(),
                        },
                    )
                }
            };
            let id = device_id(&identity_key);
            let descriptor = DeviceDescriptor {
                id: id.clone(),
                display_name,
                transport: TransportDescriptor {
                    kind: "desktop_serial".into(),
                    address: Some(address.clone()),
                    metadata,
                },
                status: DeviceStatus::AVAILABLE_IDLE,
                capabilities: vec![
                    DeviceCapability::Probe,
                    DeviceCapability::Flash,
                    DeviceCapability::ReadFlash,
                    DeviceCapability::EraseFlash,
                    DeviceCapability::SerialWrite,
                    DeviceCapability::Reset,
                    DeviceCapability::Monitor,
                ],
            };
            devices.push(ScannedDesktopDevice {
                descriptor,
                port: address,
                usb,
                identity,
            });
        }
        devices.sort_by(|a, b| a.port.cmp(&b.port));
        Ok(devices)
    }

    pub fn devices_for_addresses(&self, addresses: &[String]) -> Result<Vec<DeviceDescriptor>> {
        ensure!(
            !addresses.is_empty(),
            "at least one allowed serial port is required"
        );
        let mut records = self.records.lock().unwrap();
        let scanned = Self::scan_devices()?;
        let mut configured = Vec::with_capacity(addresses.len());
        let mut pending = Vec::with_capacity(addresses.len());
        let mut locator_groups = HashSet::new();
        for address in addresses {
            ensure!(
                locator_groups.insert(locator_group_key(address)),
                "the same serial interface was configured more than once: {address}"
            );
            let mut matches = scanned.iter().filter(|device| device.port == *address);
            let device = matches
                .next()
                .with_context(|| format!("allowed serial port is not present: {address}"))?;
            ensure!(
                matches.next().is_none(),
                "allowed serial port is ambiguous: {address}"
            );
            let id = device.descriptor.id.clone();
            ensure!(
                !records.contains_key(&id)
                    && !configured
                        .iter()
                        .any(|descriptor: &DeviceDescriptor| descriptor.id == id),
                "device was configured more than once: {id}"
            );
            let mut descriptor = device.descriptor.clone();
            descriptor
                .transport
                .metadata
                .insert("locator_generation".into(), "0".into());
            configured.push(descriptor.clone());
            pending.push((
                id,
                DesktopDeviceRecord {
                    descriptor,
                    port: device.port.clone(),
                    usb: device.usb.clone(),
                    identity: device.identity.clone(),
                    locator_generation: 0,
                },
            ));
        }
        for (id, record) in pending {
            records.insert(id, record);
        }
        Ok(configured)
    }

    pub fn device_for_address(&self, address: &str) -> Result<DeviceDescriptor> {
        self.devices_for_addresses(&[address.to_owned()])
            .map(|mut devices| devices.remove(0))
    }

    fn discover_from_scan(
        records: &mut HashMap<DeviceId, DesktopDeviceRecord>,
        scanned: &[ScannedDesktopDevice],
    ) -> Vec<DeviceDescriptor> {
        for record in records.values_mut() {
            reconcile_record(record, scanned);
        }

        let mut groups = BTreeMap::<String, Vec<&ScannedDesktopDevice>>::new();
        for candidate in scanned
            .iter()
            .filter(|candidate| matches!(candidate.identity, DesktopPhysicalIdentity::Usb { .. }))
        {
            groups
                .entry(locator_group_key(&candidate.port))
                .or_default()
                .push(candidate);
        }

        for candidates in groups.into_values() {
            if records.len() >= MAX_DISCOVERED_DEVICES {
                break;
            }
            let candidate = candidates
                .iter()
                .copied()
                .find(|candidate| {
                    macos_serial_alias(&candidate.port)
                        .is_some_and(|(kind, _)| kind == MacosSerialAddressKind::Callout)
                })
                .or_else(|| candidates.first().copied())
                .expect("a locator group is nonempty");
            let represented = records.values().any(|record| {
                (record.identity == candidate.identity
                    && locator_group_key(&record.port) == locator_group_key(&candidate.port))
                    || record.identity.is_strong_match(&candidate.identity)
            });
            if represented || records.contains_key(&candidate.descriptor.id) {
                continue;
            }
            records.insert(candidate.descriptor.id.clone(), claimed_record(candidate));
        }

        // Reconcile newly inserted records too, so duplicate strong identities are
        // immediately reported as ambiguous rather than depending on a later scan.
        for record in records.values_mut() {
            reconcile_record(record, scanned);
        }
        let mut descriptors = records
            .values()
            .map(|record| record.descriptor.clone())
            .collect::<Vec<_>>();
        descriptors.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        descriptors
    }

    fn refresh_record_with(
        &self,
        device_id: &DeviceId,
        scan: impl FnOnce() -> Result<Vec<ScannedDesktopDevice>>,
    ) -> Result<DesktopDeviceRecord> {
        // Scanning and reconciliation are one ordered registry operation. If two
        // callers overlap, the later reconciliation cannot apply a scan that
        // started before an already-committed locator update.
        let mut records = self.records.lock().unwrap();
        let scanned = scan()?;
        let record = records
            .get_mut(device_id)
            .context("device is not configured in this backend")?;
        reconcile_record(record, &scanned);
        Ok(record.clone())
    }

    fn validated_record(&self, device_id: &DeviceId) -> Result<DesktopDeviceRecord> {
        let record = self.refresh_record_with(device_id, Self::scan_devices)?;
        ensure!(
            record.descriptor.status.availability == DeviceAvailability::Available,
            "device is unavailable: {:?}",
            record.descriptor.status.availability
        );
        Ok(record)
    }
}

fn reopen_after_boot_entry(info: &UsbPortInfo) -> bool {
    info.vid == 0x303a
        && info.pid == 0x1001
        && info
            .product
            .as_deref()
            .is_some_and(|product| product.contains("JTAG/serial"))
}

fn set_availability(record: &mut DesktopDeviceRecord, availability: DeviceAvailability) {
    record.descriptor.status.availability = availability;
}

fn claimed_record(candidate: &ScannedDesktopDevice) -> DesktopDeviceRecord {
    let mut descriptor = candidate.descriptor.clone();
    descriptor
        .transport
        .metadata
        .insert("locator_generation".into(), "0".into());
    DesktopDeviceRecord {
        descriptor,
        port: candidate.port.clone(),
        usb: candidate.usb.clone(),
        identity: candidate.identity.clone(),
        locator_generation: 0,
    }
}

fn adopt_locator(record: &mut DesktopDeviceRecord, candidate: &ScannedDesktopDevice) {
    let activity = record.descriptor.status.activity;
    let id = record.descriptor.id.clone();
    record.descriptor = candidate.descriptor.clone();
    record.descriptor.id = id;
    record.descriptor.status = DeviceStatus {
        availability: DeviceAvailability::Available,
        activity,
    };
    record.port = candidate.port.clone();
    record.usb = candidate.usb.clone();
    record.identity = candidate.identity.clone();
    record.locator_generation += 1;
    record.descriptor.transport.metadata.insert(
        "locator_generation".into(),
        record.locator_generation.to_string(),
    );
}

fn reconcile_record(record: &mut DesktopDeviceRecord, scanned: &[ScannedDesktopDevice]) {
    if let Some(candidate) = scanned
        .iter()
        .find(|candidate| candidate.port == record.port)
    {
        if candidate.identity == record.identity {
            let strong_groups = scanned
                .iter()
                .filter(|candidate| record.identity.is_strong_match(&candidate.identity))
                .map(|candidate| locator_group_key(&candidate.port))
                .collect::<HashSet<_>>();
            if strong_groups.len() > 1 {
                set_availability(record, DeviceAvailability::Ambiguous);
                return;
            }
            let activity = record.descriptor.status.activity;
            let id = record.descriptor.id.clone();
            let generation = record.locator_generation;
            record.descriptor = candidate.descriptor.clone();
            record.descriptor.id = id;
            record.descriptor.status.activity = activity;
            record
                .descriptor
                .transport
                .metadata
                .insert("locator_generation".into(), generation.to_string());
            set_availability(record, DeviceAvailability::Available);
        } else {
            set_availability(record, DeviceAvailability::IdentityMismatch);
        }
        return;
    }

    let matches: Vec<_> = scanned
        .iter()
        .filter(|candidate| record.identity.is_strong_match(&candidate.identity))
        .collect();
    let mut groups = BTreeMap::<String, Vec<&ScannedDesktopDevice>>::new();
    for candidate in matches {
        groups
            .entry(locator_group_key(&candidate.port))
            .or_default()
            .push(candidate);
    }
    match groups.len() {
        0 => set_availability(record, DeviceAvailability::Disconnected),
        1 => {
            let candidates = groups.into_values().next().unwrap();
            if let Some(candidate) = select_locator_alias(&record.port, &candidates) {
                adopt_locator(record, candidate);
            } else {
                set_availability(record, DeviceAvailability::Disconnected);
            }
        }
        _ => set_availability(record, DeviceAvailability::Ambiguous),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MacosSerialAddressKind {
    Callout,
    Dialin,
}

fn macos_serial_alias(address: &str) -> Option<(MacosSerialAddressKind, &str)> {
    address
        .strip_prefix("/dev/cu.")
        .map(|name| (MacosSerialAddressKind::Callout, name))
        .or_else(|| {
            address
                .strip_prefix("/dev/tty.")
                .map(|name| (MacosSerialAddressKind::Dialin, name))
        })
}

fn locator_group_key(address: &str) -> String {
    match macos_serial_alias(address) {
        Some((_, name)) => format!("macos:{name}"),
        None => format!("address:{address}"),
    }
}

fn select_locator_alias<'a>(
    previous: &str,
    candidates: &[&'a ScannedDesktopDevice],
) -> Option<&'a ScannedDesktopDevice> {
    let previous_kind = macos_serial_alias(previous).map(|(kind, _)| kind);
    match previous_kind {
        Some(kind) => candidates.iter().copied().find(|candidate| {
            macos_serial_alias(&candidate.port).map(|value| value.0) == Some(kind)
        }),
        None if candidates.len() == 1 => candidates.first().copied(),
        None => None,
    }
}

fn device_id(identity_key: &str) -> DeviceId {
    let digest = Sha256::digest(identity_key.as_bytes());
    let suffix: String = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    DeviceId::new(format!("dev_{suffix}")).expect("fixed-size device ID is valid")
}

fn usb_identity_key(vid: u16, pid: u16, serial: Option<&str>, address: &str) -> String {
    // Address remains part of the phase-1 identity even with a USB serial.
    // Desktop APIs do not reliably expose an interface discriminator, and one
    // physical device may produce multiple ports with the same VID/PID/serial.
    // Reconnecting the same hardware at a new address needs an explicit policy.
    let serial = serial.unwrap_or("<none>");
    format!("usb:{vid:04x}:{pid:04x}:serial:{serial}:address:{address}")
}

fn unknown_usb() -> UsbPortInfo {
    UsbPortInfo {
        vid: 0,
        pid: 0,
        serial_number: None,
        manufacturer: None,
        product: None,
    }
}

impl DeviceBackend for EspflashBackend {
    fn devices(&self) -> Result<Vec<DeviceDescriptor>> {
        let mut records = self.records.lock().unwrap();
        let scanned = Self::scan_devices()?;
        Ok(Self::discover_from_scan(&mut records, &scanned))
    }

    fn refresh(&self, device_id: &DeviceId) -> Result<DeviceDescriptor> {
        Ok(self
            .refresh_record_with(device_id, Self::scan_devices)?
            .descriptor)
    }

    fn validate(&self, plan: &PreparedPlan) -> Result<()> {
        validate(plan).map(|_| ())
    }

    fn open(&self, device_id: &DeviceId, no_reset_before: bool) -> Result<Box<dyn DeviceSession>> {
        // Use the locator snapshot produced by this validation. A concurrent
        // refresh can update the registry for a later operation, but cannot
        // change which path this open attempts.
        let record = self.validated_record(device_id)?;
        let serial = serialport::new(&record.port, 115200)
            .timeout(Duration::from_secs(3))
            .preserve_dtr_on_open()
            .open_native()
            .with_context(|| {
                format!(
                    "open serial port {}; close any other serial monitor first",
                    record.port
                )
            })?;
        let connection = Connection::new(
            serial,
            record.usb,
            ResetAfterOperation::NoResetNoStub,
            if no_reset_before {
                ResetBeforeOperation::NoReset
            } else {
                ResetBeforeOperation::DefaultReset
            },
            115200,
        );
        Ok(Box::new(Session {
            connection: Some(connection),
            protocol_input: false,
        }))
    }
}

struct Session {
    connection: Option<Connection>,
    protocol_input: bool,
}

impl Session {
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
        let temporary = tempfile::NamedTempFile::new()?.into_temp_path();
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

fn validate_detected_size(plan: &PreparedPlan, chip: Chip, detected: u32) -> Result<()> {
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

fn validate(plan: &PreparedPlan) -> Result<Chip> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceActivity;
    use crate::plan::{FlashSettings, PreparedSegment};
    use std::{
        sync::{Arc, mpsc},
        thread,
        time::Duration,
    };

    fn scanned_usb(address: &str, serial: Option<&str>) -> ScannedDesktopDevice {
        let usb = UsbPortInfo {
            vid: 0x303a,
            pid: 0x1001,
            serial_number: serial.map(str::to_owned),
            manufacturer: Some("Espressif".into()),
            product: Some("USB JTAG/serial debug unit".into()),
        };
        let identity = DesktopPhysicalIdentity::from_usb(&usb);
        let identity_key = usb_identity_key(usb.vid, usb.pid, serial, address);
        ScannedDesktopDevice {
            descriptor: DeviceDescriptor {
                id: device_id(&identity_key),
                display_name: "ESP32-S3".into(),
                transport: TransportDescriptor {
                    kind: "desktop_serial".into(),
                    address: Some(address.into()),
                    metadata: BTreeMap::new(),
                },
                status: DeviceStatus::AVAILABLE_IDLE,
                capabilities: vec![DeviceCapability::Flash],
            },
            port: address.into(),
            usb,
            identity,
        }
    }

    fn claimed(device: &ScannedDesktopDevice) -> DesktopDeviceRecord {
        let mut descriptor = device.descriptor.clone();
        descriptor
            .transport
            .metadata
            .insert("locator_generation".into(), "0".into());
        DesktopDeviceRecord {
            descriptor,
            port: device.port.clone(),
            usb: device.usb.clone(),
            identity: device.identity.clone(),
            locator_generation: 0,
        }
    }

    #[test]
    fn boot_handoff_reopen_hint_is_limited_to_native_usb_serial_jtag() {
        let native = scanned_usb("/dev/cu.native", Some("serial-1"));
        assert!(reopen_after_boot_entry(&native.usb));

        let mut bridge = native.usb.clone();
        bridge.product = Some("CP2102 USB to UART Bridge Controller".into());
        assert!(!reopen_after_boot_entry(&bridge));
        bridge.product = Some("USB JTAG/serial debug unit".into());
        bridge.pid = 0xea60;
        assert!(!reopen_after_boot_entry(&bridge));
    }

    #[test]
    fn desktop_id_is_repeatable_but_remains_scoped_to_the_native_address() {
        let first = usb_identity_key(0x303a, 0x1001, Some("serial-1"), "/dev/cu.first");
        assert_eq!(device_id(&first), device_id(&first));
        let moved = usb_identity_key(0x303a, 0x1001, Some("serial-1"), "/dev/cu.second");
        assert_ne!(device_id(&first), device_id(&moved));

        let first = usb_identity_key(0x303a, 0x1001, None, "/dev/cu.first");
        let moved = usb_identity_key(0x303a, 0x1001, None, "/dev/cu.second");
        assert_ne!(device_id(&first), device_id(&moved));
    }

    #[test]
    fn dynamic_discovery_groups_aliases_and_retains_missing_devices() {
        let first_callout = scanned_usb("/dev/cu.first", Some("serial-1"));
        let first_dialin = scanned_usb("/dev/tty.first", Some("serial-1"));
        let second = scanned_usb("/dev/cu.second", Some("serial-2"));
        let mut records = HashMap::new();
        let devices = EspflashBackend::discover_from_scan(
            &mut records,
            &[first_dialin, first_callout.clone(), second.clone()],
        );
        assert_eq!(devices.len(), 2);
        assert!(
            devices
                .iter()
                .any(|device| { device.transport.address.as_deref() == Some("/dev/cu.first") })
        );

        let devices =
            EspflashBackend::discover_from_scan(&mut records, std::slice::from_ref(&second));
        assert_eq!(devices.len(), 2);
        let first = devices
            .iter()
            .find(|device| device.id == first_callout.descriptor.id)
            .unwrap();
        assert_eq!(first.status.availability, DeviceAvailability::Disconnected);
    }

    #[test]
    fn dynamic_discovery_does_not_merge_a_replacement_at_the_same_address() {
        let original = scanned_usb("/dev/cu.first", Some("serial-1"));
        let replacement = scanned_usb("/dev/cu.first", Some("serial-2"));
        let mut records = HashMap::new();
        EspflashBackend::discover_from_scan(&mut records, std::slice::from_ref(&original));
        let devices =
            EspflashBackend::discover_from_scan(&mut records, std::slice::from_ref(&replacement));
        assert_eq!(devices.len(), 2);
        assert_eq!(
            devices
                .iter()
                .find(|device| device.id == original.descriptor.id)
                .unwrap()
                .status
                .availability,
            DeviceAvailability::IdentityMismatch
        );
        assert_eq!(
            devices
                .iter()
                .find(|device| device.id == replacement.descriptor.id)
                .unwrap()
                .status
                .availability,
            DeviceAvailability::Available
        );
    }

    #[test]
    fn dynamic_discovery_marks_duplicate_strong_identities_ambiguous() {
        let mut records = HashMap::new();
        let devices = EspflashBackend::discover_from_scan(
            &mut records,
            &[
                scanned_usb("/dev/cu.first", Some("serial-1")),
                scanned_usb("/dev/cu.second", Some("serial-1")),
            ],
        );
        assert_eq!(devices.len(), 1);
        assert_eq!(
            devices[0].status.availability,
            DeviceAvailability::Ambiguous
        );
    }

    #[test]
    fn same_locator_and_identity_remain_available_without_generation_change() {
        let device = scanned_usb("/dev/cu.first", Some("serial-1"));
        let mut record = claimed(&device);
        record.descriptor.status.activity = DeviceActivity::Monitoring;
        reconcile_record(&mut record, std::slice::from_ref(&device));
        assert_eq!(
            record.descriptor.status.availability,
            DeviceAvailability::Available
        );
        assert_eq!(
            record.descriptor.status.activity,
            DeviceActivity::Monitoring
        );
        assert_eq!(record.locator_generation, 0);
    }

    #[test]
    fn occupied_locator_with_different_identity_is_never_redirected() {
        let original = scanned_usb("/dev/cu.first", Some("serial-1"));
        let replacement = scanned_usb("/dev/cu.first", Some("serial-2"));
        let relocated_original = scanned_usb("/dev/cu.second", Some("serial-1"));
        let mut record = claimed(&original);
        reconcile_record(&mut record, &[replacement, relocated_original]);
        assert_eq!(
            record.descriptor.status.availability,
            DeviceAvailability::IdentityMismatch
        );
        assert_eq!(record.port, "/dev/cu.first");
        assert_eq!(record.locator_generation, 0);
    }

    #[test]
    fn unique_strong_identity_updates_locator_but_preserves_logical_id() {
        let original = scanned_usb("/dev/cu.first", Some("serial-1"));
        let moved = scanned_usb("/dev/cu.second", Some("serial-1"));
        let mut record = claimed(&original);
        let logical_id = record.descriptor.id.clone();
        reconcile_record(&mut record, &[moved]);
        assert_eq!(
            record.descriptor.status.availability,
            DeviceAvailability::Available
        );
        assert_eq!(record.descriptor.id, logical_id);
        assert_eq!(record.port, "/dev/cu.second");
        assert_eq!(
            record.descriptor.transport.address.as_deref(),
            Some("/dev/cu.second")
        );
        assert_eq!(record.locator_generation, 1);
        assert_eq!(
            record.descriptor.transport.metadata["locator_generation"],
            "1"
        );
    }

    #[test]
    fn macos_callout_and_dialin_aliases_are_one_locator() {
        let original = scanned_usb("/dev/cu.old", Some("serial-1"));
        let mut record = claimed(&original);
        reconcile_record(
            &mut record,
            &[
                scanned_usb("/dev/tty.new", Some("serial-1")),
                scanned_usb("/dev/cu.new", Some("serial-1")),
            ],
        );
        assert_eq!(
            record.descriptor.status.availability,
            DeviceAvailability::Available
        );
        assert_eq!(record.port, "/dev/cu.new");
        assert_eq!(record.locator_generation, 1);
    }

    #[test]
    fn macos_relocation_does_not_switch_selected_address_category() {
        let original = scanned_usb("/dev/cu.old", Some("serial-1"));
        let mut record = claimed(&original);
        reconcile_record(
            &mut record,
            &[scanned_usb("/dev/tty.new", Some("serial-1"))],
        );
        assert_eq!(
            record.descriptor.status.availability,
            DeviceAvailability::Disconnected
        );
        assert_eq!(record.port, "/dev/cu.old");
        assert_eq!(record.locator_generation, 0);
    }

    #[test]
    fn duplicate_strong_matches_are_ambiguous_and_do_not_move_locator() {
        let original = scanned_usb("/dev/cu.first", Some("serial-1"));
        let mut record = claimed(&original);
        reconcile_record(
            &mut record,
            &[
                scanned_usb("/dev/cu.second", Some("serial-1")),
                scanned_usb("/dev/cu.third", Some("serial-1")),
            ],
        );
        assert_eq!(
            record.descriptor.status.availability,
            DeviceAvailability::Ambiguous
        );
        assert_eq!(record.port, "/dev/cu.first");
        assert_eq!(record.locator_generation, 0);
    }

    #[test]
    fn missing_or_weak_identity_stays_disconnected() {
        let original = scanned_usb("/dev/cu.first", None);
        let mut record = claimed(&original);
        reconcile_record(&mut record, &[scanned_usb("/dev/cu.second", None)]);
        assert_eq!(
            record.descriptor.status.availability,
            DeviceAvailability::Disconnected
        );
        assert_eq!(record.port, "/dev/cu.first");
    }

    #[test]
    fn registry_serializes_scanning_and_cannot_apply_an_old_result_last() {
        let original = scanned_usb("COM7", Some("serial-1"));
        let device_id = original.descriptor.id.clone();
        let backend = Arc::new(EspflashBackend::default());
        backend
            .records
            .lock()
            .unwrap()
            .insert(device_id.clone(), claimed(&original));

        let (first_scan_tx, first_scan_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();
        let first_backend = backend.clone();
        let first_id = device_id.clone();
        let first = thread::spawn(move || {
            first_backend
                .refresh_record_with(&first_id, || {
                    first_scan_tx.send(()).unwrap();
                    release_first_rx.recv().unwrap();
                    Ok(vec![scanned_usb("COM7", Some("serial-1"))])
                })
                .unwrap();
        });
        first_scan_rx.recv().unwrap();

        let (second_call_tx, second_call_rx) = mpsc::channel();
        let (second_scan_tx, second_scan_rx) = mpsc::channel();
        let second_backend = backend.clone();
        let second_id = device_id.clone();
        let second = thread::spawn(move || {
            second_call_tx.send(()).unwrap();
            second_backend
                .refresh_record_with(&second_id, || {
                    second_scan_tx.send(()).unwrap();
                    Ok(vec![scanned_usb("COM8", Some("serial-1"))])
                })
                .unwrap();
        });
        second_call_rx.recv().unwrap();
        assert!(
            second_scan_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );

        release_first_tx.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        let record = backend.records.lock().unwrap()[&device_id].clone();
        assert_eq!(record.port, "COM8");
        assert_eq!(record.locator_generation, 1);
    }
    fn plan() -> PreparedPlan {
        let mut data = vec![0; 24];
        data[0] = 0xe9;
        data[2] = 2;
        data[3] = 0x2f;
        data[12] = 9;
        PreparedPlan {
            chip: "esp32s3".into(),
            flash_settings: FlashSettings {
                mode: Some("dio".into()),
                size: Some("4MB".into()),
                frequency: Some("80m".into()),
            },
            segments: vec![PreparedSegment {
                offset: 0,
                data,
                original_size: 24,
            }],
        }
    }
    #[test]
    fn preserves_matching_bootloader_bytes() {
        let plan = plan();
        let original = plan.segments[0].data.clone();
        assert_eq!(validate(&plan).unwrap(), Chip::Esp32s3);
        assert_eq!(plan.segments[0].data, original);
    }
    #[test]
    fn rejects_header_mismatch_instead_of_silently_ignoring_settings() {
        let mut plan = plan();
        plan.flash_settings.size = Some("8MB".into());
        assert!(
            validate(&plan)
                .unwrap_err()
                .to_string()
                .contains("disagrees")
        );
        plan.flash_settings.size = Some("keep".into());
        assert!(validate(&plan).is_ok());
        plan.segments[0].data[12] = 5;
        assert!(validate(&plan).is_err());
    }
    #[test]
    fn app_only_validates_metadata_and_capacity_without_requiring_bootloader() {
        let mut plan = plan();
        plan.segments[0].offset = 0x20000;
        assert!(validate(&plan).is_ok());
        plan.flash_settings.frequency = Some("unknown".into());
        assert!(validate(&plan).is_err());
    }

    #[test]
    fn detect_does_not_silently_act_as_keep() {
        let mut plan = plan();
        plan.flash_settings.size = Some("detect".into());
        assert!(validate_detected_size(&plan, Chip::Esp32s3, 4 * 1024 * 1024).is_ok());
        assert!(validate_detected_size(&plan, Chip::Esp32s3, 8 * 1024 * 1024).is_err());
    }
}
