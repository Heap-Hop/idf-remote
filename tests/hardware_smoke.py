"""Explicit opt-in integration test. All hardware access goes through HTTP.

Run only against a disposable firmware target: this writes the selected app.
The required USB serial and chip checks run before any mutation.
"""
import argparse
import hashlib
import json
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", default="http://127.0.0.1:9876")
    parser.add_argument("--port", required=True)
    parser.add_argument("--usb-serial", required=True)
    parser.add_argument("--build-dir", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--token-file", type=Path)
    args = parser.parse_args()
    results = []
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    request_number = 0
    base_headers = {"Content-Type": "application/json"}
    if args.token_file:
        base_headers["Authorization"] = "Bearer " + args.token_file.read_text().strip()

    def call(path, body=None, expected=200):
        nonlocal request_number
        data = None if body is None else json.dumps(body).encode()
        headers = base_headers.copy()
        if body is not None:
            request_number += 1
            headers["Idempotency-Key"] = f"hardware-smoke-{time.time_ns()}-{request_number}"
        request = urllib.request.Request(args.url + path, data=data, headers=headers)
        try:
            with opener.open(request, timeout=30) as response:
                status, value = response.status, json.load(response)
        except urllib.error.HTTPError as error:
            status, value = error.code, json.load(error)
        assert status == expected, (path, status, value)
        return value

    def call_flash(metadata, parts, expected=202):
        nonlocal request_number
        boundary = f"idf-remote-smoke-{time.time_ns()}"
        body = bytearray()

        def field(name, data, content_type, filename=None):
            body.extend(f"--{boundary}\r\n".encode())
            disposition = f'Content-Disposition: form-data; name="{name}"'
            if filename:
                disposition += f'; filename="{filename}"'
            body.extend((disposition + "\r\n").encode())
            body.extend(f"Content-Type: {content_type}\r\n\r\n".encode())
            body.extend(data)
            body.extend(b"\r\n")

        field("metadata", json.dumps(metadata, separators=(",", ":")).encode(),
              "application/json")
        for name, data in parts:
            field(name, data, "application/octet-stream", "artifact.bin")
        body.extend(f"--{boundary}--\r\n".encode())
        request_number += 1
        headers = base_headers.copy()
        headers["Content-Type"] = f"multipart/form-data; boundary={boundary}"
        headers["Idempotency-Key"] = f"hardware-smoke-{time.time_ns()}-{request_number}"
        request = urllib.request.Request(args.url + "/v1/flash", data=bytes(body), headers=headers)
        try:
            with opener.open(request, timeout=30) as response:
                status, value = response.status, json.load(response)
        except urllib.error.HTTPError as error:
            status, value = error.code, json.load(error)
        assert status == expected, ("/v1/flash", status, value)
        return value

    def done(operation, expected="succeeded"):
        deadline = time.monotonic() + 60
        while operation["status"] == "running":
            assert time.monotonic() < deadline, operation
            time.sleep(0.1)
            operation = call("/v1/operations/" + operation["id"])
        assert operation["status"] == expected, operation
        return operation

    def check(name, evidence):
        results.append({"test": name, "evidence": evidence})
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps({"port": args.port, "complete": False, "tests": results}, indent=2) + "\n")
        print(name + ": PASS", flush=True)

    device = call("/v1/devices")
    matches = [
        candidate
        for candidate in device["devices"]
        if candidate["transport"]["address"] == args.port
    ]
    assert len(matches) == 1, device
    identity = matches[0]
    assert identity["transport"]["address"] == args.port, identity
    assert identity["transport"]["metadata"]["serial_number"] == args.usb_serial, identity
    device_id = identity["id"]
    request = {"device_id": device_id, "monitor_baud": 115200}
    probe = done(call("/v1/probe", request, 202))
    assert probe["result"]["chip"] == "esp32s3", probe
    check("exact_device_identity", {"usb": identity, "chip": probe["result"]})

    rejected = call("/v1/reset", {**request, "device_id": "dev_forbidden"}, 403)
    check("wrong_device_rejected", rejected)

    manifest = json.loads((args.build_dir / "flasher_args.json").read_text())
    app = manifest["app"]
    data = (args.build_dir / app["file"]).read_bytes()
    upload = {
        "device_id": device_id, "version": 2, "chip": "esp32s3",
        "flash_settings": {"mode": manifest["flash_settings"]["flash_mode"],
                           "frequency": manifest["flash_settings"]["flash_freq"],
                           "size": manifest["flash_settings"]["flash_size"]},
        "segments": [{"part": "segment-0", "offset": int(app["offset"], 0),
                      "size": len(data), "sha256": hashlib.sha256(data).hexdigest()}],
        "flash_baud": 460800, "monitor_baud": 115200,
    }
    bad_hash = {**upload, "segments": [{**upload["segments"][0], "sha256": "0" * 64}]}
    check("corrupt_upload_rejected", call_flash(bad_hash, [("segment-0", data)], 400))

    # POST returns and closes its HTTP connection. The flash continues on the
    # device worker; subsequent requests use entirely new TCP connections.
    operation = call_flash(upload, [("segment-0", data)])
    check("concurrent_reset_rejected", call("/v1/reset", request, 409))
    finished = done(operation)
    check("app_only_survives_request_disconnect", finished)
    marker = call("/v1/wait", {"device_id": device_id, "cursor": operation["start_cursor"],
                              "pattern": "IDF_REMOTE_BOOT smoke-v1", "timeout_ms": 5000})
    check("startup_replayed_from_flash_cursor", marker)
    event_query = {**operation["start_cursor"], "device_id": device_id}
    events = call("/v1/events?" + urllib.parse.urlencode(event_query))
    starts = [event["data"]["offset"] for event in events["events"]
              if event["kind"] == "progress" and event["data"]["event"] == "segment_started"]
    assert starts == [int(app["offset"], 0)], starts
    check("only_app_segment_written", starts)

    cursor = call("/v1/devices")["cursors"][device_id]
    check("old_boot_log_does_not_satisfy_new_wait", call("/v1/wait", {
        "device_id": device_id, "cursor": cursor, "pattern": "IDF_REMOTE_BOOT smoke-v1", "timeout_ms": 250}, 408))

    # Deliberately mismatched expected chip fails before any flash write. The
    # exact allowed physical port remains unchanged, then reset recovers it.
    failed = done(call_flash({**upload, "chip": "esp32c3"}, [("segment-0", data)]), "failed")
    assert "match" in failed["error"].lower() or "chip" in failed["error"].lower(), failed
    check("chip_mismatch_does_not_write", failed)
    reset = done(call("/v1/reset", request, 202))
    marker = call("/v1/wait", {"device_id": device_id, "cursor": reset["start_cursor"],
                              "pattern": "IDF_REMOTE_BOOT smoke-v1", "timeout_ms": 5000})
    check("failure_releases_lock_and_reset_recovers", marker)

    cursor = call("/v1/devices")["cursors"][device_id]
    path = args.url + "/v1/stream?" + urllib.parse.urlencode(
        {**cursor, "device_id": device_id}
    )
    stream_request = urllib.request.Request(path, headers=base_headers)
    with opener.open(stream_request, timeout=5) as response:
        assert response.headers["Content-Type"].startswith("text/event-stream")
        deadline = time.monotonic() + 5
        while True:
            assert time.monotonic() < deadline
            line = response.readline().decode()
            if line.startswith("data:"):
                event = json.loads(line[5:])
                if event["kind"] == "log" and "IDF_REMOTE_TICK" in event["data"]["text"]:
                    check("sse_live_serial_log", event)
                    break

    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps({"port": args.port, "complete": True, "tests": results}, indent=2) + "\n")
    print(f"{len(results)} checks passed; report: {args.report}")


if __name__ == "__main__":
    main()
