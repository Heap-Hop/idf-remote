//! Native USB backend for Android-authorized usbdevfs file descriptors.
//!
//! The caller owns permission and hotplug handling. Each attachment gets a fresh
//! identity; an old fd/session is never silently rebound to another USB device.
use super::{BoardInfo, DeviceBackend, DeviceSession, Progress, SerialIo};
use crate::{device::*, plan::PreparedPlan};
use anyhow::{Context, Result, bail, ensure};
use nusb::{
    Device, Interface, MaybeFuture,
    io::{EndpointRead, EndpointWrite},
    transfer::{Bulk, ControlOut, ControlType, In, Out, Recipient},
};
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

static NEXT_ATTACHMENT: AtomicU64 = AtomicU64::new(1);

/// Registry of Android-authorized connections. Does not enumerate USB or request permission.
#[derive(Default)]
pub struct AndroidUsbBackend {
    records: Mutex<HashMap<DeviceId, Arc<Record>>>,
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
    fn validate(&self, _: &PreparedPlan) -> Result<()> {
        bail!("Android USB flashing is not implemented")
    }
    fn open(&self, id: &DeviceId, _: bool) -> Result<Box<dyn DeviceSession>> {
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
            .with_read_timeout(Duration::from_millis(5));
        let writer = data
            .endpoint::<Bulk, Out>(record.layout.output)?
            .writer(256)
            .with_num_transfers(2)
            .with_write_timeout(Duration::from_millis(5));
        Ok(Box::new(UsbSession {
            io: UsbIo {
                reader,
                writer,
                control,
                lease,
            },
        }))
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
    lease: Lease,
}
impl UsbIo {
    fn connected(&self) -> io::Result<()> {
        if !self.lease.0.attached.load(Ordering::Acquire) {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "USB detached"));
        }
        Ok(())
    }
    fn lines(&self, bits: u16) -> Result<()> {
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
        Ok(())
    }
    fn reset(&self) -> Result<()> {
        self.lines(0)?;
        std::thread::sleep(Duration::from_millis(100));
        self.lines(2)?;
        std::thread::sleep(Duration::from_millis(100));
        self.lines(0)
    }
}
impl Read for UsbIo {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.connected()?;
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
struct UsbSession {
    io: UsbIo,
}
impl DeviceSession for UsbSession {
    fn probe(&mut self) -> Result<BoardInfo> {
        bail!("Android bootloader probing is not implemented")
    }
    fn flash(
        &mut self,
        _: &PreparedPlan,
        _: u32,
        _: &mut dyn FnMut(Progress),
    ) -> Result<BoardInfo> {
        bail!("Android flashing is not implemented")
    }
    fn read_flash(&mut self, _: u32, _: u32, _: u32, _: &mut dyn Write) -> Result<BoardInfo> {
        bail!("Android flash reading is not implemented")
    }
    fn erase_flash(&mut self, _: u32) -> Result<BoardInfo> {
        bail!("Android flash erasing is not implemented")
    }
    fn into_monitor(self: Box<Self>, baud: u32, reset: bool) -> Result<Box<dyn SerialIo>> {
        ensure!(baud > 0, "baud must be positive");
        if reset {
            self.io.reset()?;
        }
        Ok(Box::new(self.io))
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
