#!/usr/bin/env python3
"""Build both real Rust fixtures and compare their images without opening hardware."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]


def build(kind, report, usb_jtag):
    project = ROOT / "tests" / f"firmware-rust-{kind}"
    command = ["cargo", "build", "--locked", "--release", "--message-format=json"]
    environment = os.environ.copy()
    if usb_jtag:
        command += ["--target-dir", str(ROOT / ".artifacts" / f"firmware-rust-{kind}-usb-jtag")]
        if kind == "nostd":
            command += ["--no-default-features", "--features", "console-usb-jtag"]
        else:
            environment["ESP_IDF_SDKCONFIG_DEFAULTS"] = "sdkconfig.defaults;sdkconfig.usb-jtag"
            environment["ESP_IDF_SDKCONFIG"] = str(ROOT / ".artifacts/firmware-rust-std-usb-jtag/sdkconfig")
    with (report / f"{kind}-build.jsonl").open("w") as output, (report / f"{kind}-build.log").open("w") as error:
        subprocess.run(command, cwd=project, env=environment, stdout=output, stderr=error, check=True)
    executable = None
    out_dir = None
    for line in (report / f"{kind}-build.jsonl").read_text().splitlines():
        event = json.loads(line)
        if event["reason"] == "compiler-artifact" and event.get("executable") and event["target"]["name"] == f"idfr-fixture-{kind}":
            executable = Path(event["executable"])
        if event["reason"] == "build-script-executed" and "#esp-idf-sys@" in event["package_id"]:
            out_dir = Path(event["out_dir"])
    if executable is None:
        raise RuntimeError(f"Missing {kind} executable in Cargo messages")
    if kind == "std" and out_dir is None:
        raise RuntimeError("Missing esp-idf-sys build output in Cargo messages")
    return executable, out_dir


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--idfr", required=True, type=Path)
    parser.add_argument("--espflash", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path, help="New report directory")
    parser.add_argument("--usb-jtag", action="store_true", help="Use native USB Serial/JTAG console instead of UART")
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    results = []
    for kind in ["nostd", "std"]:
        executable, out_dir = build(kind, args.output, args.usb_jtag)
        command = [sys.executable, str(ROOT / "tests/elf_reference.py"), "--idfr", str(args.idfr.resolve()), "--espflash", str(args.espflash.resolve()), "--elf", str(executable), "--output", str(args.output / kind)]
        if out_dir:
            command += ["--bootloader", str(out_dir / "build/bootloader/bootloader.bin"), "--partition-table", str(out_dir / "build/partition_table/partition-table.bin")]
        subprocess.run(command, check=True)
        baseline = json.loads((args.output / kind / "baseline/plan.json").read_text())
        expected_app = 0x20000 if kind == "std" else 0x10000
        assert baseline["segments"][-1]["offset"] == expected_app, f"{kind}: fixture partition configuration was not applied"
        results.append({"kind": kind, "console": "usb-jtag" if args.usb_jtag else "uart", "elf": str(executable), "esp_idf_sys_out_dir": str(out_dir) if out_dir else None, "report": str(args.output / kind / "report.json")})
    (args.output / "fixtures.json").write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
