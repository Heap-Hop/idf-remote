//! Client-side ELF conversion using the pinned espflash image format.
//! Only generated images are rewritten; imported plans and the daemon preserve bytes.
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use esp_idf_part::{Partition, PartitionTable, Type};
use espflash::{
    flasher::{FlashData, FlashFrequency, FlashMode, FlashSettings as ImageSettings, FlashSize},
    image_format::idf::IdfBootloaderFormat,
    target::{Chip, XtalFrequency},
};
use object::{Object, ObjectSection, ObjectSymbol};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    backend::{DeviceBackend, EspflashBackend},
    plan::{FileSegment, FlashPlan, FlashSettings, MAX_ARTIFACT_BYTES, PreparedPlan},
};

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImageOptions {
    pub bootloader: Option<PathBuf>,
    pub partition_table: Option<PathBuf>,
    pub partition_table_offset: Option<u32>,
    pub target_app_partition: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ConfigFlash {
    mode: Option<FlashMode>,
    size: Option<FlashSize>,
    frequency: Option<FlashFrequency>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    format: Option<String>,
    #[serde(alias = "idf")]
    idf_format_args: ImageOptions,
    flash: ConfigFlash,
    // Recognized legacy local transport settings never select a remote device.
    baudrate: Option<u32>,
    connection: Option<Connection>,
    usb_device: Option<Vec<UsbDevice>>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Connection {
    #[serde(rename = "serial")]
    _serial: Option<String>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UsbDevice {
    #[serde(rename = "vid")]
    _vid: String,
    #[serde(rename = "pid")]
    _pid: String,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub chip: Chip,
    pub image: ImageOptions,
    pub mode: Option<FlashMode>,
    pub frequency: Option<FlashFrequency>,
    pub size: Option<FlashSize>,
    pub xtal: Option<XtalFrequency>,
    pub app_only: bool,
}

/// Paths inside configuration files retain espflash 4.5.0's CWD-relative semantics.
fn config_path(cwd: &Path, global: Option<&Path>) -> Option<PathBuf> {
    [
        Some(cwd.join("espflash.toml")),
        cwd.parent().map(|p| p.join("espflash.toml")),
        global.map(|p| p.join("espflash.toml")),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.exists())
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "{} exceeds {} bytes",
        path.display(),
        limit
    );
    Ok(bytes)
}

fn load_config() -> Result<(Config, Option<PathBuf>)> {
    let global = directories::ProjectDirs::from("rs", "esp", "espflash");
    let Some(path) = config_path(
        &std::env::current_dir()?,
        global.as_ref().map(|d| d.config_dir()),
    ) else {
        return Ok((Config::default(), None));
    };
    let bytes = read_bounded(&path, 1024 * 1024)?;
    let config: Config = toml::from_str(std::str::from_utf8(&bytes)?)
        .with_context(|| format!("parse {}", path.display()))?;
    ensure!(
        config.format.as_deref().is_none_or(|v| v == "esp-idf"),
        "unsupported espflash image format"
    );
    if config.baudrate.is_some() || config.connection.is_some() || config.usb_device.is_some() {
        eprintln!(
            "warning: espflash local transport settings are not used; select the remote device and baud rate with idfr options"
        );
    }
    Ok((config, Some(path)))
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ImageSource {
    CommandLine {
        path: PathBuf,
    },
    Config {
        config_file: PathBuf,
        path: PathBuf,
    },
    BuiltIn {
        espflash_version: &'static str,
        chip: String,
    },
}

impl ImageSource {
    fn description(&self) -> String {
        match self {
            Self::CommandLine { path } => format!("command line: {}", path.display()),
            Self::Config { config_file, path } => format!(
                "config: {}; image: {}",
                config_file.display(),
                path.display()
            ),
            Self::BuiltIn {
                espflash_version,
                chip,
            } => format!("built-in (espflash {espflash_version}, {chip})"),
        }
    }
}

#[derive(Serialize)]
struct ImageSources {
    bootloader: ImageSource,
    partition_table: ImageSource,
}

#[derive(Serialize)]
struct AppPartition {
    name: String,
    offset: u32,
    size: u32,
}

// Resolve the file and its provenance together. Config image paths remain
// relative to CWD; reporting uses the same absolute path that conversion reads.
fn resolve_image(
    cli: Option<&Path>,
    configured: Option<&Path>,
    config_file: Option<&Path>,
    chip: Chip,
) -> Result<(Option<PathBuf>, ImageSource)> {
    let Some(path) = cli.or(configured) else {
        return Ok((
            None,
            ImageSource::BuiltIn {
                espflash_version: "4.5.0",
                chip: chip.to_string(),
            },
        ));
    };
    let path = std::env::current_dir()?.join(path);
    let source = if cli.is_some() {
        ImageSource::CommandLine { path: path.clone() }
    } else {
        ImageSource::Config {
            config_file: config_file
                .context("missing source config path")?
                .to_owned(),
            path: path.clone(),
        }
    };
    Ok((Some(path), source))
}

fn validate_bootloader(bytes: &[u8], chip: Chip) -> Result<()> {
    ensure!(
        bytes.len() >= 24 && bytes[0] == 0xe9,
        "invalid or truncated bootloader header"
    );
    ensure!(
        u16::from_le_bytes([bytes[12], bytes[13]]) == chip.id(),
        "bootloader chip does not match --chip"
    );
    let mut end = 24usize;
    for _ in 0..bytes[1] {
        let header = bytes
            .get(end..end + 8)
            .context("truncated bootloader segment header")?;
        let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        end = end
            .checked_add(8)
            .and_then(|v| v.checked_add(size))
            .context("bootloader size overflow")?;
        ensure!(end <= bytes.len(), "truncated bootloader segment");
    }
    let hash_end = (end + 1).next_multiple_of(16) + 32;
    ensure!(
        hash_end <= bytes.len(),
        "truncated bootloader checksum/digest"
    );
    Ok(())
}

fn validate_elf(bytes: &[u8], chip: Chip) -> Result<()> {
    ensure!(
        bytes.get(..7) == Some(b"\x7fELF\x01\x01\x01"),
        "expected a little-endian ELF32 executable"
    );
    let elf = object::File::parse(bytes).context("parse ELF")?;
    let metadata = espflash::image_format::Metadata::from_bytes(Some(bytes));
    if let Some(name) = metadata.chip_name() {
        ensure!(
            name == chip.to_string(),
            "ELF chip metadata does not match --chip"
        );
    }
    ensure!(
        elf.kind() == object::ObjectKind::Executable,
        "ELF must be an executable"
    );
    let architecture = match chip {
        Chip::Esp32 | Chip::Esp32s2 | Chip::Esp32s3 => object::Architecture::Xtensa,
        _ => object::Architecture::Riscv32,
    };
    ensure!(
        elf.architecture() == architecture,
        "ELF architecture does not match --chip"
    );
    let mut loaded = false;
    let mut total = 0u64;
    for section in elf.sections() {
        // Validate file-backed sections before espflash's iterator can skip malformed data.
        if section.file_range().is_some() {
            section.data().context("invalid ELF section data")?;
        }
        let alloc =
            matches!(section.flags(), object::SectionFlags::Elf { sh_flags } if sh_flags & 2 != 0);
        if alloc && section.size() > 0 {
            ensure!(
                section
                    .address()
                    .checked_add(section.size())
                    .is_some_and(|v| v <= u32::MAX as u64),
                "ELF section exceeds address space"
            );
            total = total
                .checked_add(section.size())
                .context("ELF size overflow")?;
            ensure!(
                total <= MAX_ARTIFACT_BYTES as u64,
                "ELF sections exceed 128 MiB"
            );
            loaded |= section.file_range().is_some();
        }
        if section.name().ok() == Some(".flash.appdesc") {
            ensure!(
                alloc
                    && chip.addr_is_flash(section.address() as u32)
                    && section.data()?.len() >= 256,
                "invalid ELF app descriptor"
            );
        }
    }
    ensure!(loaded, "ELF has no loadable data");
    // Match espflash's CLI check, including stripped section-based descriptors.
    ensure!(
        elf.symbols()
            .any(|symbol| symbol.name() == Ok("esp_app_desc"))
            || elf.section_by_name(".flash.appdesc").is_some()
            || elf.section_by_name(".rodata_desc").is_some(),
        "ELF app descriptor missing; esp-hal applications need esp_bootloader_esp_idf::esp_app_desc!()"
    );
    Ok(())
}

fn select_app_partition<'a>(
    table: &'a PartitionTable,
    target: Option<&str>,
) -> Result<&'a Partition> {
    let partition = if let Some(name) = target {
        table
            .find(name)
            .with_context(|| format!("app partition {name:?} not found"))?
    } else {
        table
            .find("factory")
            .filter(|p| p.ty() == Type::App)
            .or_else(|| table.find_by_type(Type::App))
            .context("partition table has no app partition")?
    };
    ensure!(
        partition.ty() == Type::App,
        "target partition {:?} is not an app partition",
        partition.name()
    );
    Ok(partition)
}

/// Owns converted bytes. Conversion and validation complete before export or HTTP.
pub struct Converted {
    pub plan: FlashPlan,
    pub prepared: PreparedPlan,
    bytes: Vec<Vec<u8>>,
    sources: ImageSources,
    app_partition: AppPartition,
    app_only: bool,
}

pub fn convert(path: &Path, options: &Options) -> Result<Converted> {
    let (config, config_file) = load_config()?;
    let (bootloader, bootloader_source) = resolve_image(
        options.image.bootloader.as_deref(),
        config.idf_format_args.bootloader.as_deref(),
        config_file.as_deref(),
        options.chip,
    )?;
    let (partition_table, partition_source) = resolve_image(
        options.image.partition_table.as_deref(),
        config.idf_format_args.partition_table.as_deref(),
        config_file.as_deref(),
        options.chip,
    )?;
    let sources = ImageSources {
        bootloader: bootloader_source,
        partition_table: partition_source,
    };
    let image = ImageOptions {
        bootloader,
        partition_table,
        partition_table_offset: options
            .image
            .partition_table_offset
            .or(config.idf_format_args.partition_table_offset),
        target_app_partition: options
            .image
            .target_app_partition
            .clone()
            .or(config.idf_format_args.target_app_partition),
    };
    if image.bootloader.is_none() {
        ensure!(
            options.chip != Chip::Esp32p4,
            "ESP32-P4 requires an explicit bootloader; revision selection is unavailable in the espflash library API"
        );
        ensure!(
            !matches!(options.chip, Chip::Esp32 | Chip::Esp32c2) || options.xtal.is_some(),
            "this chip requires --xtal-freq when using the built-in bootloader"
        );
    }
    let xtal = options.xtal.unwrap_or(if options.chip == Chip::Esp32h2 {
        XtalFrequency::_32Mhz
    } else {
        XtalFrequency::_40Mhz
    });
    let settings = ImageSettings::new(
        options.mode.or(config.flash.mode),
        Some(options.size.or(config.flash.size).unwrap_or_default()),
        options.frequency.or(config.flash.frequency),
    );
    let flash_data = FlashData::new(settings, 0, None, options.chip, xtal);
    let elf = read_bounded(path, MAX_ARTIFACT_BYTES)?;
    validate_elf(&elf, options.chip)?;
    // Snapshot auxiliary files so validation and the upstream parser use identical bytes.
    let scratch = tempfile::tempdir()?;
    let bootloader = if let Some(path) = &image.bootloader {
        let bytes = read_bounded(path, MAX_ARTIFACT_BYTES)?;
        validate_bootloader(&bytes, options.chip)?;
        let snapshot = scratch.path().join("bootloader.bin");
        fs::write(&snapshot, bytes)?;
        Some(snapshot)
    } else {
        None
    };
    let mut target_app_partition = image.target_app_partition.clone();
    let partition = if let Some(path) = &image.partition_table {
        let bytes = read_bounded(path, 1024 * 1024)?;
        ensure!(bytes.len() >= 2, "truncated partition table");
        let table = PartitionTable::try_from(bytes.clone()).context("parse partition table")?;
        target_app_partition =
            Some(select_app_partition(&table, target_app_partition.as_deref())?.name());
        let snapshot = scratch.path().join("partitions.bin");
        fs::write(&snapshot, bytes)?;
        Some(snapshot)
    } else {
        None
    };
    // espflash has additional unchecked assumptions for malformed ELF metadata.
    let format = std::panic::catch_unwind(|| {
        IdfBootloaderFormat::new(
            &elf,
            &flash_data,
            partition.as_deref(),
            bootloader.as_deref(),
            image.partition_table_offset,
            target_app_partition.as_deref(),
        )
    })
    .map_err(|_| anyhow::anyhow!("espflash rejected malformed image data"))?
    .context("convert ELF image")?;
    // The built-in table is private to espflash. Validate its selection too,
    // before exporting any bytes or allowing an upload (including app-only).
    let table = format.partition_table();
    let selected = select_app_partition(&table, target_app_partition.as_deref())?;
    let app_partition = AppPartition {
        name: selected.name(),
        offset: selected.offset(),
        size: selected.size(),
    };
    let mut segments: Vec<_> = format
        .flash_segments()
        .map(|s| (s.addr, s.data.into_owned()))
        .collect();
    let header = &segments[0].1;
    let mode = match header[2] {
        0 => "qio",
        1 => "qout",
        2 => "dio",
        3 => "dout",
        _ => bail!("unsupported bootloader flash mode"),
    };
    let frequency = settings
        .freq
        .unwrap_or(options.chip.default_flash_frequency());
    let flash_settings = FlashSettings {
        mode: Some(mode.into()),
        frequency: Some(serde_json::to_value(frequency)?.as_str().unwrap().into()),
        size: Some(
            serde_json::to_value(settings.size.unwrap())?
                .as_str()
                .unwrap()
                .into(),
        ),
    };
    if options.app_only {
        segments.drain(..2);
    }
    let mut plan = FlashPlan {
        version: 1,
        chip: options.chip.to_string(),
        flash_settings,
        segments: Vec::new(),
    };
    let mut bytes = Vec::new();
    for (offset, data) in segments {
        let file = PathBuf::from(format!("{offset:08x}.bin"));
        fs::write(scratch.path().join(&file), &data)?;
        plan.segments.push(FileSegment { offset, file });
        bytes.push(data);
    }
    let prepared = plan.prepare(scratch.path())?;
    EspflashBackend::default().validate(&prepared)?;
    Ok(Converted {
        plan,
        prepared,
        bytes,
        sources,
        app_partition,
        app_only: options.app_only,
    })
}

#[derive(Serialize)]
struct Artifact<'a> {
    offset: u32,
    file: &'a Path,
    size: usize,
    sha256: String,
}

