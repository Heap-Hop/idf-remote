//! Client-side ESP-IDF build manifest adapter. No ESP-IDF executable is invoked.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;

use crate::plan::{FileSegment, FlashPlan, FlashSettings, parse_offset, read_json};

#[derive(Deserialize)]
struct Manifest {
    flash_files: BTreeMap<String, PathBuf>,
    flash_settings: BTreeMap<String, String>,
    write_flash_args: Vec<String>,
    extra_esptool_args: ExtraArgs,
    #[serde(flatten)]
    images: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtraArgs {
    chip: String,
    before: String,
    after: String,
    stub: bool,
}

#[derive(Deserialize)]
struct Image {
    offset: String,
    file: PathBuf,
    #[serde(default, deserialize_with = "deserialize_encrypted")]
    encrypted: bool,
}

#[derive(Debug)]
pub struct Import {
    pub plan: FlashPlan,
    pub warnings: Vec<ImportWarning>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ImportWarning {
    pub image: Option<String>,
    pub offset: u32,
    pub file: PathBuf,
}

fn encrypted_value(value: &Value) -> Result<bool> {
    match value {
        Value::Bool(value) => Ok(*value),
        Value::String(value) if value == "true" => Ok(true),
        Value::String(value) if value == "false" => Ok(false),
        _ => bail!("encrypted must be true/false or the corresponding string"),
    }
}

fn deserialize_encrypted<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<bool, D::Error> {
    encrypted_value(&Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
}

pub fn import(path: &Path, selected_images: &[String]) -> Result<Import> {
    let manifest: Manifest = read_json(path)?;
    let base = path.parent().unwrap_or(Path::new("."));
    let before = manifest.extra_esptool_args.before.replace('-', "_");
    let after = manifest.extra_esptool_args.after.replace('-', "_");
    ensure!(
        before == "default_reset" && after == "hard_reset" && manifest.extra_esptool_args.stub,
        "phase 1 requires default_reset, hard_reset and stub=true; custom reset/no-stub manifests are not supported"
    );
    for (name, image) in &manifest.images {
        if name.contains("encrypt")
            || image
                .get("encrypted")
                .map(encrypted_value)
                .transpose()?
                .unwrap_or(false)
        {
            bail!("encrypted flash entries are not supported: {name}");
        }
    }
    // Only accept semantics we implement; never silently discard esptool flags.
    let mut settings = BTreeMap::new();
    let mut args = manifest.write_flash_args.iter();
    while let Some(arg) = args.next() {
        let key = arg.replace('-', "_");
        let key = key.trim_start_matches('_');
        ensure!(
            ["flash_mode", "flash_freq", "flash_size"].contains(&key),
            "unsupported write_flash argument: {arg}"
        );
        let value = args
            .next()
            .with_context(|| format!("missing value for {arg}"))?;
        ensure!(
            settings.insert(key.to_owned(), value.clone()).is_none(),
            "duplicate argument: {arg}"
        );
    }
    ensure!(
        settings == manifest.flash_settings,
        "flash_settings and write_flash_args disagree"
    );
    ensure!(
        settings.len() == 3,
        "expected flash_mode, flash_freq and flash_size"
    );
    let mut warnings = Vec::new();
    let segments = if selected_images.is_empty() {
        manifest
            .flash_files
            .iter()
            .filter_map(|(offset, file)| {
                let result = (|| -> Result<Option<FileSegment>> {
                    let offset = parse_offset(offset)?;
                    let metadata = std::fs::metadata(base.join(file)).with_context(|| {
                        format!("inspect artifact {}", base.join(file).display())
                    })?;
                    if metadata.len() == 0 {
                        warnings.push(ImportWarning {
                            image: image_name(&manifest.images, offset, file),
                            offset,
                            file: file.clone(),
                        });
                        return Ok(None);
                    }
                    Ok(Some(FileSegment {
                        offset,
                        file: file.clone(),
                    }))
                })();
                match result {
                    Ok(Some(segment)) => Some(Ok(segment)),
                    Ok(None) => None,
                    Err(error) => Some(Err(error)),
                }
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        let available = manifest.images.keys().cloned().collect::<Vec<_>>();
        let mut seen = BTreeSet::new();
        let mut segments = Vec::with_capacity(selected_images.len());
        for name in selected_images {
            ensure!(seen.insert(name), "duplicate image selection: {name}");
            let value = manifest.images.get(name).with_context(|| {
                format!(
                    "unknown image '{name}'; available images: {}",
                    available.join(", ")
                )
            })?;
            let image: Image = serde_json::from_value(value.clone())
                .with_context(|| format!("invalid image entry '{name}'"))?;
            ensure!(!image.encrypted, "encrypted image is not supported: {name}");
            let offset = parse_offset(&image.offset)?;
            let matching = manifest
                .flash_files
                .iter()
                .any(|(key, file)| parse_offset(key).ok() == Some(offset) && file == &image.file);
            ensure!(matching, "{name} entry does not match flash_files");
            let artifact = base.join(&image.file);
            let metadata = std::fs::metadata(&artifact)
                .with_context(|| format!("inspect artifact {}", artifact.display()))?;
            ensure!(
                metadata.len() > 0,
                "selected image '{name}' is empty: {}",
                artifact.display()
            );
            segments.push(FileSegment {
                offset,
                file: image.file,
            });
        }
        segments
    };
    ensure!(
        !segments.is_empty(),
        "flasher_args.json contains no non-empty images to flash"
    );
    Ok(Import {
        plan: FlashPlan {
            version: 1,
            chip: manifest.extra_esptool_args.chip,
            flash_settings: FlashSettings {
                mode: settings.remove("flash_mode"),
                frequency: settings.remove("flash_freq"),
                size: settings.remove("flash_size"),
            },
            segments,
        },
        warnings,
    })
}

fn image_name(images: &BTreeMap<String, Value>, offset: u32, file: &Path) -> Option<String> {
    images.iter().find_map(|(name, value)| {
        let image: Image = serde_json::from_value(value.clone()).ok()?;
        (parse_offset(&image.offset).ok() == Some(offset) && image.file == file)
            .then(|| name.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> Value {
        serde_json::json!({
            "write_flash_args": ["--flash_mode", "dio", "--flash_size", "4MB", "--flash_freq", "80m"],
            "flash_settings": {"flash_mode": "dio", "flash_size": "4MB", "flash_freq": "80m"},
            "flash_files": {"0x0": "bootloader/bootloader.bin", "0x8000": "partition_table/partition-table.bin", "0x20000": "hello.bin"},
            "app": {"offset": "0x20000", "file": "hello.bin", "encrypted": "false"},
            "bootloader": {"offset": "0x0", "file": "bootloader/bootloader.bin", "encrypted": "false"},
            "extra_esptool_args": {"chip": "esp32s3", "before": "default_reset", "after": "hard_reset", "stub": true}
        })
    }
    fn run(value: &Value, selected_images: &[&str], empty: &[&str]) -> Result<Import> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("flasher_args.json");
        std::fs::write(&path, serde_json::to_vec(value)?)?;
        for file in [
            "bootloader/bootloader.bin",
            "partition_table/partition-table.bin",
            "hello.bin",
        ] {
            let path = dir.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(
                &path,
                if empty.contains(&file) {
                    &[][..]
                } else {
                    &[1][..]
                },
            )?;
        }
        import(
            &path,
            &selected_images
                .iter()
                .map(|name| (*name).into())
                .collect::<Vec<_>>(),
        )
    }
    #[test]
    fn imports_all_segments_and_uses_actual_app_offset() {
        assert_eq!(run(&manifest(), &[], &[]).unwrap().plan.segments.len(), 3);
        let app = run(&manifest(), &["app"], &[]).unwrap();
        assert_eq!(app.plan.segments.len(), 1);
        assert_eq!(app.plan.segments[0].offset, 0x20000);
    }

    #[test]
    fn filters_empty_idf_images_and_reports_them() {
        let imported = run(&manifest(), &[], &["bootloader/bootloader.bin"]).unwrap();
        assert_eq!(imported.plan.segments.len(), 2);
        assert_eq!(
            imported.warnings,
            [ImportWarning {
                image: Some("bootloader".into()),
                offset: 0,
                file: "bootloader/bootloader.bin".into(),
            }]
        );
    }

    #[test]
    fn rejects_an_empty_selected_image_or_an_all_empty_manifest() {
        let selected = run(&manifest(), &["app"], &["hello.bin"]).unwrap_err();
        assert!(
            selected
                .to_string()
                .contains("selected image 'app' is empty")
        );
        let all = run(
            &manifest(),
            &[],
            &[
                "bootloader/bootloader.bin",
                "partition_table/partition-table.bin",
                "hello.bin",
            ],
        )
        .unwrap_err();
        assert!(all.to_string().contains("no non-empty images"));
    }

    #[test]
    fn selects_multiple_named_images_and_lists_choices_for_unknown_names() {
        let selected = run(&manifest(), &["app", "bootloader"], &[]).unwrap();
        assert_eq!(
            selected
                .plan
                .segments
                .iter()
                .map(|segment| segment.offset)
                .collect::<Vec<_>>(),
            [0x20000, 0]
        );
        let error = run(&manifest(), &["missing"], &[]).unwrap_err().to_string();
        assert!(error.contains("unknown image 'missing'"), "{error}");
        assert!(error.contains("app, bootloader"), "{error}");
    }
    #[test]
    fn rejects_unimplemented_flash_semantics() {
        let mut m = manifest();
        m["write_flash_args"]
            .as_array_mut()
            .unwrap()
            .push("--encrypt".into());
        assert!(run(&m, &[], &[]).is_err());
        let mut m = manifest();
        m["app"]["encrypted"] = true.into();
        assert!(run(&m, &[], &[]).is_err());
        let mut m = manifest();
        m["extra_esptool_args"]["stub"] = false.into();
        assert!(run(&m, &[], &[]).is_err());
        let mut m = manifest();
        m["flash_settings"]["flash_size"] = "8MB".into();
        assert!(run(&m, &[], &[]).is_err());
        let mut m = manifest();
        m["app"]["offset"] = "0x30000".into();
        assert!(run(&m, &["app"], &[]).is_err());
        let mut m = manifest();
        m["bootloader"]["encrypted"] = "true".into();
        assert!(run(&m, &[], &[]).is_err());
        let mut m = manifest();
        m["bootloader"]["encrypted"] = "no".into();
        assert!(run(&m, &[], &[]).is_err());
    }
}
