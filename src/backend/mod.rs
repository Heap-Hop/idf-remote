mod espflash;

use anyhow::Result;
use serde::Serialize;
use std::io::{Read, Write};

pub use self::espflash::EspflashBackend;
use crate::{
    device::{DeviceDescriptor, DeviceId},
    plan::PreparedPlan,
};

#[derive(Debug, Serialize)]
pub struct BoardInfo {
    pub chip: String,
    pub flash_size_bytes: u32,
    pub revision: Option<(u32, u32)>,
    pub mac_address: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Progress {
    SegmentStarted {
        offset: u32,
        total: usize,
    },
    SegmentProgress {
        offset: u32,
        written: usize,
        total: usize,
    },
    Verifying {
        offset: u32,
    },
    SegmentFinished {
        offset: u32,
    },
}

/// Synchronous hardware boundary. The later daemon will run this on device
/// workers, not on the HTTP executor. espflash types stay inside its adapter.
pub trait DeviceBackend: Send + Sync + 'static {
    fn devices(&self) -> Result<Vec<DeviceDescriptor>>;
    fn refresh(&self, device_id: &DeviceId) -> Result<DeviceDescriptor>;
    fn validate(&self, plan: &PreparedPlan) -> Result<()>;
    fn open(&self, device_id: &DeviceId, no_reset_before: bool) -> Result<Box<dyn DeviceSession>>;
}

/// One owner for one open native port. No clone or secondary monitor handle.
pub trait DeviceSession {
    fn probe(&mut self) -> Result<BoardInfo>;
    fn flash(
        &mut self,
        plan: &PreparedPlan,
        baud: u32,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<BoardInfo>;
    fn read_flash(
        &mut self,
        offset: u32,
        size: u32,
        baud: u32,
        output: &mut dyn Write,
    ) -> Result<BoardInfo>;
    fn erase_flash(&mut self, baud: u32) -> Result<BoardInfo>;
    fn into_monitor(self: Box<Self>, baud: u32, reset: bool) -> Result<Box<dyn SerialIo>>;
}

pub trait SerialIo: Read + Write {}
impl<T: Read + Write> SerialIo for T {}

// A narrow seam for proving handoff ordering without connected hardware.
trait MonitorControl {
    fn set_baud(&mut self, baud: u32) -> Result<()>;
    fn clear_protocol_input(&mut self) -> Result<()>;
    fn release_boot_pin(&mut self) -> Result<()>;
    fn reset(&mut self) -> Result<()>;
}

fn prepare_monitor(
    port: &mut impl MonitorControl,
    baud: u32,
    protocol_input: bool,
    reset: bool,
) -> Result<()> {
    port.set_baud(baud)?;
    // An explicit reset defines a new log boundary even if this handle has
    // never spoken the bootloader protocol. Discard old UART bytes beforehand.
    if protocol_input || reset {
        port.clear_protocol_input()?;
    }
    if reset {
        port.release_boot_pin()?;
        port.reset()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Port {
        calls: Vec<&'static str>,
        fail_baud: bool,
    }
    impl MonitorControl for Port {
        fn set_baud(&mut self, _: u32) -> Result<()> {
            self.calls.push("baud");
            anyhow::ensure!(!self.fail_baud, "baud failed");
            Ok(())
        }
        fn clear_protocol_input(&mut self) -> Result<()> {
            self.calls.push("clear");
            Ok(())
        }
        fn reset(&mut self) -> Result<()> {
            self.calls.push("reset");
            Ok(())
        }
        fn release_boot_pin(&mut self) -> Result<()> {
            self.calls.push("boot-pin");
            Ok(())
        }
    }
    #[test]
    fn configures_capture_before_reset_and_never_clears_boot_output() {
        let mut port = Port::default();
        prepare_monitor(&mut port, 115200, true, true).unwrap();
        assert_eq!(port.calls, ["baud", "clear", "boot-pin", "reset"]);
    }
    #[test]
    fn passive_monitor_does_not_reset_or_discard_input() {
        let mut port = Port::default();
        prepare_monitor(&mut port, 115200, false, false).unwrap();
        assert_eq!(port.calls, ["baud"]);
    }
    #[test]
    fn reset_discards_previous_run_bytes_before_releasing_boot() {
        let mut port = Port::default();
        prepare_monitor(&mut port, 115200, false, true).unwrap();
        assert_eq!(port.calls, ["baud", "clear", "boot-pin", "reset"]);
    }
    #[test]
    fn failed_setup_does_not_start_firmware() {
        let mut port = Port {
            fail_baud: true,
            ..Default::default()
        };
        assert!(prepare_monitor(&mut port, 115200, true, true).is_err());
        assert_eq!(port.calls, ["baud"]);
    }
}
