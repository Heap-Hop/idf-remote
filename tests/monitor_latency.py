"""Opt-in USB echo benchmark. Never flashes; requires latency_firmware already running.

Measures real CLI input through a PTY (raw-mode path), and HTTP-to-USB echo.
Only the exact --port and --usb-serial pair may receive monitor/write requests.
Python standard library for HTTP/CLI; --direct additionally needs pyserial.
Run on macOS/Linux. Stop the daemon before --direct to release the USB port.
"""
import argparse
import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import pty
import select
import statistics
import subprocess
import termios
import time
import urllib.parse


def summary(values):
    ordered = sorted(values)
    if not ordered:
        return {"count": 0}
    return {"count": len(values), "median_ms": round(statistics.median(values), 3),
            "p95_ms": round(ordered[min(len(values) - 1, int(len(values) * .95))], 3),
            "max_ms": round(max(values), 3)}


class Api:
    def __init__(self, url):
        url = urllib.parse.urlsplit(url)
        assert url.scheme == "http"
        self.connection = http.client.HTTPConnection(url.hostname, url.port, timeout=10)

    def call(self, path, body=None):
        headers = {}
        if body is not None:
            headers = {"Content-Type": "application/json", "Idempotency-Key": f"latency-{time.time_ns()}"}
        self.connection.request("GET" if body is None else "POST", path,
                                None if body is None else json.dumps(body), headers)
        response = self.connection.getresponse()
        value = json.loads(response.read())
        assert response.status in (200, 202), (response.status, value)
        return value

    def done(self, operation):
        deadline = time.monotonic() + 10
        while operation["status"] == "running":
            assert time.monotonic() < deadline, operation
            time.sleep(.001)
            operation = self.call("/v1/operations/" + operation["id"])
        assert operation["status"] == "succeeded", operation
        return operation


def api_latency(api, device_id, count):
    api.done(api.call("/v1/monitor", {"device_id": device_id}))
    cursor = api.call("/v1/devices")["cursors"][device_id]
    latency, admission, completion = [], [], []
    for index in range(count):
        data = f"[api-{index:06d}]".encode()
        started = time.perf_counter()
        operation = api.call("/v1/serial-write", {
            "device_id": device_id, "data": base64.b64encode(data).decode(),
            "sha256": hashlib.sha256(data).hexdigest(), "baud": 115200, "timeout_ms": 2000})
        admission.append((time.perf_counter() - started) * 1000)
        received = bytearray()
        deadline = time.monotonic() + 3
        while data not in received and time.monotonic() < deadline:
            batch = api.call("/v1/events?" + urllib.parse.urlencode({"device_id": device_id, **cursor}))
            cursor = batch["cursor"]
            for event in batch["events"]:
                if event["kind"] == "raw":
                    received.extend(base64.b64decode(event["data"]["base64"]))
            if data not in received:
                time.sleep(.001)
        if data in received:
            latency.append((time.perf_counter() - started) * 1000)
        api.done(operation)
        completion.append((time.perf_counter() - started) * 1000)
        time.sleep(.017)
    return {"echo": summary(latency), "post_response": summary(admission),
            "completion_observed": summary(completion), "missing": count - len(latency)}


def read_ready(fd, timeout):
    return os.read(fd, 65536) if select.select([fd], [], [], timeout)[0] else b""


def direct_latency(args):
    import serial
    import serial.tools.list_ports
    matches = [port for port in serial.tools.list_ports.comports() if port.device == args.port]
    assert len(matches) == 1 and matches[0].serial_number == args.usb_serial, matches
    result = {"port": args.port, "usb_serial": args.usb_serial}
    with serial.Serial(args.port, 115200, timeout=1, exclusive=True) as port:
        for flush in (False, True):
            echo, write = [], []
            port.reset_input_buffer()
            for index in range(args.samples):
                data = bytes([ord('a') + index % 26])
                started = time.perf_counter()
                port.write(data)
                if flush:
                    port.flush()
                write.append((time.perf_counter() - started) * 1000)
                assert port.read(1) == data, "expected immediate echo fixture"
                echo.append((time.perf_counter() - started) * 1000)
                time.sleep(.017)
            result["with_flush" if flush else "without_flush"] = {
                "echo": summary(echo), "write": summary(write)}
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result), flush=True)


