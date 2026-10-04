//! Native USB backend for Android-authorized usbdevfs file descriptors.
//!
//! The caller owns permission and hotplug handling. Each attachment gets a fresh
//! identity; an old fd/session is never silently rebound to another USB device.
use super::android_flashing::{Session, validate};
use super::{DeviceBackend, DeviceSession};
use crate::{device::*, plan::PreparedPlan};
use anyhow::{Context, Result, ensure};
use espflash_android::connection::{
    Connection, ResetAfterOperation, ResetBeforeOperation, Transport,
};
use nusb::{
    Device, Interface, MaybeFuture,
    io::{EndpointRead, EndpointWrite},
    transfer::{Bulk, ControlOut, ControlType, In, Out, Recipient},
};
use serialport::{ClearBuffer, UsbPortInfo};
use std::{
    collections::{BTreeMap, HashMap},
    io::{self, Read, Write},
    os::fd::OwnedFd,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

// Keep IN requests queued while espflash writes or waits between replies.
// A demand-only reader loses burst ROM SYNC replies on native USB Serial/JTAG.
const RX_TRANSFERS: usize = 8;

static NEXT_ATTACHMENT: AtomicU64 = AtomicU64::new(1);

/// Registry of Android-authorized connections. Does not enumerate USB or request permission.
#[derive(Default)]
pub struct AndroidUsbBackend {
    records: Mutex<HashMap<DeviceId, Arc<Record>>>,
    cache_dir: Option<std::path::PathBuf>,
}
struct Record {
    device: Device,
    descriptor: DeviceDescriptor,
    layout: Layout,
    attached: AtomicBool,
    leased: AtomicBool,
}
#[derive(Debug, PartialEq)]
struct Layout {
    control: u8,
    data: u8,
    input: u8,
    output: u8,
}
fn layout(device: &Device) -> Result<Layout> {
    parse_layout(device.active_configuration()?)
}
fn parse_layout(config: nusb::descriptors::ConfigurationDescriptor<'_>) -> Result<Layout> {
    let mut control = vec![];
    let mut data = vec![];
    for group in config.interfaces() {
        for intf in group.alt_settings().filter(|i| i.alternate_setting() == 0) {
            if intf.class() == 2 && intf.subclass() == 2 {
                control.push(intf.interface_number());
            }
            if intf.class() == 10 {
                let endpoints: Vec<_> = intf
                    .endpoints()
                    .filter(|e| e.transfer_type() == nusb::descriptors::TransferType::Bulk)
                    .collect();
                let input: Vec<_> = endpoints
                    .iter()
                    .filter(|e| e.address() & 0x80 != 0)
                    .collect();
                let output: Vec<_> = endpoints
                    .iter()
                    .filter(|e| e.address() & 0x80 == 0)
                    .collect();
                ensure!(
                    input.len() == 1 && output.len() == 1,
                    "expected one CDC bulk endpoint in each direction"
                );
                data.push((
                    intf.interface_number(),
                    input[0].address(),
                    output[0].address(),
                ));
            }
        }
    }
    ensure!(
        control.len() == 1 && data.len() == 1,
        "expected exactly one CDC-ACM function"
    );
    Ok(Layout {
        control: control[0],
        data: data[0].0,
        input: data[0].1,
        output: data[0].2,
    })
}
impl AndroidUsbBackend {
    /// Use an app-private directory for temporary flash readback files.
    pub fn with_cache_dir(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            cache_dir: Some(path.into()),
            ..Self::default()
        }
    }

    /// Takes an owned duplicate, not the fd owned by UsbDeviceConnection.
    /// Keep the Android connection alive until workers stop and this backend is dropped.
    /// Blocking: call off the Android UI thread.
    pub fn attach(&self, fd: OwnedFd) -> Result<DeviceDescriptor> {
        let device = Device::from_fd(fd)
            .wait()
            .context("import Android USB fd")?;
        let desc = device.device_descriptor();
        ensure!(
            (desc.vendor_id(), desc.product_id()) == (0x303a, 0x1001),
            "only Espressif native USB Serial/JTAG (303a:1001) is supported"
        );
        let layout = layout(&device)?;
        let id = DeviceId::new(format!(
            "android_usb_{}",
            NEXT_ATTACHMENT.fetch_add(1, Ordering::Relaxed)
        ))
        .map_err(anyhow::Error::msg)?;
        let descriptor = DeviceDescriptor {
            id: id.clone(),
            display_name: "Espressif USB Serial/JTAG".into(),
            transport: TransportDescriptor {
                kind: "android_usb".into(),
                address: None,
                metadata: BTreeMap::from([
                    ("vid".into(), "303a".into()),
                    ("pid".into(), "1001".into()),
                    ("identity_scope".into(), "attachment".into()),
                ]),
            },
            status: DeviceStatus::AVAILABLE_IDLE,
            capabilities: vec![
                DeviceCapability::Probe,
                DeviceCapability::Flash,
                DeviceCapability::ReadFlash,
                DeviceCapability::EraseFlash,
                DeviceCapability::Monitor,
                DeviceCapability::SerialWrite,
                DeviceCapability::Reset,
            ],
        };
        self.records.lock().unwrap().insert(
            id,
            Arc::new(Record {
                device,
                descriptor: descriptor.clone(),
                layout,
                attached: AtomicBool::new(true),
                leased: AtomicBool::new(false),
            }),
        );
        Ok(descriptor)
    }
    /// Invalidate first; worker IO then fails instead of using a replacement device.
    pub fn detach(&self, id: &DeviceId) {
        if let Some(record) = self.records.lock().unwrap().remove(id) {
            record.attached.store(false, Ordering::Release);
        }
    }
    fn record(&self, id: &DeviceId) -> Result<Arc<Record>> {
        self.records
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .context("USB attachment disconnected")
    }
}
impl DeviceBackend for AndroidUsbBackend {
    fn devices(&self) -> Result<Vec<DeviceDescriptor>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .values()
            .map(|r| r.descriptor.clone())
            .collect())
    }
    fn refresh(&self, id: &DeviceId) -> Result<DeviceDescriptor> {
        Ok(self.record(id)?.descriptor.clone())
    }
    fn validate(&self, plan: &PreparedPlan) -> Result<()> {
        validate(plan).map(|_| ())
    }
    fn open(&self, id: &DeviceId, no_reset_before: bool) -> Result<Box<dyn DeviceSession>> {
        let record = self.record(id)?;
        ensure!(
            !record.leased.swap(true, Ordering::AcqRel),
            "USB attachment already has an owner"
        );
        let lease = Lease(record.clone());
        // nusb's Android linux_usbfs implementation supports atomic detach/claim.
        // It refuses to steal an interface from another usbfs user.
        let control = record
            .device
            .detach_and_claim_interface(record.layout.control)
            .wait()
            .context("claim CDC control")?;
        let data = record
            .device
            .detach_and_claim_interface(record.layout.data)
            .wait()
            .context("claim CDC data")?;
        let reader = data
            .endpoint::<Bulk, In>(record.layout.input)?
            .reader(64)
            .with_num_transfers(RX_TRANSFERS)
            .with_read_timeout(Duration::from_secs(3));
        let writer = data
            .endpoint::<Bulk, Out>(record.layout.output)?
            .writer(256)
            .with_num_transfers(2)
            .with_write_timeout(Duration::from_secs(3));
        let io = UsbIo {
            reader,
            writer,
            control,
            timeout: Duration::from_secs(3),
            baud: 115200,
            line_bits: 0,
            pending: std::collections::VecDeque::new(),
            lease,
        };
        let connection = Connection::new(
            Box::new(io),
            UsbPortInfo {
                vid: 0x303a,
                pid: 0x1001,
                serial_number: None,
                manufacturer: None,
                product: None,
            },
            ResetAfterOperation::NoResetNoStub,
            if no_reset_before {
                ResetBeforeOperation::NoReset
            } else {
                ResetBeforeOperation::DefaultReset
            },
            115200,
        );
        Ok(Box::new(Session::new(connection, self.cache_dir.clone())))
    }
}
// Declared last in UsbIo: endpoints are released before allowing another open.
struct Lease(Arc<Record>);
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.leased.store(false, Ordering::Release);
    }
}
struct UsbIo {
    reader: EndpointRead<Bulk>,
    writer: EndpointWrite<Bulk>,
    control: Interface,
    timeout: Duration,
    baud: u32,
    line_bits: u16,
    pending: std::collections::VecDeque<u8>,
    lease: Lease,
}
impl UsbIo {
    fn connected(&self) -> io::Result<()> {
        if !self.lease.0.attached.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "USB detached"));
        }
        Ok(())
    }
    fn lines(&mut self, bits: u16) -> Result<()> {
        self.connected()?;
        self.control
            .control_out(
                ControlOut {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request: 0x22,
                    value: bits,
                    index: self.control.interface_number() as u16,
                    data: &[],
                },
                Duration::from_secs(1),
            )
            .wait()?;
        self.line_bits = bits;
        Ok(())
    }
}
impl Read for UsbIo {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        if b.is_empty() {
            return Ok(0);
        }
        self.connected()?;
        if !self.pending.is_empty() {
            let n = b.len().min(self.pending.len());
            for byte in &mut b[..n] {
                *byte = self.pending.pop_front().unwrap();
            }
            return Ok(n);
        }
        self.reader.read(b)
    }
}
impl Write for UsbIo {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.connected()?;
        let n = self.writer.write(b)?;
        // Force a short packet for small commands without waiting for USB completion.
        // Service success means accepted by driver, matching the desktop contract.
        self.writer.submit();
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.connected()?;
        self.writer.flush()
    }
}
impl std::fmt::Debug for UsbIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AndroidUsbTransport")
            .finish_non_exhaustive()
    }
}
fn port_error(error: anyhow::Error) -> serialport::Error {
    serialport::Error::new(serialport::ErrorKind::Unknown, format!("{error:#}"))
}
impl Transport for UsbIo {
    fn name(&self) -> Option<String> {
        Some(self.lease.0.descriptor.id.to_string())
    }
    fn baud_rate(&self) -> serialport::Result<u32> {
        Ok(self.baud)
    }
    fn set_baud_rate(&mut self, baud: u32) -> serialport::Result<()> {
        // Native USB Serial/JTAG has a fixed USB link rate; no UART clock to configure.
        self.connected()?;
        self.baud = baud;
        Ok(())
    }
    fn timeout(&self) -> Duration {
        self.timeout
    }
    fn set_timeout(&mut self, timeout: Duration) -> serialport::Result<()> {
        self.reader.set_read_timeout(timeout);
        self.writer.set_write_timeout(timeout);
        self.timeout = timeout;
        Ok(())
    }
    fn bytes_to_read(&mut self) -> serialport::Result<u32> {
        self.reader.set_read_timeout(Duration::ZERO);
        let mut bytes = [0; 64];
        let result = self.reader.read(&mut bytes);
        self.reader.set_read_timeout(self.timeout);
        match result {
            Ok(n) => self.pending.extend(&bytes[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(e) => return Err(e.into()),
        }
        Ok(self.pending.len() as u32)
    }
    fn clear(&mut self, buffer: ClearBuffer) -> serialport::Result<()> {
        self.connected()?;
        if matches!(buffer, ClearBuffer::Output | ClearBuffer::All) {
            // espflash only purges input. Do not report a drain as an output purge.
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "USB output purge is unsupported",
            )
            .into());
        }
        if matches!(buffer, ClearBuffer::Input | ClearBuffer::All) {
            self.pending.clear();
            // Stop submissions so an active firmware log stream cannot make clear unbounded.
            self.reader.cancel_all();
            self.reader.set_read_timeout(Duration::from_secs(1));
            let mut discarded = [0; 256];
            let result = (|| -> io::Result<()> {
                loop {
                    match self.reader.read(&mut discarded) {
                        Ok(0) => return Ok(()),
                        Ok(_) => {}
                        Err(e) => return Err(e),
                    }
                }
            })();
            self.reader.set_read_timeout(self.timeout);
            self.reader.set_num_transfers(RX_TRANSFERS);
            result?;
        }
        Ok(())
    }
    fn write_data_terminal_ready(&mut self, level: bool) -> serialport::Result<()> {
        self.lines((self.line_bits & !1) | u16::from(level))
            .map_err(port_error)
    }
    fn write_request_to_send(&mut self, level: bool) -> serialport::Result<()> {
        self.lines((self.line_bits & !2) | (u16::from(level) << 1))
            .map_err(port_error)
    }
    fn set_dtr_rts(&mut self, dtr: bool, rts: bool) -> serialport::Result<()> {
        self.lines(u16::from(dtr) | (u16::from(rts) << 1))
            .map_err(port_error)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_missing_cdc_function() {
        assert!(
            parse_layout(
                nusb::descriptors::ConfigurationDescriptor::new(&[9, 2, 9, 0, 0, 1, 0, 128, 50])
                    .unwrap()
            )
            .is_err()
        );
    }
    #[test]
    fn cdc_bulk_endpoints_exclude_interrupt_and_vendor_interfaces() {
        let raw = [
            9, 2, 57, 0, 3, 1, 0, 128, 50, 9, 4, 0, 0, 1, 2, 2, 0, 0, 7, 5, 0x81, 3, 64, 0, 1, 9,
            4, 1, 0, 2, 10, 0, 0, 0, 7, 5, 0x02, 2, 64, 0, 0, 7, 5, 0x82, 2, 64, 0, 0, 9, 4, 2, 0,
            0, 0xff, 0, 0, 0,
        ];
        assert_eq!(
            parse_layout(nusb::descriptors::ConfigurationDescriptor::new(&raw).unwrap()).unwrap(),
            Layout {
                control: 0,
                data: 1,
                input: 0x82,
                output: 0x02
            }
        );
        let mut invalid = raw;
        invalid[43] = 0x03;
        assert!(
            parse_layout(nusb::descriptors::ConfigurationDescriptor::new(&invalid).unwrap())
                .is_err()
        );
    }
    #[test]
    fn detach_unknown_id_is_safe() {
        let backend = AndroidUsbBackend::default();
        let id = DeviceId::new("absent").unwrap();
        backend.detach(&id);
        assert!(backend.devices().unwrap().is_empty());
        assert!(backend.refresh(&id).is_err());
    }
}
