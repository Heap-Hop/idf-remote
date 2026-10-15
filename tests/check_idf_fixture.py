#!/usr/bin/env python3
"""Check real ESP-IDF manifest imports preserve offsets and image bytes."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--idfr", required=True, type=Path)
    parser.add_argument("--build-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    build = args.build_dir.resolve()
    manifest = json.loads((build / "flasher_args.json").read_text())
    images = {int(offset, 0): build / file for offset, file in manifest["flash_files"].items()}
    before = {offset: path.read_bytes() for offset, path in images.items()}
    for app_only in [False, True]:
        command = [str(args.idfr.resolve()), "plan", "--build-dir", str(build)]
        if app_only:
            command.append("--app-only")
        plan = json.loads(subprocess.check_output(command))
        wanted = {int(manifest["app"]["offset"], 0)} if app_only else {offset for offset, data in before.items() if data}
        assert {s["offset"] for s in plan["segments"]} == wanted
        for segment in plan["segments"]:
            assert Path(segment["file"]).read_bytes() == before[segment["offset"]]
    assert all(path.read_bytes() == before[offset] for offset, path in images.items())
    report = {"chip": manifest["extra_esptool_args"]["chip"], "full_plan": "passed", "app_only": "passed", "segments": [{"offset": offset, "size": len(data), "sha256": hashlib.sha256(data).hexdigest()} for offset, data in before.items()]}
    with args.output.open("x") as output:
        output.write(json.dumps(report, indent=2) + "\n")
    print(f"C manifest regression passed: {args.output}")


if __name__ == "__main__":
    main()
