#!/usr/bin/env python3
"""Compare generated ELF bundles with espflash 4.5.0, without hardware access."""
import argparse
import hashlib
import json
from pathlib import Path
import struct
import subprocess


def run(argv, cwd):
    result = subprocess.run([str(a) for a in argv], cwd=cwd, capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(f"{argv}\n{result.stdout}\n{result.stderr}")
    return result.stdout


def boot_size(data):
    assert data[0] == 0xE9
    end = 24
    for _ in range(data[1]):
        end += 8 + struct.unpack_from("<I", data, end + 4)[0]
    return (end + 1 + 15) // 16 * 16 + 32


def app_offset(partition):
    apps = []
    for pos in range(0, len(partition), 32):
        magic, kind, subtype, offset, size = struct.unpack_from("<HBBII", partition, pos)
        if magic != 0x50AA:
            break
        if kind == 0:
            apps.append((subtype, offset))
    return next((offset for subtype, offset in apps if subtype == 0), apps[0][1])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--idfr", required=True, type=Path)
    parser.add_argument("--espflash", required=True, type=Path)
    parser.add_argument("--elf", required=True, type=Path)
    parser.add_argument("--chip", default="esp32s3", choices=["esp32s3", "esp32c3", "esp32c6"])
    parser.add_argument("--bootloader", type=Path)
    parser.add_argument("--partition-table", type=Path)
    parser.add_argument("--output", required=True, type=Path, help="New report directory")
    parser.add_argument("--synthetic", action="store_true", help="Label the report as synthetic ELF data")
    args = parser.parse_args()
    idfr, espflash, elf = args.idfr.resolve(), args.espflash.resolve(), args.elf.resolve()
    supplied = []
    for flag, path in [("--bootloader", args.bootloader), ("--partition-table", args.partition_table)]:
        if path:
            supplied += [flag, str(path.resolve())]
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    (root / "espflash.toml").write_text("")
    version = run([espflash, "--version"], root).strip()
    assert version == "espflash 4.5.0", version
    common = ["--chip", args.chip, "--flash-size", "4mb"]
    reference = [espflash, "save-image", "--skip-update-check", *common]
    # Reference-derived support images: never use idfr output as the oracle.
    run([*reference, *supplied, "--partition-table-offset", "0x8000", "--merge", elf, "seed.bin"], root)
    seed = (root / "seed.bin").read_bytes()
    (root / "boot.bin").write_bytes(seed[:boot_size(seed)])
    (root / "parts.bin").write_bytes(seed[0x8000:0x8C00])
    (root / "parts.csv").write_text(
        "nvs,data,nvs,0x9000,0x6000\nphy_init,data,phy,0xf000,0x1000\n"
        "factory,app,factory,0x20000,0x100000\nother,app,ota_0,0x120000,0x100000\n"
    )
    cases = [
        ("baseline", supplied + ["--partition-table-offset", "0x8000"], [], app_offset(seed[0x8000:0x8C00]), False),
        ("binary", ["--bootloader", "boot.bin", "--partition-table", "parts.bin", "--partition-table-offset", "0x8000"], [], app_offset(seed[0x8000:0x8C00]), False),
        ("csv", ["--bootloader", "boot.bin", "--partition-table", "parts.csv", "--partition-table-offset", "0x8000"], [], 0x20000, False),
        ("selected", ["--partition-table", "parts.csv", "--partition-table-offset", "0x9000", "--target-app-partition", "other", "--flash-mode", "dio", "--flash-freq", "80mhz"], [], 0x120000, False),
        ("app-only", ["--partition-table", "parts.csv", "--target-app-partition", "other"], [], 0x120000, True),
        # Upstream only falls back to config for bootloader and partition_table.
        # Pass other effective values explicitly to the reference CLI.
        ("config", ["--partition-table-offset", "0x9000", "--target-app-partition", "other", "--flash-freq", "80mhz"], ["--flash-freq", "80mhz"], 0x120000, False),
    ]
    report = {"reference": version, "chip": args.chip, "synthetic": args.synthetic, "elf_sha256": hashlib.sha256(elf.read_bytes()).hexdigest(), "cases": []}
    for name, reference_args, client_args, expected_app, only in cases:
        if name == "config":
            (root / "espflash.toml").write_text(
                "[idf]\nbootloader = 'boot.bin'\npartition_table = 'parts.csv'\n"
                "partition_table_offset = 0x9000\ntarget_app_partition = 'other'\n"
                "[flash]\nmode = 'dio'\nfrequency = '40MHz'\n"
            )
        else:
            client_args = reference_args
        merged = root / f"{name}-reference.bin"
        app = root / f"{name}-app.bin"
        run([*reference, *reference_args, "--merge", elf, merged], root)
        run([*reference, *reference_args, elf, app], root)
        destination = root / name
        command = [idfr, "plan", "--elf", elf, *common, *client_args, "--output", destination]
        if only:
            command.append("--app-only")
        run(command, root)
        plan = json.loads((destination / "plan.json").read_text())
        segments = plan["segments"]
        expected_table = 0x9000 if name in ("selected", "config") else 0x8000
        assert [s["offset"] for s in segments] == ([expected_app] if only else [0, expected_table, expected_app]), name
        image = bytearray(b"\xff" * (4 * 1024 * 1024))
        reference_image = merged.read_bytes()
        for segment in segments:
            data = (destination / segment["file"]).read_bytes()
            offset = segment["offset"]
            assert data == reference_image[offset:offset + len(data)], (name, offset)
            image[offset:offset + len(data)] = data
        app_data = (destination / segments[-1]["file"]).read_bytes()
        assert app_data == app.read_bytes(), name
        if not only:
            assert image == reference_image, f"{name}: full layout mismatch"
            assert len((destination / segments[0]["file"]).read_bytes()) == boot_size(reference_image)
            assert len((destination / segments[1]["file"]).read_bytes()) == 0xC00
        report["cases"].append({"name": name, "segments": json.loads((destination / "artifacts.json").read_text())["segments"], "passed": True})
    (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"{len(cases)} differential cases passed: {root / 'report.json'}")


if __name__ == "__main__":
    main()
