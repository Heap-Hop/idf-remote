use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const MAX_ARTIFACT_BYTES: usize = 128 * 1024 * 1024;
const MAX_PLAN_BYTES: u64 = 1024 * 1024;
const SECTOR_SIZE: u64 = 4096;

/// Portable metadata. Paths are resolved by the client, never by a future server.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FlashPlan {
    pub version: u32,
    pub chip: String,
    #[serde(default)]
    pub flash_settings: FlashSettings,
    pub segments: Vec<FileSegment>,
}

/// Values assert the existing bootloader header; phase 1 never rewrites images.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FlashSettings {
    pub mode: Option<String>,
    pub frequency: Option<String>,
    pub size: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileSegment {
    #[serde(deserialize_with = "deserialize_offset")]
    pub offset: u32,
    pub file: PathBuf,
}

#[derive(Debug)]
pub struct PreparedPlan {
    pub chip: String,
    pub flash_settings: FlashSettings,
    pub segments: Vec<PreparedSegment>,
}

#[derive(Debug)]
pub struct PreparedSegment {
    pub offset: u32,
    pub data: Vec<u8>,
    pub original_size: usize,
}

pub fn parse_offset(value: &str) -> Result<u32> {
    let result = if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16)
    } else {
        value.parse()
    };
    result.with_context(|| format!("invalid flash offset: {value}"))
}

fn deserialize_offset<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Offset {
        Number(u32),
        Text(String),
    }
    match Offset::deserialize(deserializer)? {
        Offset::Number(value) => Ok(value),
        Offset::Text(value) => parse_offset(&value).map_err(serde::de::Error::custom),
    }
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_PLAN_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_PLAN_BYTES,
        "manifest exceeds 1 MiB"
    );
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

impl FlashPlan {
    pub fn load(path: &Path) -> Result<Self> {
        read_json(path)
    }

    /// Snapshot every artifact before opening hardware. Account for both the
    /// backend's four-byte padding and flash erase sectors when checking ranges.
    pub fn prepare(&self, base: &Path) -> Result<PreparedPlan> {
        ensure!(
            self.version == 1,
            "unsupported FlashPlan version {}",
            self.version
        );
        ensure!(!self.chip.is_empty(), "chip is required");
        ensure!(
            !self.segments.is_empty() && self.segments.len() <= 64,
            "expected 1..64 segments"
        );
        let mut total = 0;
        let mut segments = Vec::new();
        for segment in &self.segments {
            ensure!(
                segment.offset % 4 == 0,
                "offset {:#x} must be four-byte aligned",
                segment.offset
            );
            let path = base.join(&segment.file);
            let file =
                File::open(&path).with_context(|| format!("open artifact {}", path.display()))?;
            let mut data = Vec::new();
            file.take((MAX_ARTIFACT_BYTES - total + 1) as u64)
                .read_to_end(&mut data)?;
            total += data.len();
            ensure!(total <= MAX_ARTIFACT_BYTES, "artifacts exceed 128 MiB");
            ensure!(!data.is_empty(), "empty artifact: {}", path.display());
            let original_size = data.len();
            data.resize(data.len().next_multiple_of(4), 0xff);
            ensure!(
                u64::from(segment.offset) + data.len() as u64 <= u64::from(u32::MAX) + 1,
                "segment at {:#x} exceeds the flash address space",
                segment.offset
            );
            segments.push(PreparedSegment {
                offset: segment.offset,
                data,
                original_size,
            });
        }
        segments.sort_by_key(|s| s.offset);
        for pair in segments.windows(2) {
            let erase_end = (u64::from(pair[0].offset) + pair[0].data.len() as u64)
                .next_multiple_of(SECTOR_SIZE);
            let next_start = u64::from(pair[1].offset) / SECTOR_SIZE * SECTOR_SIZE;
            ensure!(
                erase_end <= next_start,
                "segments at {:#x} and {:#x} overlap or share an erase sector",
                pair[0].offset,
                pair[1].offset
            );
        }
        Ok(PreparedPlan {
            chip: self.chip.clone(),
            flash_settings: self.flash_settings.clone(),
            segments,
        })
    }
}

impl PreparedPlan {
    pub fn validate_capacity(&self, size: u32) -> Result<()> {
        for segment in &self.segments {
            ensure!(
                u64::from(segment.offset) + segment.data.len() as u64 <= u64::from(size),
                "segment at {:#x} exceeds flash capacity ({size} bytes)",
                segment.offset
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plan(offsets: &[u32]) -> FlashPlan {
        FlashPlan {
            version: 1,
            chip: "esp32s3".into(),
            flash_settings: FlashSettings::default(),
            segments: offsets
                .iter()
                .map(|offset| FileSegment {
                    offset: *offset,
                    file: "image.bin".into(),
                })
                .collect(),
        }
    }
    fn artifact(bytes: &[u8]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("image.bin"), bytes).unwrap();
        dir
    }
    #[test]
    fn accepts_numeric_and_hex_offsets() {
        for value in ["65536", "\"0x10000\""] {
            let s: FileSegment =
                serde_json::from_str(&format!(r#"{{"offset":{value},"file":"app.bin"}}"#)).unwrap();
            assert_eq!(s.offset, 65536);
        }
        assert!(parse_offset("0x100000000").is_err());
        assert!(parse_offset("-1").is_err());
    }
    #[test]
    fn snapshots_sorts_and_pads_without_mutating_artifacts() {
        let dir = artifact(&[1, 2, 3]);
        let prepared = plan(&[0x10000, 0]).prepare(dir.path()).unwrap();
        assert_eq!(prepared.segments[0].offset, 0);
        assert_eq!(prepared.segments[0].data, [1, 2, 3, 255]);
        assert_eq!(prepared.segments[0].original_size, 3);
        std::fs::write(dir.path().join("image.bin"), [9]).unwrap();
        assert_eq!(prepared.segments[0].data, [1, 2, 3, 255]);
    }
    #[test]
    fn rejects_shared_erase_sectors_even_without_byte_overlap() {
        let dir = artifact(&[1; 4]);
        assert!(
            plan(&[0, 4])
                .prepare(dir.path())
                .unwrap_err()
                .to_string()
                .contains("erase sector")
        );
        assert!(plan(&[0, 0]).prepare(dir.path()).is_err());
        assert!(plan(&[0, 4096]).prepare(dir.path()).is_ok());
    }
    #[test]
    fn rejects_invalid_artifacts_ranges_and_versions() {
        let dir = artifact(&[1; 8]);
        assert!(plan(&[u32::MAX - 3]).prepare(dir.path()).is_err());
        assert!(plan(&[1]).prepare(dir.path()).is_err());
        assert!(plan(&[]).prepare(dir.path()).is_err());
        let mut wrong = plan(&[0]);
        wrong.version = 2;
        assert!(wrong.prepare(dir.path()).is_err());
        let prepared = plan(&[4096]).prepare(dir.path()).unwrap();
        assert!(prepared.validate_capacity(4096).is_err());
        std::fs::write(dir.path().join("image.bin"), []).unwrap();
        assert!(plan(&[0]).prepare(dir.path()).is_err());
    }
}
