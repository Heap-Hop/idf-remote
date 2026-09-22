use anyhow::Result;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use idf_remote::{
    application::Command,
    backend::{BoardInfo, DeviceBackend, DeviceSession, Progress, SerialIo},
    device::{DeviceCapability, DeviceDescriptor, DeviceId, DeviceStatus, TransportDescriptor},
    mux::{self, Decoded, Frame},
    plan::PreparedPlan,
    server::{self, Service},
    wire::{ApplicationRequest, Operation, SerialWriteRequest},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tower::ServiceExt;

// Timeouts are deadlock guards, not latency assertions. Protocol ordering is
// controlled explicitly so runner load and timer granularity cannot decide it.
const WATCHDOG: Duration = Duration::from_secs(10);
const OP_TIMEOUT_MS: u64 = 30_000;

#[derive(Default)]
struct PeerControl {
    release_response: AtomicBool,
    pause_writes: AtomicBool,
}
struct Backend {
    opens: Arc<AtomicUsize>,
    control: Arc<PeerControl>,
}
fn descriptor() -> DeviceDescriptor {
    DeviceDescriptor {
        id: DeviceId::new("fixture").unwrap(),
        display_name: "fake".into(),
        transport: TransportDescriptor {
            kind: "fake".into(),
            address: None,
            metadata: Default::default(),
        },
        status: DeviceStatus::AVAILABLE_IDLE,
        capabilities: vec![
            DeviceCapability::SerialWrite,
            DeviceCapability::Monitor,
            DeviceCapability::Reset,
        ],
    }
}
impl DeviceBackend for Backend {
    fn devices(&self) -> Result<Vec<DeviceDescriptor>> {
        Ok(vec![descriptor()])
    }
    fn refresh(&self, _: &DeviceId) -> Result<DeviceDescriptor> {
        Ok(descriptor())
    }
    fn validate(&self, _: &PreparedPlan) -> Result<()> {
        Ok(())
    }
    fn open(&self, _: &DeviceId, _: bool) -> Result<Box<dyn DeviceSession>> {
        self.opens.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(Hardware(self.control.clone())))
    }
}
struct Hardware(Arc<PeerControl>);
impl DeviceSession for Hardware {
    fn probe(&mut self) -> Result<BoardInfo> {
        unreachable!()
    }
    fn flash(
        &mut self,
        _: &PreparedPlan,
        _: u32,
        _: &mut dyn FnMut(Progress),
    ) -> Result<BoardInfo> {
        unreachable!()
    }
    fn read_flash(&mut self, _: u32, _: u32, _: u32, _: &mut dyn Write) -> Result<BoardInfo> {
        unreachable!()
    }
    fn erase_flash(&mut self, _: u32) -> Result<BoardInfo> {
        unreachable!()
    }
    fn into_monitor(self: Box<Self>, _: u32, _: bool) -> Result<Box<dyn SerialIo>> {
        Ok(Box::new(Peer {
            control: self.0.clone(),
            ..Peer::default()
        }))
    }
}
#[derive(Default)]
struct Peer {
    decoder: mux::Decoder,
    bytes: VecDeque<u8>,
    deferred: Option<Vec<u8>>,
    control: Arc<PeerControl>,
    write_blocked: bool,
}
impl Write for Peer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.write_blocked || self.control.pause_writes.load(Ordering::SeqCst) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.write_blocked = true;
        let bytes = &bytes[..bytes.len().min(7)];
        for item in self.decoder.feed(bytes) {
            if let Decoded::Frame(frame) = item {
                match frame.kind {
                    mux::HELLO => self.bytes.extend(
                        Frame {
                            kind: mux::HELLO_ACK,
                            payload: br#"{"protocol":1,"application":"test"}"#.to_vec(),
                            ..frame
                        }
                        .encode()
                        .unwrap(),
                    ),
                    mux::REQUEST => {
                        self.bytes.extend(
                            Frame {
                                kind: mux::CONSOLE,
                                request: 0,
                                payload: b"still logging\n".to_vec(),
                                ..frame.clone()
                            }
                            .encode()
                            .unwrap(),
                        );
                        self.bytes.extend(
                            Frame {
                                kind: mux::EVENT,
                                request: 0,
                                payload: br#"{"alive":true}"#.to_vec(),
                                ..frame.clone()
                            }
                            .encode()
                            .unwrap(),
                        );
                        let command: Command = serde_json::from_slice(&frame.payload).unwrap();
                        let response = Frame {
                            kind: mux::RESPONSE,
                            payload: serde_json::to_vec(&command.params).unwrap(),
                            ..frame
                        }
                        .encode()
                        .unwrap();
                        if command.method == "fast" {
                            // Stop subsequent writes but keep RX live until the
                            // test has observed this reply during console input.
                            self.control.pause_writes.store(true, Ordering::SeqCst);
                            self.bytes.extend(response);
                        } else {
                            self.deferred = Some(response);
                        }
                    }
                    mux::CONSOLE => self.bytes.extend(frame.encode().unwrap()),
                    _ => {}
                }
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        panic!("the worker must not block on drain/flush")
    }
}
impl Read for Peer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.write_blocked = false;
        if self.control.release_response.load(Ordering::SeqCst)
            && let Some(response) = self.deferred.take()
        {
            self.bytes.extend(response);
        }
        if self.bytes.is_empty() {
            std::thread::sleep(Duration::from_millis(1));
            return Err(io::ErrorKind::TimedOut.into());
        }
        let n = buf.len().min(self.bytes.len());
        for b in &mut buf[..n] {
            *b = self.bytes.pop_front().unwrap();
        }
        Ok(n)
    }
}
async fn finish(service: &Service, op: Operation) -> Operation {
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let current = service.get_operation(&op.id).unwrap();
            if current.status != "running" {
                return current;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap()
}
async fn post(app: axum::Router, path: &str, key: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .header("idempotency-key", key)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    (
        response.status(),
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap(),
    )
}
#[tokio::test]
async fn http_and_embedded_calls_share_one_owner_and_stream_while_busy() {
    let opens = Arc::new(AtomicUsize::new(0));
    let control = Arc::new(PeerControl::default());
    let running = Arc::new(AtomicBool::new(true));
    let (service, worker) = Service::start(
        Arc::new(Backend {
            opens: opens.clone(),
            control: control.clone(),
        }),
        descriptor(),
        None,
        running.clone(),
    )
    .unwrap();
    let app = server::router(service.clone());
    let request = ApplicationRequest {
        device_id: descriptor().id,
        monitor_baud: 115200,
        timeout_ms: OP_TIMEOUT_MS,
        command: None,
    };
    // Raw HTTP clients must supply the key too; rejection must not touch USB.
    let response = app
        .clone()
        .oneshot(
            Request::post("/v1/application")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024).await.unwrap()).unwrap();
    assert_eq!(error["error"], "missing Idempotency-Key header");
    assert_eq!(opens.load(Ordering::Relaxed), 0);
    let connected = finish(
        &service,
        service
            .submit_application(request.clone(), "connect")
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(connected.status, "succeeded");
    // Distinct clients may explicitly negotiate again. Broadcasting a false
    // disconnect here causes reconnect-on-disconnect clients to trigger themselves.
    for key in ["client-a-connect", "client-b-connect"] {
        let (status, value) = post(
            app.clone(),
            "/v1/application",
            key,
            serde_json::to_value(&request).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let connected = finish(&service, serde_json::from_value(value).unwrap()).await;
        assert_eq!(connected.status, "succeeded");
        let events = service
            .application_events(&descriptor().id, &connected.start_cursor)
            .unwrap();
        assert_eq!(
            events
                .events
                .iter()
                .filter(|e| e.kind == "application_connected")
                .count(),
            1
        );
        assert!(
            !events
                .events
                .iter()
                .any(|e| e.kind == "application_disconnected")
        );
        let (status, replay) = post(
            app.clone(),
            "/v1/application",
            key,
            serde_json::to_value(&request).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(replay["id"], connected.id);
        let replayed_events = service
            .application_events(&descriptor().id, &connected.start_cursor)
            .unwrap();
        assert_eq!(replayed_events.events.len(), events.events.len());
    }
    assert_eq!(opens.load(Ordering::Relaxed), 1);
    let command = ApplicationRequest {
        command: Some(Command {
            method: "echo".into(),
            params: json!({"value":42}),
        }),
        ..request.clone()
    };
    let (status, value) = post(
        app.clone(),
        "/v1/application",
        "call",
        serde_json::to_value(&command).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let op: Operation = serde_json::from_value(value).unwrap();
    assert!(
        service
            .submit_application(command.clone(), "competing")
            .await
            .unwrap_err()
            .to_string()
            .contains("device_busy")
    );
    let replay = service.submit_application(command, "call").await.unwrap();
    assert_eq!(replay.id, op.id);
    // The HTTP executor stays responsive while a worker waits for its response.
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let events = service
                .application_events(&descriptor().id, &op.start_cursor)
                .unwrap();
            if events.events.iter().any(|e| e.kind == "application_event")
                && events.events.iter().any(|e| e.kind == "log")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(service.get_operation(&op.id).unwrap().status, "running");

    // A console input can finish while the command is still waiting for its
    // response. Hardware replacement and renegotiation remain exclusive.
    for path in ["/v1/reset", "/v1/monitor"] {
        let (status, _) = post(
            app.clone(),
            path,
            &format!("busy-{}", path.rsplit('/').next().unwrap()),
            json!({"device_id":"fixture"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }
    assert!(
        service
            .submit_application(request.clone(), "reconnect-busy")
            .await
            .is_err()
    );
    // A mismatched baud must fail without disturbing the pending command.
    let wrong_baud =
        SerialWriteRequest::from_bytes(descriptor().id, b"bad", 9600, OP_TIMEOUT_MS).unwrap();
    let (status, value) = post(
        app.clone(),
        "/v1/serial-write",
        "wrong-baud",
        serde_json::to_value(wrong_baud).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        finish(&service, serde_json::from_value(value).unwrap())
            .await
            .status,
        "failed"
    );
    let input = SerialWriteRequest::from_bytes(
        descriptor().id,
        b"interactive input\n",
        115200,
        OP_TIMEOUT_MS,
    )
    .unwrap();
    let (status, value) = post(
        app.clone(),
        "/v1/serial-write",
        "input",
        serde_json::to_value(input).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        finish(&service, serde_json::from_value(value).unwrap())
            .await
            .status,
        "succeeded"
    );
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let events = service
                .application_events(&descriptor().id, &op.start_cursor)
                .unwrap();
            if events
                .events
                .iter()
                .any(|e| e.kind == "log" && e.data.to_string().contains("interactive input"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(service.get_operation(&op.id).unwrap().status, "running");
    control.release_response.store(true, Ordering::SeqCst);
    assert_eq!(finish(&service, op).await.result, Some(json!({"value":42})));
    // Reverse admission: hold writes until both operations are admitted. Two
    // console frames suffice; bulk throughput is not part of this contract.
    control.pause_writes.store(true, Ordering::SeqCst);
    let input =
        SerialWriteRequest::from_bytes(descriptor().id, &vec![b'x'; 512], 115200, OP_TIMEOUT_MS)
            .unwrap();
    let (status, value) = post(
        app.clone(),
        "/v1/serial-write",
        "long-input",
        serde_json::to_value(&input).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let long_input: Operation = serde_json::from_value(value).unwrap();
    let (status, _) = post(
        app.clone(),
        "/v1/serial-write",
        "second-input",
        serde_json::to_value(&input).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let quick = ApplicationRequest {
        command: Some(Command {
            method: "fast".into(),
            params: json!("quick"),
        }),
        ..request.clone()
    };
    let quick = service
        .submit_application(quick, "during-input")
        .await
        .unwrap();
    control.pause_writes.store(false, Ordering::SeqCst);
    assert_eq!(finish(&service, quick).await.result, Some(json!("quick")));
    assert_eq!(
        service.get_operation(&long_input.id).unwrap().status,
        "running"
    );
    control.pause_writes.store(false, Ordering::SeqCst);
    let finished = finish(&service, long_input).await;
    assert_eq!(finished.status, "succeeded", "{finished:?}");
    // Re-attaching a monitor must not replace the negotiated session or port.
    let (_, value) = post(
        app.clone(),
        "/v1/monitor",
        "monitor",
        json!({"device_id":"fixture"}),
    )
    .await;
    assert_eq!(
        finish(&service, serde_json::from_value(value).unwrap())
            .await
            .status,
        "succeeded"
    );
    assert_eq!(opens.load(Ordering::Relaxed), 1);
    let (_, value) = post(app, "/v1/reset", "reset", json!({"device_id":"fixture"})).await;
    let reset: Operation = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        finish(&service, serde_json::from_value(value).unwrap())
            .await
            .status,
        "succeeded"
    );
    let events = service
        .application_events(&descriptor().id, &reset.start_cursor)
        .unwrap();
    assert!(
        events
            .events
            .iter()
            .any(|e| e.kind == "application_disconnected"
                && e.data["reason"] == "hardware operation")
    );
    let command = ApplicationRequest {
        command: Some(Command {
            method: "echo".into(),
            params: Value::Null,
        }),
        ..request
    };
    let op = finish(
        &service,
        service
            .submit_application(command, "after-reset")
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(op.status, "failed");
    assert!(op.error.unwrap().contains("not connected"));
    running.store(false, Ordering::Relaxed);
    worker.join();
}
