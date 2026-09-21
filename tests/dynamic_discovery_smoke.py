"""Opt-in USB hot-add test. This script never programs flash."""

import argparse
import json
import time
import urllib.error
import urllib.request
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", default="http://127.0.0.1:38473")
    parser.add_argument("--attach-usb-serial", required=True)
    parser.add_argument("--attach-marker", required=True)
    parser.add_argument("--unaffected-usb-serial", required=True)
    parser.add_argument("--unaffected-marker", required=True)
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
                f"dynamic-discovery-smoke-{time.time_ns()}-{request_number}"
            )
        request = urllib.request.Request(
            args.url + path, data=data, headers=request_headers
        )
        try:
            with opener.open(request, timeout=10) as response:
                status, value = response.status, json.load(response)
        except urllib.error.HTTPError as error:
            status, value = error.code, json.load(error)
        assert status == expected, (path, status, value)
        return value

    def done(operation):
        deadline = time.monotonic() + 30
        while operation["status"] == "running":
            assert time.monotonic() < deadline, operation
            time.sleep(0.1)
            operation = call("/v1/operations/" + operation["id"])
        assert operation["status"] == "succeeded", operation
        return operation

    def by_serial(response, serial):
        matches = [
            device
            for device in response["devices"]
            if device["transport"]["metadata"].get("serial_number") == serial
        ]
        assert len(matches) <= 1, response
        return matches[0] if matches else None

    def record(name, value):
        evidence.append({"test": name, "evidence": value})
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(
            json.dumps({"complete": False, "tests": evidence}, indent=2) + "\n"
        )
        print(name + ": PASS", flush=True)

    initial = call("/v1/devices")
    assert by_serial(initial, args.attach_usb_serial) is None, (
        "attach target must be absent when the daemon starts",
        initial,
    )
    unaffected = by_serial(initial, args.unaffected_usb_serial)
    assert unaffected is not None, initial
    assert unaffected["status"]["availability"] == "available", unaffected
    done(
        call(
            "/v1/monitor",
            {"device_id": unaffected["id"], "monitor_baud": 115200},
            202,
        )
    )
    baseline = call("/v1/devices")
    unaffected = by_serial(baseline, args.unaffected_usb_serial)
    unaffected_cursor = baseline["cursors"][unaffected["id"]]
    record("daemon_started_without_target", {"unaffected": unaffected})

    print(
        "ACTION: attach device with USB serial " + args.attach_usb_serial,
        flush=True,
    )
    deadline = time.monotonic() + args.action_timeout
    attached = None
    response = None
    while attached is None:
        assert time.monotonic() < deadline, "new USB device was not discovered"
        response = call("/v1/devices")
        attached = by_serial(response, args.attach_usb_serial)
        if attached is None:
            time.sleep(0.2)
    assert attached["status"] == {
        "availability": "available",
        "activity": "idle",
    }, attached
    assert attached["transport"]["metadata"].get("identity_strength") == "strong"
    assert attached["id"] != unaffected["id"]
    record("hot_add_created_runtime", {"attached": attached})

    attached_cursor = response["cursors"][attached["id"]]
    done(
        call(
            "/v1/monitor",
            {"device_id": attached["id"], "monitor_baud": 115200},
            202,
        )
    )
    attached_log = call(
        "/v1/wait",
        {
            "device_id": attached["id"],
            "cursor": attached_cursor,
            "pattern": args.attach_marker,
            "timeout_ms": 5000,
        },
    )
    unaffected_log = call(
        "/v1/wait",
        {
            "device_id": unaffected["id"],
            "cursor": unaffected_cursor,
            "pattern": args.unaffected_marker,
            "timeout_ms": 5000,
        },
    )
    final = call("/v1/devices")
    attached = by_serial(final, args.attach_usb_serial)
    unaffected = by_serial(final, args.unaffected_usb_serial)
    assert attached["status"] == {
        "availability": "available",
        "activity": "monitoring",
    }, attached
    assert unaffected["status"] == {
        "availability": "available",
        "activity": "monitoring",
    }, unaffected
    record(
        "new_worker_and_existing_worker_are_independent",
        {
            "attached": attached,
            "unaffected": unaffected,
            "attached_log": attached_log,
            "unaffected_log": unaffected_log,
        },
    )

    args.report.write_text(
        json.dumps({"complete": True, "tests": evidence}, indent=2) + "\n"
    )
    print(f"{len(evidence)} checks passed; report: {args.report}", flush=True)


if __name__ == "__main__":
    main()
