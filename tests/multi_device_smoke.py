"""Opt-in two-device isolation test. This script never programs flash."""

import argparse
import json
import time
import urllib.error
import urllib.request
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", default="http://127.0.0.1:38473")
    parser.add_argument("--detach-usb-serial", required=True)
    parser.add_argument("--detach-marker", required=True)
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
                f"multi-device-smoke-{time.time_ns()}-{request_number}"
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

    def devices():
        return call("/v1/devices")

    def by_serial(response, serial):
        matches = [
            device
            for device in response["devices"]
            if device["transport"]["metadata"].get("serial_number") == serial
        ]
        assert len(matches) == 1, response
        return matches[0]

    def record(name, value):
        evidence.append({"test": name, "evidence": value})
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(
            json.dumps({"complete": False, "tests": evidence}, indent=2) + "\n"
        )
        print(name + ": PASS", flush=True)

    initial = devices()
    detached = by_serial(initial, args.detach_usb_serial)
    unaffected = by_serial(initial, args.unaffected_usb_serial)
    assert detached["id"] != unaffected["id"]
    for device in (detached, unaffected):
        assert device["status"]["availability"] == "available", device
        done(
            call(
                "/v1/monitor",
                {"device_id": device["id"], "monitor_baud": 115200},
                202,
            )
        )
    monitored = devices()
    detached = by_serial(monitored, args.detach_usb_serial)
    unaffected = by_serial(monitored, args.unaffected_usb_serial)
    assert detached["status"]["activity"] == "monitoring", detached
    assert unaffected["status"]["activity"] == "monitoring", unaffected
    unaffected_cursor = monitored["cursors"][unaffected["id"]]
    record(
        "two_independent_monitors",
        {"detached": detached, "unaffected": unaffected},
    )

    print(
        "ACTION: unplug device with USB serial " + args.detach_usb_serial,
        flush=True,
    )
    deadline = time.monotonic() + args.action_timeout
    while True:
        assert time.monotonic() < deadline, "detach was not observed"
        response = devices()
        detached = by_serial(response, args.detach_usb_serial)
        unaffected = by_serial(response, args.unaffected_usb_serial)
        if detached["status"]["availability"] == "disconnected":
            disconnect_cursor = response["cursors"][detached["id"]]
            break
        time.sleep(0.2)
    assert detached["status"]["activity"] == "reconnecting", detached
    assert unaffected["status"] == {
        "availability": "available",
        "activity": "monitoring",
    }, unaffected
    live = call(
        "/v1/wait",
        {
            "device_id": unaffected["id"],
            "cursor": unaffected_cursor,
            "pattern": args.unaffected_marker,
            "timeout_ms": 5000,
        },
    )
    record(
        "detach_isolated_to_one_device",
        {"detached": detached, "unaffected": unaffected, "live_log": live},
    )

    print(
        "ACTION: reconnect device with USB serial " + args.detach_usb_serial,
        flush=True,
    )
    deadline = time.monotonic() + args.action_timeout
    while True:
        assert time.monotonic() < deadline, "reattach was not observed"
        response = devices()
        detached = by_serial(response, args.detach_usb_serial)
        unaffected = by_serial(response, args.unaffected_usb_serial)
        if detached["status"] == {
            "availability": "available",
            "activity": "monitoring",
        }:
            break
        time.sleep(0.2)
    assert unaffected["status"] == {
        "availability": "available",
        "activity": "monitoring",
    }, unaffected
    recovered = call(
        "/v1/wait",
        {
            "device_id": detached["id"],
            "cursor": disconnect_cursor,
            "pattern": args.detach_marker,
            "timeout_ms": 5000,
        },
    )
    record(
        "reattach_recovers_only_target_worker",
        {"reconnected": detached, "unaffected": unaffected, "log": recovered},
    )

    args.report.write_text(
        json.dumps({"complete": True, "tests": evidence}, indent=2) + "\n"
    )
    print(f"{len(evidence)} checks passed; report: {args.report}", flush=True)


if __name__ == "__main__":
    main()