def cli_latency(args):
    master, slave = pty.openpty()
    original = termios.tcgetattr(slave)
    process = subprocess.Popen([str(args.binary.resolve()), "--url", args.url, "--color", "never",
                                "monitor", "--port", args.port], stdin=slave,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        deadline = time.monotonic() + 10
        while termios.tcgetattr(slave)[3] & termios.ICANON:
            assert process.poll() is None, process.stderr.read().decode()
            assert time.monotonic() < deadline, "CLI did not enter raw mode"
            time.sleep(.01)
        fd = process.stdout.fileno()
        while read_ready(fd, .1):
            pass
        latency = []
        for index in range(args.samples):
            data = bytes([ord('a') + index % 26])
            started = time.perf_counter()
            os.write(master, data)
            received = bytearray()
            deadline = time.monotonic() + 2
            while data not in received and time.monotonic() < deadline:
                received.extend(read_ready(fd, .05))
                assert process.poll() is None, process.stderr.read().decode()
            if data in received:
                latency.append((time.perf_counter() - started) * 1000)
            time.sleep(.017)

        # Overlap input with earlier output: catches echo discarded during admission.
        received = bytearray()
        sent, echoed = {}, {}
        start = time.perf_counter()
        for index in range(args.burst_samples):
            token = f"<{index:06d}>".encode()
            sent[token] = time.perf_counter()
            os.write(master, token)
            next_send = start + (index + 1) * args.interval_ms / 1000
            while time.perf_counter() < next_send:
                received.extend(read_ready(fd, max(0, next_send - time.perf_counter())))
                now = time.perf_counter()
                for key, timestamp in sent.items():
                    if key not in echoed and key in received:
                        echoed[key] = (now - timestamp) * 1000
        deadline = time.monotonic() + 3
        while len(echoed) < len(sent) and time.monotonic() < deadline:
            received.extend(read_ready(fd, .01))
            now = time.perf_counter()
            for key, timestamp in sent.items():
                if key not in echoed and key in received:
                    echoed[key] = (now - timestamp) * 1000
        os.write(master, b'\x1d')
        process.wait(timeout=5)
        assert process.returncode == 0, process.stderr.read().decode()
        restored = termios.tcgetattr(slave)
        # The kernel clears PENDIN when input is read. It is pending input state,
        # not a raw-mode setting which the application failed to restore.
        restored[3] &= ~termios.PENDIN
        original[3] &= ~termios.PENDIN
        return {"single_key_echo": summary(latency), "single_key_missing": args.samples - len(latency),
                "paced_echo": summary(list(echoed.values())), "paced_missing": len(sent) - len(echoed),
                "paced_exact_bytes": received == b"".join(sent),
                "paced_interval_ms": args.interval_ms, "terminal_restored": restored == original,
                "terminal_changes": [index for index in range(len(original)) if original[index] != restored[index]],
                "terminal_lflag_xor": hex(original[3] ^ restored[3])}
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=5)
        os.close(master)
        os.close(slave)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:38473")
    parser.add_argument("--port", required=True)
    parser.add_argument("--usb-serial", required=True)
    parser.add_argument("--binary", type=Path, default=Path("target/release/idfr"))
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=50)
    parser.add_argument("--burst-samples", type=int, default=100)
    parser.add_argument("--interval-ms", type=float, default=10)
    parser.add_argument("--max-p95-ms", type=float,
                        help="optional CLI latency gate; results are saved before checking")
    parser.add_argument("--direct", action="store_true", help="USB-only baseline; daemon must be stopped")
    args = parser.parse_args()
    assert args.samples > 0 and args.burst_samples > 0 and args.interval_ms > 0
    if args.direct:
        direct_latency(args)
        return
    api = Api(args.url)
    matches = [device for device in api.call("/v1/devices")["devices"]
               if device["transport"]["address"] == args.port]
    assert len(matches) == 1, matches
    device = matches[0]
    assert device["transport"]["metadata"]["serial_number"] == args.usb_serial, device
    result = {"device": device, "binary": str(args.binary), "url": args.url,
              "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
              "measured_at": time.strftime("%Y-%m-%dT%H:%M:%S%z")}
    result["api"] = api_latency(api, device["id"], args.samples)
    print(json.dumps({"api": result["api"]}), flush=True)
    result["cli"] = cli_latency(args)
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"cli": result["cli"]}), flush=True)
    assert result["api"]["missing"] == 0 and result["cli"]["single_key_missing"] == 0, result
    assert result["cli"]["paced_exact_bytes"], "paced echo lost, duplicated or reordered bytes"
    assert result["cli"]["terminal_restored"], "terminal settings not restored"
    if args.max_p95_ms is not None:
        for key in ("single_key_echo", "paced_echo"):
            assert result["cli"][key]["p95_ms"] <= args.max_p95_ms, result["cli"]


if __name__ == "__main__":
    main()
