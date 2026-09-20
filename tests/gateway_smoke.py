#!/usr/bin/env python3
"""Non-flashing real-board application protocol checks; requires the gateway demo."""
import argparse
import base64
import hashlib
import json
import pathlib
import statistics
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:19876")
    parser.add_argument("--port", required=True)
    parser.add_argument("--usb-serial", required=True)
    parser.add_argument("--report", type=pathlib.Path, required=True)
    args = parser.parse_args()

    def http(path, body=None, key=None):
        request = urllib.request.Request(args.url + path, data=None if body is None else json.dumps(body).encode(), headers={"Content-Type": "application/json", "Idempotency-Key": key or str(uuid.uuid4())})
        with urllib.request.urlopen(request, timeout=35) as response:
            return json.load(response)

    devices = http("/v1/devices")
    matches = [d for d in devices["devices"] if d["transport"].get("address") == args.port and d["transport"]["metadata"].get("serial_number") == args.usb_serial]
    assert len(matches) == 1, "expected USB fixture not found"
    device = matches[0]["id"]
    cursor = devices["cursors"][device]

    def submit(method=None, params=None, timeout_ms=2000):
        body = {"device_id": device, "timeout_ms": timeout_ms}
        if method is not None:
            body["command"] = {"method": method, "params": params}
        return http("/v1/application", body)

    def finish(operation, success=True):
        deadline = time.monotonic() + 35
        while operation["status"] == "running":
            assert time.monotonic() < deadline, "operation polling timed out"
            time.sleep(0.002)
            operation = http("/v1/operations/" + operation["id"])
        assert operation["status"] == ("succeeded" if success else "failed"), operation
        return operation

    def call(method=None, params=None, timeout_ms=2000):
        return finish(submit(method, params, timeout_ms))["result"]

    identity = call()
    assert identity["application"] == "gateway-demo" and identity["protocol"] == 1, identity
    initial_status = call("status")
    special = {"text": "~}\\中文", "values": [1, 2, 3]}
    assert call("echo", special) == special
    try:
        submit("echo", {"text": "a\0b"})
        raise AssertionError("expected rejection of embedded NUL")
    except urllib.error.HTTPError as error:
        assert error.code == 400 and "NUL" in error.read().decode()

    latencies = []
    for i in range(100):
        start = time.monotonic()
        assert call("echo", {"sequence": i}) == {"sequence": i}
        latencies.append((time.monotonic() - start) * 1000)

    # Input travels as console frames while requests/events use other channels.
    data = b"abc\t\x03\x7f\r"
    serial = http("/v1/serial-write", {"device_id": device, "data": base64.b64encode(data).decode(), "sha256": hashlib.sha256(data).hexdigest(), "baud": 115200, "timeout_ms": 2000})
    finish(serial)
    delayed = submit("delay", 1200)
    try:
        submit("echo", "competing client")
        raise AssertionError("expected device_busy")
    except urllib.error.HTTPError as error:
        assert error.code == 409 and "device_busy" in error.read().decode()
    time.sleep(1.05)
    query = urllib.parse.urlencode({"device_id": device, **cursor})
    during = http("/v1/events?" + query)
    for kind in ("application_event", "log"):
        assert any(e["kind"] == kind and e["seq"] > delayed["start_cursor"]["after"] for e in during["events"]), f"no {kind} during pending request"
    assert http("/v1/operations/" + delayed["id"])["status"] == "running"
    finish(delayed)
    unknown = finish(submit("not_a_method"), success=False)
    assert "ESP_ERR_NOT_SUPPORTED" in unknown["error"]
    timed_out = finish(submit("delay", 400, timeout_ms=50), success=False)
    assert "outcome unknown" in timed_out["error"]
    time.sleep(0.5)
    assert call("echo", "after timeout") == "after timeout"
    before_burst = call("status")
    start = time.monotonic()
    assert call("log_burst") == "done"
    burst_ms = (time.monotonic() - start) * 1000
    status = call("status")
    time.sleep(0.2)
    events = http("/v1/events?" + query)["events"]
    raw = b"".join(base64.b64decode(e["data"]["base64"]) for e in events if e["kind"] == "raw")
    for marker in (b"gateway:", b"printf tick", b"stderr tick", b"stdin:09", b"stdin:03", b"stdin:7f"):
        assert marker in raw, (marker, raw[-2000:])
    for byte in data:
        assert f"stdin:{byte:02x}".encode() in raw
    assert not any(e["kind"] == "application_protocol_error" for e in events), "corrupt frame"
    assert status["dropped_control"] == initial_status["dropped_control"], (initial_status, status)
    report = {"identity": identity, "samples": len(latencies), "http_roundtrip_ms": {"p50": statistics.median(latencies), "p95": sorted(latencies)[94], "max": max(latencies)}, "log_burst_roundtrip_ms": burst_ms, "status_after_burst": status, "burst_drops": {key: status[key] - before_burst[key] for key in ("dropped_console", "dropped_control")}, "checks": ["echo", "stdio capture", "console input", "events during request", "busy admission", "application error", "timeout without replay", "late response isolation", "no corrupt frames"]}
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))

if __name__ == "__main__":
    main()