impl Converted {
    /// Requires a new directory. On failure, removes only the directory we created.
    pub fn export(&self, directory: &Path) -> Result<()> {
        fs::create_dir(directory)
            .with_context(|| format!("create new output directory {}", directory.display()))?;
        let result = (|| -> Result<()> {
            let mut artifacts = Vec::new();
            for (segment, bytes) in self.plan.segments.iter().zip(&self.bytes) {
                fs::write(directory.join(&segment.file), bytes)?;
                artifacts.push(Artifact {
                    offset: segment.offset,
                    file: &segment.file,
                    size: bytes.len(),
                    sha256: digest(bytes),
                });
            }
            fs::write(
                directory.join("artifacts.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "segments": artifacts,
                    "sources": self.sources,
                    "app_partition": self.app_partition,
                    "app_only": self.app_only,
                }))?,
            )?;
            fs::write(
                directory.join("plan.json"),
                serde_json::to_vec_pretty(&self.plan)?,
            )?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(directory);
        }
        result
    }

    pub fn print_summary(&self) {
        eprintln!("bootloader       {}", self.sources.bootloader.description());
        eprintln!(
            "partition table  {}",
            self.sources.partition_table.description()
        );
        eprintln!(
            "app partition    {} @ {:#x}, size {:#x}",
            self.app_partition.name, self.app_partition.offset, self.app_partition.size
        );
        let defaults: Vec<_> = [
            ("bootloader", &self.sources.bootloader),
            ("partition table", &self.sources.partition_table),
        ]
        .into_iter()
        .filter_map(|(name, source)| matches!(source, ImageSource::BuiltIn { .. }).then_some(name))
        .collect();
        if !defaults.is_empty() {
            eprintln!("note: using built-in {} defaults", defaults.join(" and "));
        }
        if self.app_only {
            eprintln!(
                "note: app-only; bootloader and partition table are used for conversion, not written"
            );
        }
        for (segment, bytes) in self.plan.segments.iter().zip(&self.bytes) {
            eprintln!(
                "{:#010x}  {} bytes  sha256 {}",
                segment.offset,
                bytes.len(),
                digest(bytes)
            );
        }
    }
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searches_only_cwd_parent_then_global() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("workspace/member");
        let global = root.path().join("global");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir(&global).unwrap();
        fs::write(root.path().join("espflash.toml"), "").unwrap();
        assert_eq!(config_path(&cwd, Some(&global)), None);
        fs::write(global.join("espflash.toml"), "").unwrap();
        assert_eq!(
            config_path(&cwd, Some(&global)),
            Some(global.join("espflash.toml"))
        );
        let parent = cwd.parent().unwrap().join("espflash.toml");
        fs::write(&parent, "").unwrap();
        assert_eq!(config_path(&cwd, Some(&global)), Some(parent));
        let local = cwd.join("espflash.toml");
        fs::write(&local, "").unwrap();
        assert_eq!(config_path(&cwd, Some(&global)), Some(local));
    }

    #[test]
    fn config_rejects_unknown_nested_fields_and_unsupported_values() {
        for input in [
            "unknown = 1",
            "[idf]\nunknown = 1",
            "[flash]\nmode = 'keep'",
            "[flash]\nsize = 'detect'",
            "[connection]\nport = 'local'",
            "[idf]\nbootloader = 3",
            "[flash",
        ] {
            assert!(toml::from_str::<Config>(input).is_err(), "{input}");
        }
        let parsed: Config = toml::from_str("[idf]\npartition_table_offset = 0x9000\nbootloader = 'boot.bin'\n[flash]\nfrequency = '80MHz'").unwrap();
        assert_eq!(parsed.idf_format_args.partition_table_offset, Some(0x9000));
        assert_eq!(parsed.idf_format_args.bootloader, Some("boot.bin".into()));
    }

    #[test]
    fn bootloader_bounds_cover_segments_checksum_and_digest() {
        for bytes in [vec![], vec![0xe9], vec![0xe9; 24]] {
            assert!(validate_bootloader(&bytes, Chip::Esp32s3).is_err());
        }
        let mut boot = vec![0; 80];
        boot[0] = 0xe9;
        boot[1] = 1;
        boot[12] = 9;
        boot[28..32].copy_from_slice(&4u32.to_le_bytes());
        assert!(validate_bootloader(&boot, Chip::Esp32s3).is_ok());
        assert!(validate_bootloader(&boot[..79], Chip::Esp32s3).is_err());
        boot[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(validate_bootloader(&boot, Chip::Esp32s3).is_err());
    }
}
