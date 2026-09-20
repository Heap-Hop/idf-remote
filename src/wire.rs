use crate::{
    device::{DeviceDescriptor, DeviceId, DeviceStatus},
    plan::{FlashSettings, PreparedPlan, PreparedSegment},
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, prelude::BASE64_STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub const MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_READ_BYTES: u32 = 32 * 1024 * 1024;
pub const MAX_SERIAL_WRITE_BYTES: usize = 64 * 1024;
pub const ERASE_FLASH_CONFIRMATION: &str = "erase-all-flash";
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;

pub fn validate_idempotency_key(key: &str) -> Result<()> {
    ensure!(
        !key.is_empty() && key.len() <= MAX_IDEMPOTENCY_KEY_BYTES,
        "idempotency key must be 1..128 bytes"
    );
    ensure!(
        key.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')),
        "idempotency key contains unsupported characters"
    );
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FlashUploadManifest {
    pub device_id: DeviceId,
    pub version: u32,
    pub chip: String,
    pub flash_settings: FlashSettings,
    pub segments: Vec<UploadSegmentMetadata>,
    pub flash_baud: u32,
    pub monitor_baud: u32,
    #[serde(default)]
    pub no_reset_before: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<FlashWarning>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum FlashWarning {
    EmptyIdfImageSkipped {
        #[serde(skip_serializing_if = "Option::is_none")]
        image: Option<String>,
        offset: u32,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UploadSegmentMetadata {
    pub part: String,
    pub offset: u32,
    pub size: u64,
    pub sha256: String,
}

impl FlashUploadManifest {
    pub fn from_plan(
        device_id: DeviceId,
        plan: &PreparedPlan,
        flash_baud: u32,
        monitor_baud: u32,
        no_reset_before: bool,
    ) -> Result<Self> {
        ensure!(
            plan.segments.iter().map(|s| s.data.len()).sum::<usize>() <= MAX_UPLOAD_BYTES,
            "HTTP upload exceeds 32 MiB"
        );
        let segments = plan
            .segments
            .iter()
            .enumerate()
            .map(|(index, segment)| UploadSegmentMetadata {
                part: format!("segment-{index}"),
                offset: segment.offset,
                size: segment.original_size as u64,
                sha256: sha256(&segment.data[..segment.original_size]),
            })
            .collect();
        Ok(Self {
            device_id,
            version: 2,
            chip: plan.chip.clone(),
            flash_settings: plan.flash_settings.clone(),
            segments,
            flash_baud,
            monitor_baud,
            no_reset_before,
            warnings: Vec::new(),
        })
    }

    pub fn with_warnings(mut self, warnings: Vec<FlashWarning>) -> Self {
        self.warnings = warnings;
        self
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 2, "unsupported upload version");
        ensure!(!self.chip.is_empty(), "chip is required");
        ensure!(self.warnings.len() <= 64, "too many flash warnings");
        for warning in &self.warnings {
            let FlashWarning::EmptyIdfImageSkipped { image, .. } = warning;
            ensure!(
                image
                    .as_ref()
                    .is_none_or(|name| !name.is_empty() && name.len() <= 128),
                "invalid image name in flash warning"
            );
        }
        ensure!(
            self.flash_baud >= 115200 && self.monitor_baud > 0,
            "invalid baud rate"
        );
        ensure!(
            !self.segments.is_empty() && self.segments.len() <= 64,
            "expected 1..64 segments"
        );
        let mut total = 0u64;
        let mut ranges = Vec::with_capacity(self.segments.len());
        for (index, segment) in self.segments.iter().enumerate() {
            ensure!(
                segment.part == format!("segment-{index}"),
                "segment part names must be canonical"
            );
            ensure!(
                segment.offset % 4 == 0,
                "segment offset must be four-byte aligned"
            );
            total = total
                .checked_add(segment.size)
                .context("upload size overflow")?;
            ensure!(
                segment.size > 0 && total <= MAX_UPLOAD_BYTES as u64,
                "empty artifact or upload exceeds 32 MiB"
            );
            ensure!(
                segment.sha256.len() == 64
                    && segment
                        .sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "artifact SHA256 must be lowercase hex"
            );
            let padded_size = segment.size.next_multiple_of(4);
            ensure!(
                u64::from(segment.offset) + padded_size <= u64::from(u32::MAX) + 1,
                "segment address overflow"
            );
            ranges.push((segment.offset, padded_size));
        }
        ranges.sort_by_key(|range| range.0);
        for pair in ranges.windows(2) {
            ensure!(
                (u64::from(pair[0].0) + pair[0].1).next_multiple_of(4096)
                    <= u64::from(pair[1].0) / 4096 * 4096,
                "segments overlap or share an erase sector"
            );
        }
        Ok(())
    }

    /// No client filesystem paths reach the host. Verify the received binary
    /// parts before queue admission, then transfer ownership to the worker.
    pub fn into_plan(&self, data: Vec<Vec<u8>>) -> Result<PreparedPlan> {
        self.validate()?;
        ensure!(data.len() == self.segments.len(), "missing artifact part");
        let mut segments = Vec::with_capacity(data.len());
        for (metadata, mut data) in self.segments.iter().zip(data) {
            ensure!(data.len() as u64 == metadata.size, "artifact size mismatch");
            ensure!(sha256(&data) == metadata.sha256, "artifact SHA256 mismatch");
            let original_size = data.len();
            data.resize(data.len().next_multiple_of(4), 0xff);
            segments.push(PreparedSegment {
                offset: metadata.offset,
                data,
                original_size,
            });
        }
        segments.sort_by_key(|segment| segment.offset);
        Ok(PreparedPlan {
            chip: self.chip.clone(),
            flash_settings: self.flash_settings.clone(),
            segments,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceRequest {
    pub device_id: DeviceId,
    #[serde(default = "default_baud")]
    pub monitor_baud: u32,
    #[serde(default)]
    pub no_reset_before: bool,
}
fn default_baud() -> u32 {
    115200
}

fn default_flash_baud() -> u32 {
    460800
}

fn default_serial_write_timeout_ms() -> u64 {
    2_000
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFlashRequest {
    pub device_id: DeviceId,
    pub offset: u32,
    pub size: u32,
    #[serde(default = "default_flash_baud")]
    pub flash_baud: u32,
    #[serde(default = "default_baud")]
    pub monitor_baud: u32,
    #[serde(default)]
    pub no_reset_before: bool,
}

impl ReadFlashRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.size > 0, "read size must be positive");
        ensure!(self.size <= MAX_READ_BYTES, "read exceeds 32 MiB limit");
        ensure!(
            self.flash_baud >= 115200,
            "read baud must be at least 115200"
        );
        ensure!(self.monitor_baud > 0, "monitor baud must be positive");
        ensure!(
            u64::from(self.offset) + u64::from(self.size) <= u64::from(u32::MAX) + 1,
            "read address overflow"
        );
        Ok(())
    }

    pub fn device_request(&self) -> DeviceRequest {
        DeviceRequest {
            device_id: self.device_id.clone(),
            monitor_baud: self.monitor_baud,
            no_reset_before: self.no_reset_before,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EraseFlashRequest {
    pub device_id: DeviceId,
    pub confirmation: String,
    #[serde(default = "default_flash_baud")]
    pub flash_baud: u32,
    #[serde(default)]
    pub no_reset_before: bool,
}

impl EraseFlashRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.confirmation == ERASE_FLASH_CONFIRMATION,
            "erase-flash requires confirmation: {ERASE_FLASH_CONFIRMATION}"
        );
        ensure!(
            self.flash_baud >= 115200,
            "erase baud must be at least 115200"
        );
        Ok(())
    }

    pub fn device_request(&self) -> DeviceRequest {
        DeviceRequest {
            device_id: self.device_id.clone(),
            monitor_baud: default_baud(),
            no_reset_before: self.no_reset_before,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SerialWriteRequest {
    pub device_id: DeviceId,
    pub data: String,
    pub sha256: String,
    #[serde(default = "default_baud")]
    pub baud: u32,
    #[serde(default = "default_serial_write_timeout_ms")]
    pub timeout_ms: u64,
}

impl SerialWriteRequest {
    pub fn from_bytes(
        device_id: DeviceId,
        data: &[u8],
        baud: u32,
        timeout_ms: u64,
    ) -> Result<Self> {
        validate_serial_write(data, baud, timeout_ms)?;
        Ok(Self {
            device_id,
            data: BASE64_STANDARD.encode(data),
            sha256: sha256(data),
            baud,
            timeout_ms,
        })
    }

    pub fn decode(&self) -> Result<Vec<u8>> {
        let max_encoded_len = MAX_SERIAL_WRITE_BYTES.div_ceil(3) * 4;
        ensure!(
            self.data.len() <= max_encoded_len,
            "serial-write payload exceeds 64 KiB"
        );
        ensure!(self.sha256.len() == 64, "invalid serial-write SHA256");
        let data = BASE64_STANDARD
            .decode(&self.data)
            .context("invalid serial-write base64")?;
        validate_serial_write(&data, self.baud, self.timeout_ms)?;
        ensure!(sha256(&data) == self.sha256, "serial-write SHA256 mismatch");
        Ok(data)
    }

    pub fn device_request(&self) -> DeviceRequest {
        DeviceRequest {
            device_id: self.device_id.clone(),
            monitor_baud: self.baud,
            no_reset_before: true,
        }
    }
}

fn validate_serial_write(data: &[u8], baud: u32, timeout_ms: u64) -> Result<()> {
    ensure!(!data.is_empty(), "serial-write payload must not be empty");
    ensure!(
        data.len() <= MAX_SERIAL_WRITE_BYTES,
        "serial-write payload exceeds 64 KiB"
    );
    ensure!(baud > 0, "serial-write baud must be positive");
    ensure!(
        (1..=30_000).contains(&timeout_ms),
        "serial-write timeout must be 1..30000 ms"
    );
    Ok(())
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Cursor {
    pub epoch: String,
    pub after: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Event {
    pub seq: u64,
    pub kind: String,
    pub data: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EventBatch {
    pub cursor: Cursor,
    pub events: Vec<Event>,
    pub device_status: DeviceStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EventQuery {
    pub device_id: DeviceId,
    pub epoch: String,
    pub after: u64,
}

impl EventQuery {
    pub fn new(device_id: DeviceId, cursor: Cursor) -> Self {
        Self {
            device_id,
            epoch: cursor.epoch,
            after: cursor.after,
        }
    }

    pub fn cursor(&self) -> Cursor {
        Cursor {
            epoch: self.epoch.clone(),
            after: self.after,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DevicesResponse {
    pub devices: Vec<DeviceDescriptor>,
    pub cursors: HashMap<DeviceId, Cursor>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArtifactMetadata {
    pub size: u64,
    pub sha256: String,
    pub content_type: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Operation {
    pub id: String,
    pub device_id: DeviceId,
    pub request_key: String,
    pub status: String,
    pub start_cursor: Cursor,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactMetadata>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaitRequest {
    pub device_id: DeviceId,
    pub cursor: Cursor,
    pub pattern: String,
    pub timeout_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_cursors_and_event_queries_are_device_scoped() {
        let device_id = DeviceId::new("dev_test").unwrap();
        let cursor = Cursor {
            epoch: "epoch".into(),
            after: 7,
        };
        let response = DevicesResponse {
            devices: Vec::new(),
            cursors: HashMap::from([(device_id.clone(), cursor.clone())]),
        };
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["cursors"]["dev_test"]["after"], 7);

        let query = serde_json::to_value(EventQuery::new(device_id, cursor)).unwrap();
        assert_eq!(query["device_id"], "dev_test");
        assert_eq!(query["epoch"], "epoch");
        assert_eq!(query["after"], 7);
    }

    #[test]
    fn operation_requests_accept_device_id_and_reject_legacy_port_fields() {
        let request: DeviceRequest = serde_json::from_value(serde_json::json!({
            "device_id": "dev_test",
            "monitor_baud": 115200
        }))
        .unwrap();
        assert_eq!(request.device_id, DeviceId::new("dev_test").unwrap());
        assert!(
            serde_json::from_value::<DeviceRequest>(serde_json::json!({
                "port": "COM7",
                "monitor_baud": 115200
            }))
            .is_err()
        );
    }

    #[test]
    fn idempotency_keys_have_bounded_header_safe_syntax() {
        validate_idempotency_key("flash_2026-09-08:01").unwrap();
        assert!(validate_idempotency_key("").is_err());
        assert!(validate_idempotency_key(&"a".repeat(129)).is_err());
        assert!(validate_idempotency_key("contains a space").is_err());
        assert!(validate_idempotency_key("non-ascii-中文").is_err());
    }

    #[test]
    fn verifies_artifact_digest_and_erase_ranges_before_admission() {
        let plan = PreparedPlan {
            chip: "esp32s3".into(),
            flash_settings: FlashSettings::default(),
            segments: vec![PreparedSegment {
                offset: 0x10000,
                data: vec![1, 2, 3, 4],
                original_size: 4,
            }],
        };
        let mut upload = FlashUploadManifest::from_plan(
            DeviceId::new("dev_test").unwrap(),
            &plan,
            460800,
            115200,
            false,
        )
        .unwrap();
        assert_eq!(
            upload.into_plan(vec![vec![1, 2, 3, 4]]).unwrap().segments[0].data,
            [1, 2, 3, 4]
        );
        upload.segments[0].sha256 = "wrong".into();
        assert!(upload.into_plan(vec![vec![1, 2, 3, 4]]).is_err());
        upload.segments[0].sha256 = sha256(&[1, 2, 3, 4]);
        upload.segments.push(UploadSegmentMetadata {
            part: "segment-1".into(),
            offset: 0x10004,
            size: 4,
            sha256: upload.segments[0].sha256.clone(),
        });
        assert!(
            upload
                .into_plan(vec![vec![1, 2, 3, 4], vec![1, 2, 3, 4]])
                .is_err()
        );
    }

    #[test]
    fn read_flash_requests_are_bounded_before_hardware_admission() {
        let mut request = ReadFlashRequest {
            device_id: DeviceId::new("dev_test").unwrap(),
            offset: 0x1000,
            size: 0x2000,
            flash_baud: 460800,
            monitor_baud: 115200,
            no_reset_before: false,
        };
        request.validate().unwrap();
        assert_eq!(request.device_request().device_id, request.device_id);
        request.size = 0;
        assert!(request.validate().is_err());
        request.size = MAX_READ_BYTES + 1;
        assert!(request.validate().is_err());
        request.size = 2;
        request.offset = u32::MAX;
        assert!(request.validate().is_err());
    }

    #[test]
    fn serial_write_verifies_payload_and_bounds_before_admission() {
        let data = b"status\r\n";
        let mut request =
            SerialWriteRequest::from_bytes(DeviceId::new("dev_test").unwrap(), data, 115200, 2_000)
                .unwrap();
        assert_eq!(request.decode().unwrap(), data);
        assert!(request.device_request().no_reset_before);

        request.sha256 = "wrong".into();
        assert!(request.decode().is_err());
        request.sha256 = sha256(data);
        request.timeout_ms = 0;
        assert!(request.decode().is_err());
        request.data = "A".repeat(MAX_SERIAL_WRITE_BYTES.div_ceil(3) * 4 + 1);
        assert!(request.decode().is_err());
        assert!(
            SerialWriteRequest::from_bytes(
                DeviceId::new("dev_test").unwrap(),
                &vec![0; MAX_SERIAL_WRITE_BYTES + 1],
                115200,
                2_000,
            )
            .is_err()
        );
    }

    #[test]
    fn erase_flash_requires_exact_confirmation_before_admission() {
        let mut request = EraseFlashRequest {
            device_id: DeviceId::new("dev_test").unwrap(),
            confirmation: ERASE_FLASH_CONFIRMATION.into(),
            flash_baud: 460800,
            no_reset_before: false,
        };
        request.validate().unwrap();
        request.confirmation = "yes".into();
        assert!(request.validate().is_err());
        request.confirmation = ERASE_FLASH_CONFIRMATION.into();
        request.flash_baud = 115199;
        assert!(request.validate().is_err());
    }
}

/// Experimental application protocol: omit command to explicitly negotiate.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationRequest {
    pub device_id: DeviceId,
    #[serde(default = "default_baud")]
    pub monitor_baud: u32,
    #[serde(default = "default_serial_write_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub command: Option<crate::application::Command>,
}
impl ApplicationRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.monitor_baud > 0, "monitor baud must be positive");
        ensure!(
            (1..=30_000).contains(&self.timeout_ms),
            "timeout must be 1..30000 ms"
        );
        if let Some(command) = &self.command {
            command.payload()?;
        }
        Ok(())
    }
}
