# ELF inputs

`idfr plan` and `idfr flash` accept `--elf FILE --chip CHIP`. This source is
exclusive with `--plan`, an explicit `--build-dir`, and `--image`. ELF-specific
options require `--elf`. Conversion runs entirely on the client and does not
connect to a board. The daemon and Android USB host receive ordinary plan bytes.

## Images and configuration

Supported options are `--bootloader`, `--partition-table` (CSV or binary),
`--partition-table-offset`, `--target-app-partition`, `--flash-mode`,
`--flash-freq`, `--flash-size`, and `--xtal-freq`. Offsets accept decimal or `0x`
hexadecimal. Frequencies accept forms such as `80mhz`, `80MHz`, and `80m`;
flash sizes accept forms such as `4MB`. `keep` and `detect` are not ELF options.
`--app-only` emits only the selected application segment.

Image values resolve independently: command line, then `espflash.toml`, then
espflash's built-in defaults. Flash size defaults to 4MB. For esp-idf-sys, pass
the matching build's `build/bootloader/bootloader.bin` and
`build/partition_table/partition-table.bin` beneath its build-script `out_dir`.
idfr does not invoke Cargo or automatically discover these files.

Configuration is searched in the current directory, its immediate parent, then
the platform's user espflash configuration directory, matching espflash 4.5.0.
Relative image paths are resolved against the current working directory, even
when configuration was found elsewhere. Only the first existing config is read.

```toml
format = "esp-idf"

[idf_format_args] # alias: [idf]
bootloader = "bootloader.bin"
partition_table = "partitions.csv"
partition_table_offset = 0x8000
target_app_partition = "factory"

[flash]
mode = "dio"
frequency = "80MHz"
size = "4MB"
```

Unknown keys, malformed TOML, and unsupported values are errors. Legacy
`baudrate`, `[connection].serial`, and `[[usb_device]]` VID/PID fields are
recognized with a warning: they do not select remote hardware or change idfr's
baud rates. Use idfr's device and baud options. No config files are migrated.

Unlike the standalone espflash 4.5.0 CLI, idfr also honors config fallback for
`partition_table_offset` and `target_app_partition`. Byte comparisons use the
same effective values, passing those options explicitly to the reference CLI.

## Chip variants

ESP32 and ESP32-C2 require explicit `--xtal-freq 26mhz` or `40mhz` when choosing
a built-in bootloader. An explicit bootloader, including one from config,
resolves that choice. ESP32-P4 requires an explicit bootloader because the pinned
library's chip-revision selection API is private. idfr does not substitute the
minimum supported revision for the actual device revision.

The ELF's declared minimum chip revision remains a floor enforced by espflash.
ELF architecture and available chip metadata are checked against `--chip`;
physical device identity is still checked by the daemon during flashing.
MMU-page overrides, explicit minimum-chip-revision overrides, encrypted images,
and partition erasure options are not supported.

## Plans and Cargo runners

```sh
idfr plan --chip esp32s3 --elf APP_ELF --output NEW_PLAN_DIR
idfr flash --plan NEW_PLAN_DIR/plan.json --monitor
```

The output directory must not exist. The exported `plan.json` uses relative
paths and can move together with its images. `artifacts.json` records each raw
image's offset, size, and SHA-256; the same summary is printed to stderr. The
existing plan JSON stays compatible. The normal four-byte upload padding is
applied later, without changing the exported image files.

`flash --elf` can export a bundle with `--output`, or keep files in a temporary
directory until the operation finishes. Both modes validate images before HTTP.
The backend preserves supplied bytes; ELF header and digest rewriting happens
only during client conversion, following the pinned espflash 4.5.0 rules.

For esp-hal projects, a Cargo target configuration can use:

```toml
[target.xtensa-esp32s3-none-elf]
runner = "idfr flash --chip esp32s3 --monitor --elf"
```

Cargo appends the executable path, so `--elf` must be last in the runner string.
For std projects, add explicit `--bootloader` and `--partition-table` arguments
before `--elf`, using paths for the current build. Application arguments after
the executable are not supported. Monitoring remains raw serial monitoring.

See [Testing](../TESTING.md) for synthetic regressions and real source fixtures.
