"""Opt-in desktop detach/reattach test. This script never programs flash.

Start `idfr serve` for the intended device first. The script starts passive
monitoring, asks for a physical detach and reattach, then verifies that the daemon
reopens the same logical device and captures an application recovery marker without
an explicit reset or monitor operation.
"""

import argparse
import json
import time
import urllib.error
import urllib.request
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", default="http://127.0.0.1:38473")
    parser.add_argument("--usb-serial", required=True)
    parser.add_argument("--marker", required=True)
    parser.add_argument("--action-timeout", type=float, default=120)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--token-file", type=Path)
    args = parser.parse_args()
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    headers = {"Content-Type": "application/json"}
    if args.token_file:
        headers["Authorization"] = "Bearer " + args.token_file.read_text().strip()
    evidence = []
    request_number = 0

    def call(path, body=None, expected=200):
        nonlocal request_number
        data = None if body is None else json.dumps(body).encode()
        request_headers = headers.copy()
        if body is not None:
            request_number += 1
            request_headers["Idempotency-Key"] = (
                f"reconnect-smoke-{time.time_ns()}-{request_number}"
            )
        request = urllib.request.Request(args.url + path, data=data, headers=request_headers)
        try:
            with opener.open(request, timeout=10) as response:
                status, value = response.status, json.load(response)
        except urllib.error.HTTPError as error:
            status, value = error.code, json.load(error)
        assert status == expected, (path, status, value)
        return value

    def configured_device():
        response = call("/v1/devices")
        matches = [
            device
            for device in response["devices"]
            if device["transport"]["metadata"].get("serial_number") == args.usb_serial
        ]
        assert len(matches) == 1, response
        device = matches[0]
        return device, response["cursors"][device["id"]]

    def done(operation):
        deadline = time.monotonic() + 30
        while operation["status"] == "running":
            assert time.monotonic() < deadline, operation
            time.sleep(0.1)
            operation = call("/v1/operations/" + operation["id"])
        assert operation["status"] == "succeeded", operation
        return operation

    def wait_availability(expected):
        deadline = time.monotonic() + args.action_timeout
        while time.monotonic() < deadline:
            device, cursor = configured_device()
            if device["status"]["availability"] == expected:
                return device, cursor
            time.sleep(0.2)
        raise AssertionError(f"device did not become {expected}")

    def record(name, value):
        evidence.append({"test": name, "evidence": value})
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(
            json.dumps({"complete": False, "tests": evidence}, indent=2) + "\n"
        )
        print(name + ": PASS", flush=True)

    initial, _ = configured_device()
    assert initial["status"]["availability"] == "available", initial
    assert initial["transport"]["metadata"]["serial_number"] == args.usb_serial, initial
    device_id = initial["id"]
    request = {"device_id": device_id, "monitor_baud": 115200}
    done(call("/v1/monitor", request, 202))
    monitoring, _ = configured_device()
    assert monitoring["status"]["activity"] == "monitoring", monitoring
    record("initial_identity_and_monitor", monitoring)

    print("ACTION: unplug the selected ESP32-S3 USB cable now", flush=True)
    disconnected, disconnect_cursor = wait_availability("disconnected")
    assert disconnected["id"] == device_id, disconnected
    assert disconnected["status"]["activity"] == "reconnecting", disconnected
    record("detach_reports_disconnected", disconnected)

    print("ACTION: reconnect the same ESP32-S3 USB cable now", flush=True)
    reconnected, _ = wait_availability("available")
    assert reconnected["id"] == device_id, reconnected
    assert reconnected["transport"]["metadata"]["serial_number"] == args.usb_serial, reconnected
    assert reconnected["status"]["activity"] == "monitoring", reconnected
    record("reattach_preserves_logical_id", reconnected)

    marker = call(
        "/v1/wait",
        {
            "device_id": device_id,
            "cursor": disconnect_cursor,
            "pattern": args.marker,
            "timeout_ms": 10000,
        },
    )
    record("automatic_monitor_output_after_reattach", marker)

    args.report.write_text(
        json.dumps({"complete": True, "tests": evidence}, indent=2) + "\n"
    )
    print(f"{len(evidence)} checks passed; report: {args.report}", flush=True)


if __name__ == "__main__":
    main()
