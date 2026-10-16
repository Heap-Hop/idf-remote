#!/usr/bin/env python3
"""Prepare a desktop-only crates.io source tree without changing Android builds."""
import argparse
import copy
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]
ANDROID_TARGET = 'cfg(any(target_os = "android", target_os = "linux"))'


def desktop_manifest(text):
    original = tomllib.loads(text)
    expected = copy.deepcopy(original)
    del expected["features"]["android-usb"]
    del expected["dependencies"]["nusb"]
    dependencies = expected["target"][ANDROID_TARGET]["dependencies"]
    del dependencies["espflash-android"]
    if not dependencies:
        del expected["target"][ANDROID_TARGET]["dependencies"]
    if not expected["target"][ANDROID_TARGET]:
        del expected["target"][ANDROID_TARGET]
    if not expected["target"]:
        del expected["target"]
    # Shared Rust sources retain their Android cfgs, but the published crate
    # deliberately does not expose an unusable android-usb feature.
    if "lints" in expected:
        raise ValueError("update the release manifest transform to preserve existing lints")
    expected["lints"] = {"rust": {"unexpected_cfgs": {
        "level": "warn", "check-cfg": ['cfg(feature, values("android-usb"))']
    }}}
    for key in ["android-usb", "nusb", "espflash-android"]:
        text, count = re.subn(r"^" + re.escape(key) + r"\s*=.*\n", "", text, flags=re.M)
        if count != 1:
            raise ValueError(f"expected one single-line {key} entry")
    text = text.replace(f"[target.'{ANDROID_TARGET}'.dependencies]\n", "")
    text += '\n[lints.rust]\nunexpected_cfgs = { level = "warn", check-cfg = [\'cfg(feature, values("android-usb"))\'] }\n'
    if tomllib.loads(text) != expected:
        raise ValueError("release manifest transform changed unexpected fields; update the transform")
    return text


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path, help="New release directory")
    parser.add_argument("--offline", action="store_true", help="Use cached registry dependencies")
    args = parser.parse_args()
    output = args.output.resolve()
    if output.exists():
        parser.error("output directory must not exist")
    text = desktop_manifest((ROOT / "Cargo.toml").read_text())
    # Copy tracked working-tree files, so no local firmware output, device logs,
    # credentials or unrelated untracked files can enter the release tree.
    files = [name for name in git("ls-files", "-z").decode().split("\0")
             if name and not name.startswith(("android/", ".github/"))]
    source = output / "source"
    source.mkdir(parents=True)
    for name in files:
        destination = source / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / name, destination)
    (source / "Cargo.toml").write_text(text)
    # Reconcile the copied lockfile without upgrading pinned registry packages.
    command = ["cargo", "metadata", "--format-version", "1", "--manifest-path", str(source / "Cargo.toml")]
    if args.offline:
        command.append("--offline")
    metadata = json.loads(subprocess.check_output(command, cwd=source))
    if any((p.get("source") or "").startswith("git+") for p in metadata["packages"]):
        raise ValueError("release dependency graph still contains a Git dependency")
    before = tomllib.loads((ROOT / "Cargo.lock").read_text())
    after = tomllib.loads((source / "Cargo.lock").read_text())
    pinned = {(p["name"], p["version"], p.get("source")) for p in before["package"]}
    if any((p["name"], p["version"], p.get("source")) not in pinned for p in after["package"]):
        raise ValueError("release resolution changed pinned versions; update the repository lockfile first")
    manifest = tomllib.loads(text)
    report = {
        "package": manifest["package"]["name"],
        "version": manifest["package"]["version"],
        "source_commit": git("rev-parse", "HEAD").decode().strip(),
        "tracked_changes": bool(git("status", "--porcelain", "--untracked-files=no")),
        "excluded_features": ["android-usb"],
        "manifest_sha256": hashlib.sha256(text.encode()).hexdigest(),
        "lockfile_sha256": hashlib.sha256((source / "Cargo.lock").read_bytes()).hexdigest(),
    }
    (output / "release.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"Prepared {report['package']} {report['version']}: {source}")
    print("Android remains available from the repository; no package was uploaded.")


if __name__ == "__main__":
    main()
