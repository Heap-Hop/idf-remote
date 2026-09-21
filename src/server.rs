use crate::{
    backend::{DeviceBackend, SerialIo},
    device::{
        DeviceActivity, DeviceAvailability, DeviceCapability, DeviceDescriptor, DeviceId,
        DeviceStatus,
    },
    logs::LineDecoder,
    plan::PreparedPlan,
    wire::{
        ArtifactMetadata, Cursor, DeviceRequest, DevicesResponse, EraseFlashRequest, Event,
        EventBatch, EventQuery, FlashUploadManifest, Operation, ReadFlashRequest,
        SerialWriteRequest, WaitRequest, validate_idempotency_key,
    },
};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Multipart, Path, Query, State, multipart::Field},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event as SseEvent, KeepAlive},
    },
    routing::{get, post},
};
use base64::{Engine, prelude::BASE64_STANDARD};
use regex::Regex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    convert::Infallible,
    io::{self, Read, Write},
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::io::AsyncWriteExt;

const MAX_EVENTS: usize = 4096;
const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_MANAGED_DEVICES: usize = 64;
const MAX_FLASH_METADATA_BYTES: usize = 64 * 1024;
const MONITOR_RECONNECT_INITIAL_DELAY: Duration = Duration::from_millis(50);
const MONITOR_RECONNECT_MAX_DELAY: Duration = Duration::from_millis(100);
const NATIVE_USB_BOOT_REOPEN_DELAY: Duration = Duration::from_millis(200);
const DEVICE_DISCOVERY_INTERVAL: Duration = Duration::from_secs(1);

struct MonitorReconnect {
    baud: u32,
    next_attempt: Instant,
    delay: Duration,
    attempts: u64,
}

impl MonitorReconnect {
    fn now(baud: u32) -> Self {
        Self {
            baud,
            next_attempt: Instant::now(),
            delay: MONITOR_RECONNECT_INITIAL_DELAY,
            attempts: 0,
        }
    }

    fn retry_later(&mut self) {
        self.attempts += 1;
        self.next_attempt = Instant::now() + self.delay;
        self.delay = (self.delay * 2).min(MONITOR_RECONNECT_MAX_DELAY);
    }
}

fn inspect_artifact(path: &std::path::Path) -> Result<ArtifactMetadata> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok(ArtifactMetadata {
        size,
        sha256: hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        content_type: "application/octet-stream".into(),
    })
}

#[derive(Clone)]
pub struct Service {
    inner: Arc<Mutex<Store>>,
    devices: Arc<Mutex<HashMap<DeviceId, DeviceRuntime>>>,
    device_workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    backend: Arc<dyn DeviceBackend>,
    token: Option<String>,
    running: Arc<AtomicBool>,
    artifact_dir: Arc<tempfile::TempDir>,
    discovering: bool,
}

#[derive(Clone)]
struct DeviceRuntime {
    descriptor: DeviceDescriptor,
    tx: mpsc::SyncSender<Job>,
}

#[derive(Clone)]
struct ArtifactRecord {
    metadata: ArtifactMetadata,
    path: PathBuf,
}

struct Store {
    epoch: String,
    devices: HashMap<DeviceId, DeviceState>,
    operations: VecDeque<Operation>,
    idempotency: HashMap<String, IdempotencyRecord>,
    artifacts: HashMap<String, ArtifactRecord>,
}

struct DeviceState {
    seq: u64,
    events: VecDeque<(Event, usize)>,
    event_bytes: usize,
    busy: bool,
    // At most one request and one console input; all other actions are exclusive.
    application_pending: bool,
    input_pending: bool,
    // Only operations that replace the monitor invalidate a read already in flight.
    discard_monitor_read: bool,
    changed: Arc<tokio::sync::Notify>,
    status: DeviceStatus,
}

impl Store {
    fn new(devices: impl IntoIterator<Item = (DeviceId, DeviceStatus)>) -> Self {
        Self {
            epoch: format!(
                "{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ),
            devices: devices
                .into_iter()
                .map(|(id, status)| (id, DeviceState::new(status)))
                .collect(),
            operations: VecDeque::new(),
            idempotency: HashMap::new(),
            artifacts: HashMap::new(),
        }
    }

    fn device(&self, device_id: &DeviceId) -> &DeviceState {
        self.devices
            .get(device_id)
            .expect("configured device has runtime state")
    }

    fn device_mut(&mut self, device_id: &DeviceId) -> &mut DeviceState {
        self.devices
            .get_mut(device_id)
            .expect("configured device has runtime state")
    }

    fn cursor(&self, device_id: &DeviceId) -> Cursor {
        Cursor {
            epoch: self.epoch.clone(),
            after: self.device(device_id).seq,
        }
    }

    fn emit(&mut self, device_id: &DeviceId, kind: &str, data: Value) {
        let state = self.device_mut(device_id);
        state.seq += 1;
        let event = Event {
            seq: state.seq,
            kind: kind.into(),
            data,
        };
        let bytes = serde_json::to_vec(&event).unwrap().len();
        state.events.push_back((event, bytes));
        state.changed.notify_waiters();
        state.event_bytes += bytes;
        while state.events.len() > MAX_EVENTS || state.event_bytes > MAX_EVENT_BYTES {
            if let Some((_, bytes)) = state.events.pop_front() {
                state.event_bytes -= bytes;
            }
        }
    }

    fn events_after(&self, device_id: &DeviceId, cursor: &Cursor) -> ApiResult<EventBatch> {
        if cursor.epoch != self.epoch {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "daemon_restarted: cursor epoch differs".into(),
            ));
        }
        let state = self.device(device_id);
        if cursor.after > state.seq {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "cursor is ahead of the event stream".into(),
            ));
        }
        if state
            .events
            .front()
            .is_some_and(|(event, _)| cursor.after < event.seq.saturating_sub(1))
        {
            return Err(ApiError(
                StatusCode::GONE,
                "log_gap: requested events have been evicted".into(),
            ));
        }
        Ok(EventBatch {
            cursor: self.cursor(device_id),
            events: state
                .events
                .iter()
                .filter(|(event, _)| event.seq > cursor.after)
                .map(|(event, _)| event.clone())
                .collect(),
            device_status: state.status,
        })
    }

    fn push_operation(&mut self, operation: Operation) {
        // Keep in-flight requests even when many inputs finish during a slow call.
        while self
            .operations
            .iter()
            .filter(|op| op.status != "running")
            .count()
            >= 64
        {
            let index = self
                .operations
                .iter()
                .position(|op| op.status != "running")
                .unwrap();
            let expired = self.operations.remove(index).unwrap();
            self.idempotency.remove(&expired.request_key);
            if let Some(artifact) = self.artifacts.remove(&expired.id) {
                let _ = std::fs::remove_file(artifact.path);
            }
        }
        self.operations.push_back(operation);
    }
}

impl DeviceState {
    fn new(status: DeviceStatus) -> Self {
        Self {
            seq: 0,
            events: VecDeque::new(),
            event_bytes: 0,
            busy: false,
            application_pending: false,
            input_pending: false,
            discard_monitor_read: false,
            changed: Arc::new(tokio::sync::Notify::new()),
            status,
        }
    }
}

#[derive(Clone)]
struct IdempotencyRecord {
    fingerprint: String,
    operation_id: String,
}

#[derive(Clone)]
struct RequestIdentity {
    key: String,
    fingerprint: String,
}

struct DigestWriter<'a>(&'a mut Sha256);

impl Write for DigestWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn request_identity(
    headers: &HeaderMap,
    operation: &str,
    request: &impl serde::Serialize,
) -> ApiResult<RequestIdentity> {
    let key = headers
        .get("idempotency-key")
        .ok_or_else(|| ApiError::bad("missing Idempotency-Key header"))?
        .to_str()
        .map_err(|_| ApiError::bad("Idempotency-Key must be ASCII"))?;
    validate_idempotency_key(key).map_err(|error| ApiError::bad(format!("{error:#}")))?;
    let mut digest = Sha256::new();
    digest.update(operation.as_bytes());
    digest.update([0]);
    serde_json::to_writer(DigestWriter(&mut digest), request)
        .map_err(|error| ApiError::bad(format!("canonicalize request: {error}")))?;
    Ok(RequestIdentity {
        key: key.into(),
        fingerprint: digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    })
}

enum Action {
    Probe,
    Reset,
    Monitor,
    Application {
        command: Option<crate::application::Command>,
        timeout_ms: u64,
    },
    Flash {
        plan: PreparedPlan,
        baud: u32,
        warnings: Vec<crate::wire::FlashWarning>,
    },
    ReadFlash {
        offset: u32,
        size: u32,
        baud: u32,
    },
    EraseFlash {
        baud: u32,
    },
    SerialWrite {
        data: Vec<u8>,
        sha256: String,
        baud: u32,
        timeout_ms: u64,
    },
}
struct Job {
    id: String,
    request: DeviceRequest,
    action: Action,
}

pub struct Worker {
    device_workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    discovery: Option<thread::JoinHandle<()>>,
}
impl Worker {
    pub fn join(mut self) {
        if let Some(discovery) = self.discovery.take() {
            let _ = discovery.join();
        }
        let workers = std::mem::take(&mut *self.device_workers.lock().unwrap());
        for worker in workers {
            let _ = worker.join();
        }
    }
}

impl Service {
    pub fn start(
        backend: Arc<dyn DeviceBackend>,
        device: DeviceDescriptor,
        token: Option<String>,
        running: Arc<AtomicBool>,
    ) -> Result<(Self, Worker)> {
        Self::start_many(backend, vec![device], token, running)
    }

    pub fn start_many(
        backend: Arc<dyn DeviceBackend>,
        devices: Vec<DeviceDescriptor>,
        token: Option<String>,
        running: Arc<AtomicBool>,
    ) -> Result<(Self, Worker)> {
        ensure!(
            !devices.is_empty(),
            "at least one allowed device is required"
        );
        let mut runtimes = HashMap::with_capacity(devices.len());
        let mut receivers = Vec::with_capacity(devices.len());
        for device in devices {
            let device_id = device.id.clone();
            let (tx, rx) = mpsc::sync_channel(2);
            ensure!(
                runtimes
                    .insert(
                        device_id.clone(),
                        DeviceRuntime {
                            descriptor: device,
                            tx,
                        },
                    )
                    .is_none(),
                "duplicate configured device ID: {device_id}"
            );
            receivers.push((device_id, rx));
        }
        let initial_states = runtimes
            .iter()
            .map(|(id, runtime)| (id.clone(), runtime.descriptor.status));
        let artifact_dir = Arc::new(tempfile::Builder::new().prefix("idf-remote-").tempdir()?);
        let device_workers = Arc::new(Mutex::new(Vec::with_capacity(receivers.len())));
        let service = Self {
            inner: Arc::new(Mutex::new(Store::new(initial_states))),
            devices: Arc::new(Mutex::new(runtimes)),
            device_workers: device_workers.clone(),
            backend,
            token,
            running: running.clone(),
            artifact_dir,
            discovering: false,
        };
        for (device_id, rx) in receivers {
            let worker_service = service.clone();
            let worker_running = running.clone();
            let name = format!("esp-device-{device_id}");
            device_workers.lock().unwrap().push(
                thread::Builder::new()
                    .name(name)
                    .spawn(move || worker_service.worker(device_id, rx, worker_running))?,
            );
        }
        Ok((
            service,
            Worker {
                device_workers,
                discovery: None,
            },
        ))
    }

    pub fn start_discovering(
        backend: Arc<dyn DeviceBackend>,
        token: Option<String>,
        running: Arc<AtomicBool>,
    ) -> Result<(Self, Worker)> {
        Self::start_discovering_with_interval(backend, token, running, DEVICE_DISCOVERY_INTERVAL)
    }

    fn start_discovering_with_interval(
        backend: Arc<dyn DeviceBackend>,
        token: Option<String>,
        running: Arc<AtomicBool>,
        interval: Duration,
    ) -> Result<(Self, Worker)> {
        let device_workers = Arc::new(Mutex::new(Vec::new()));
        let service = Self {
            inner: Arc::new(Mutex::new(Store::new(
                Vec::<(DeviceId, DeviceStatus)>::new(),
            ))),
            devices: Arc::new(Mutex::new(HashMap::new())),
            device_workers: device_workers.clone(),
            backend,
            token,
            running: running.clone(),
            artifact_dir: Arc::new(tempfile::Builder::new().prefix("idf-remote-").tempdir()?),
            discovering: true,
        };
        let discovery_service = service.clone();
        let discovery_running = running.clone();
        let discovery = thread::Builder::new()
            .name("esp-device-discovery".into())
            .spawn(move || {
                let mut last_error = None;
                while discovery_running.load(Ordering::Relaxed) {
                    match discovery_service.backend.devices() {
                        Ok(devices) => match discovery_service.sync_discovered(devices) {
                            Ok(()) => {
                                if last_error.take().is_some() {
                                    eprintln!("device discovery recovered");
                                }
                            }
                            Err(error) => {
                                let message = format!("{error:#}");
                                if last_error.as_deref() != Some(message.as_str()) {
                                    eprintln!("device discovery update failed: {message}");
                                    last_error = Some(message);
                                }
                            }
                        },
                        Err(error) => {
                            let message = format!("{error:#}");
                            if last_error.as_deref() != Some(message.as_str()) {
                                eprintln!("device discovery failed: {message}");
                                last_error = Some(message);
                            }
                        }
                    }
                    let deadline = Instant::now() + interval;
                    while discovery_running.load(Ordering::Relaxed) && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(20));
                    }
                }
            })?;
        Ok((
            service,
            Worker {
                device_workers,
                discovery: Some(discovery),
            },
        ))
    }

    fn runtime(&self, device_id: &DeviceId) -> ApiResult<DeviceRuntime> {
        self.devices
            .lock()
            .unwrap()
            .get(device_id)
            .cloned()
            .ok_or_else(|| {
                ApiError(
                    StatusCode::FORBIDDEN,
                    "device_not_allowed: device is not in the daemon inventory".into(),
                )
            })
    }

    fn authorize_device(&self, device_id: &DeviceId) -> ApiResult<()> {
        self.runtime(device_id).map(|_| ())
    }

    fn sync_discovered(&self, descriptors: Vec<DeviceDescriptor>) -> Result<()> {
        let seen: HashSet<_> = descriptors
            .iter()
            .map(|descriptor| descriptor.id.clone())
            .collect();
        for descriptor in descriptors {
            self.upsert_discovered(descriptor)?;
        }

        let missing: Vec<_> = self
            .devices
            .lock()
            .unwrap()
            .keys()
            .filter(|device_id| !seen.contains(*device_id))
            .cloned()
            .collect();
        for device_id in missing {
            if let Some(runtime) = self.devices.lock().unwrap().get_mut(&device_id) {
                runtime.descriptor.status.availability = DeviceAvailability::Disconnected;
            }
            self.inner
                .lock()
                .unwrap()
                .device_mut(&device_id)
                .status
                .availability = DeviceAvailability::Disconnected;
        }
        Ok(())
    }

    fn upsert_discovered(&self, descriptor: DeviceDescriptor) -> Result<()> {
        let device_id = descriptor.id.clone();
        let mut runtimes = self.devices.lock().unwrap();
        if let Some(runtime) = runtimes.get_mut(&device_id) {
            runtime.descriptor = descriptor.clone();
            self.inner
                .lock()
                .unwrap()
                .device_mut(&device_id)
                .status
                .availability = descriptor.status.availability;
            return Ok(());
        }
        ensure!(
            runtimes.len() < MAX_MANAGED_DEVICES,
            "dynamic device limit of {MAX_MANAGED_DEVICES} reached"
        );

        let (tx, rx) = mpsc::sync_channel(2);
        self.inner
            .lock()
            .unwrap()
            .devices
            .insert(device_id.clone(), DeviceState::new(descriptor.status));
        let worker_service = self.clone();
        let worker_running = self.running.clone();
        let worker_id = device_id.clone();
        let name = format!("esp-device-{device_id}");
        let worker = match thread::Builder::new()
            .name(name)
            .spawn(move || worker_service.worker(worker_id, rx, worker_running))
        {
            Ok(worker) => worker,
            Err(error) => {
                self.inner.lock().unwrap().devices.remove(&device_id);
                return Err(error.into());
            }
        };
        runtimes.insert(
            device_id.clone(),
            DeviceRuntime {
                descriptor: descriptor.clone(),
                tx,
            },
        );
        self.device_workers.lock().unwrap().push(worker);
        eprintln!(
            "discovered device {device_id} at {}",
            descriptor
                .transport
                .address
                .as_deref()
                .unwrap_or("<unknown>")
        );
        Ok(())
    }

    fn cached_devices(&self) -> DevicesResponse {
        let mut devices: Vec<_> = self
            .devices
            .lock()
            .unwrap()
            .values()
            .map(|runtime| runtime.descriptor.clone())
            .collect();
        let store = self.inner.lock().unwrap();
        for descriptor in &mut devices {
            descriptor.status = store.device(&descriptor.id).status;
        }
        devices.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        let cursors = devices
            .iter()
            .map(|descriptor| (descriptor.id.clone(), store.cursor(&descriptor.id)))
            .collect();
        DevicesResponse { devices, cursors }
    }

    fn refresh_device_blocking(&self, device_id: &DeviceId) -> ApiResult<DeviceDescriptor> {
        self.authorize_device(device_id)?;
        let mut device = self.backend.refresh(device_id).map_err(|error| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("device_refresh_failed: {error:#}"),
            )
        })?;
        if let Some(runtime) = self.devices.lock().unwrap().get_mut(device_id) {
            runtime.descriptor = device.clone();
        }
        let mut store = self.inner.lock().unwrap();
        let state = store.device_mut(device_id);
        state.status.availability = device.status.availability;
        device.status = state.status;
        Ok(device)
    }

    async fn refresh_device(&self, device_id: &DeviceId) -> ApiResult<DeviceDescriptor> {
        let service = self.clone();
        let device_id = device_id.clone();
        tokio::task::spawn_blocking(move || service.refresh_device_blocking(&device_id))
            .await
            .map_err(|error| {
                ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("device_refresh_task_failed: {error}"),
                )
            })?
    }

    fn replay(&self, identity: &RequestIdentity) -> ApiResult<Option<Operation>> {
        let store = self.inner.lock().unwrap();
        let Some(record) = store.idempotency.get(&identity.key) else {
            return Ok(None);
        };
        if record.fingerprint != identity.fingerprint {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "idempotency_key_conflict: key was already used for a different request".into(),
            ));
        }
        let operation = store
            .operations
            .iter()
            .find(|operation| operation.id == record.operation_id)
            .cloned()
            .expect("idempotency records share the operation retention boundary");
        Ok(Some(operation))
    }

    async fn submit(
        &self,
        request: DeviceRequest,
        action: Action,
        identity: RequestIdentity,
    ) -> ApiResult<Operation> {
        if !self.running.load(Ordering::Relaxed) {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "daemon shutting down".into(),
            ));
        }
        let runtime = self.runtime(&request.device_id)?;
        if let Some(operation) = self.replay(&identity)? {
            return Ok(operation);
        }
        let capability = match &action {
            Action::Probe => DeviceCapability::Probe,
            Action::Reset => DeviceCapability::Reset,
            Action::Monitor => DeviceCapability::Monitor,
            Action::Flash { .. } => DeviceCapability::Flash,
            Action::ReadFlash { .. } => DeviceCapability::ReadFlash,
            Action::EraseFlash { .. } => DeviceCapability::EraseFlash,
            Action::SerialWrite { .. } | Action::Application { .. } => {
                DeviceCapability::SerialWrite
            }
        };
        let device = if matches!(
            action,
            Action::SerialWrite { .. } | Action::Application { .. }
        ) && let Some(status) = self.active_monitor_status(&request.device_id)
        {
            let mut device = runtime.descriptor.clone();
            device.status = status;
            device
        } else {
            self.refresh_device(&request.device_id).await?
        };
        if !device.supports(capability) {
            return Err(ApiError::bad(format!(
                "unsupported_operation: device does not support {capability:?}"
            )));
        }
        match device.status.availability {
            DeviceAvailability::PermissionRequired => {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "device_permission_required".into(),
                ));
            }
            DeviceAvailability::Disconnected => {
                return Err(ApiError(StatusCode::CONFLICT, "device_disconnected".into()));
            }
            DeviceAvailability::IdentityMismatch => {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "device_identity_mismatch".into(),
                ));
            }
            DeviceAvailability::Ambiguous => {
                return Err(ApiError(StatusCode::CONFLICT, "device_ambiguous".into()));
            }
            DeviceAvailability::Available => {}
        }
        if request.monitor_baud == 0 {
            return Err(ApiError::bad("monitor baud must be positive"));
        }
        let mut store = self.inner.lock().unwrap();
        if let Some(record) = store.idempotency.get(&identity.key) {
            if record.fingerprint != identity.fingerprint {
                return Err(ApiError(
                    StatusCode::CONFLICT,
                    "idempotency_key_conflict: key was already used for a different request".into(),
                ));
            }
            return Ok(store
                .operations
                .iter()
                .find(|operation| operation.id == record.operation_id)
                .cloned()
                .expect("idempotency records share the operation retention boundary"));
        }
        let device_id = request.device_id.clone();
        let app_lane = matches!(
            action,
            Action::Application {
                command: Some(_),
                ..
            }
        );
        let input_lane = matches!(action, Action::SerialWrite { .. });
        let state = store.device_mut(&device_id);
        let compatible = (app_lane && state.input_pending && !state.application_pending)
            || (input_lane && state.application_pending && !state.input_pending);
        if state.busy && !compatible {
            return Err(ApiError(StatusCode::CONFLICT, "device_busy".into()));
        }
        state.busy = true;
        state.application_pending |= app_lane;
        state.input_pending |= input_lane;
        state.discard_monitor_read = !matches!(
            action,
            Action::SerialWrite { .. } | Action::Application { .. }
        );
        store.emit(&device_id, "operation_started", json!({}));
        let start_cursor = store.cursor(&device_id);
        let operation = Operation {
            id: format!("{}-{}-{}", store.epoch, device_id, start_cursor.after),
            device_id: device_id.clone(),
            request_key: identity.key.clone(),
            status: "running".into(),
            start_cursor,
            result: None,
            error: None,
            artifact: None,
        };
        store.push_operation(operation.clone());
        store.idempotency.insert(
            identity.key.clone(),
            IdempotencyRecord {
                fingerprint: identity.fingerprint,
                operation_id: operation.id.clone(),
            },
        );
        store.device_mut(&device_id).status.activity = DeviceActivity::Busy;
        if runtime
            .tx
            .try_send(Job {
                id: operation.id.clone(),
                request,
                action,
            })
            .is_err()
        {
            let state = store.device_mut(&device_id);
            if app_lane {
                state.application_pending = false;
            }
            if input_lane {
                state.input_pending = false;
            }
            state.busy = state.application_pending || state.input_pending;
            state.discard_monitor_read = false;
            state.status.activity = if state.busy {
                DeviceActivity::Busy
            } else {
                DeviceActivity::Error
            };
            store.operations.pop_back();
            store.idempotency.remove(&identity.key);
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "device worker unavailable".into(),
            ));
        }
        Ok(operation)
    }
    /// Embedded gateway entry point. Shares admission/ownership with HTTP.
    pub async fn submit_application(
        &self,
        request: crate::wire::ApplicationRequest,
        key: &str,
    ) -> Result<Operation> {
        self.submit_application_api(request, key)
            .await
            .map_err(|error| anyhow::anyhow!(error.1))
    }

    async fn submit_application_api(
        &self,
        request: crate::wire::ApplicationRequest,
        key: &str,
    ) -> ApiResult<Operation> {
        request
            .validate()
            .map_err(|e| ApiError::bad(e.to_string()))?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "idempotency-key",
            key.parse()
                .map_err(|_| ApiError::bad("invalid request key"))?,
        );
        let identity = request_identity(&headers, "application", &request)?;
        let device = DeviceRequest {
            device_id: request.device_id,
            monitor_baud: request.monitor_baud,
            no_reset_before: true,
        };
        self.submit(
            device,
            Action::Application {
                command: request.command,
                timeout_ms: request.timeout_ms,
            },
            identity,
        )
        .await
    }

    pub fn get_operation(&self, id: &str) -> Option<Operation> {
        self.inner
            .lock()
            .unwrap()
            .operations
            .iter()
            .find(|op| op.id == id)
            .cloned()
    }

    pub fn application_events(&self, device_id: &DeviceId, cursor: &Cursor) -> Result<EventBatch> {
        self.authorize_device(device_id)
            .map_err(|e| anyhow::anyhow!(e.1))?;
        self.inner
            .lock()
            .unwrap()
            .events_after(device_id, cursor)
            .map_err(|e| anyhow::anyhow!(e.1))
    }

    fn application_output(
        &self,
        device_id: &DeviceId,
        decoder: &mut LineDecoder,
        output: crate::application::Output,
    ) {
        use crate::application::Output;
        let mut store = self.inner.lock().unwrap();
        if store.device(device_id).discard_monitor_read {
            return;
        }
        match output {
            Output::Console(bytes) => {
                store.emit(
                    device_id,
                    "raw",
                    json!({"base64":BASE64_STANDARD.encode(&bytes)}),
                );
                for record in decoder.feed(&bytes) {
                    store.emit(device_id, "log", serde_json::to_value(record).unwrap());
                }
            }
            Output::Event(value) => store.emit(device_id, "application_event", value),
            Output::CorruptFrame => store.emit(
                device_id,
                "application_protocol_error",
                crate::application::corruption_event(),
            ),
        }
    }

    fn emit(&self, device_id: &DeviceId, kind: &str, data: Value) {
        self.inner.lock().unwrap().emit(device_id, kind, data);
    }

    fn begin_monitor_reconnect(
        &self,
        device_id: &DeviceId,
        baud: u32,
        reason: &str,
    ) -> MonitorReconnect {
        let mut store = self.inner.lock().unwrap();
        let state = store.device_mut(device_id);
        state.status.availability = DeviceAvailability::Disconnected;
        state.status.activity = DeviceActivity::Reconnecting;
        store.emit(
            device_id,
            "reconnecting",
            json!({"reason": reason, "monitor_baud": baud}),
        );
        MonitorReconnect::now(baud)
    }

    fn finish_monitor_reconnect(&self, device_id: &DeviceId, baud: u32, attempts: u64) {
        let mut store = self.inner.lock().unwrap();
        let state = store.device_mut(device_id);
        state.status.availability = DeviceAvailability::Available;
        state.status.activity = DeviceActivity::Monitoring;
        store.emit(
            device_id,
            "reconnected",
            json!({"attempts": attempts, "monitor_baud": baud}),
        );
    }

    fn reopens_after_boot_entry(&self, device_id: &DeviceId) -> bool {
        self.devices
            .lock()
            .unwrap()
            .get(device_id)
            .and_then(|runtime| {
                runtime
                    .descriptor
                    .transport
                    .metadata
                    .get("reopen_after_boot_entry")
            })
            .is_some_and(|value| value == "true")
    }

    fn active_monitor_status(&self, device_id: &DeviceId) -> Option<DeviceStatus> {
        let store = self.inner.lock().unwrap();
        let state = store.device(device_id);
        (state.status.availability == DeviceAvailability::Available
            && (state.status.activity == DeviceActivity::Monitoring
                || state.application_pending
                || state.input_pending))
            .then_some(state.status)
    }

    fn finish_stream(
        &self,
        device_id: &DeviceId,
        done: Vec<crate::duplex::Completion>,
        has_reader: bool,
    ) {
        if done.is_empty() {
            return;
        }
        let mut store = self.inner.lock().unwrap();
        for completion in done {
            let op = store
                .operations
                .iter_mut()
                .find(|op| op.id == completion.key)
                .expect("running operations are retained");
            match completion.result {
                Ok(value) => {
                    op.status = "succeeded".into();
                    op.result = Some(value);
                }
                Err(error) => {
                    op.status = "failed".into();
                    op.error = Some(format!("{error:#}"));
                }
            }
            let op = op.clone();
            let state = store.device_mut(device_id);
            match completion.lane {
                crate::duplex::Lane::Application => state.application_pending = false,
                crate::duplex::Lane::Input => state.input_pending = false,
            }
            state.busy = state.application_pending || state.input_pending;
            state.discard_monitor_read = false;
            state.status.activity = if state.busy {
                DeviceActivity::Busy
            } else if has_reader {
                DeviceActivity::Monitoring
            } else if state.status.activity == DeviceActivity::Reconnecting {
                DeviceActivity::Reconnecting
            } else {
                DeviceActivity::Error
            };
            store.emit(
                device_id,
                "operation_finished",
                serde_json::to_value(op).unwrap(),
            );
        }
    }

    fn worker(&self, device_id: DeviceId, rx: mpsc::Receiver<Job>, running: Arc<AtomicBool>) {
        let mut reader: Option<Box<dyn SerialIo>> = None;
        let mut reader_baud = None;
        let mut duplex = crate::duplex::Duplex::default();
        let mut reconnect: Option<MonitorReconnect> = None;
        let mut boot_reopen_at = None;
        let mut decoder = LineDecoder::default();
        let mut buffer = [0; 4096];
        loop {
            if reader.is_none() && (duplex.session.is_some() || duplex.has_pending()) {
                let had_session = duplex.session.is_some();
                let done = duplex.abort("transport closed; outcome unknown (not retried)");
                self.finish_stream(&device_id, done, false);
                if had_session {
                    self.emit(
                        &device_id,
                        "application_disconnected",
                        json!({"reason":"transport closed"}),
                    );
                }
            }
            if !running.load(Ordering::Relaxed)
                && !self.inner.lock().unwrap().device(&device_id).busy
            {
                break;
            }
            match rx.try_recv() {
                Ok(job) => {
                    if matches!(
                        job.action,
                        Action::Application { .. } | Action::SerialWrite { .. }
                    ) {
                        let connecting =
                            matches!(job.action, Action::Application { command: None, .. });
                        let lane = if matches!(job.action, Action::SerialWrite { .. }) {
                            crate::duplex::Lane::Input
                        } else {
                            crate::duplex::Lane::Application
                        };
                        let result = (|| -> Result<()> {
                            let baud = match &job.action {
                                Action::SerialWrite { baud, .. } => *baud,
                                _ => job.request.monitor_baud,
                            };
                            ensure!(
                                !duplex.has_pending() || reader_baud == Some(baud),
                                "active stream uses a different baud"
                            );
                            ensure!(
                                duplex.session.is_none() || connecting || reader_baud == Some(baud),
                                "active application uses a different baud"
                            );
                            if reader.is_none() || reader_baud != Some(baud) {
                                if matches!(
                                    job.action,
                                    Action::Application {
                                        command: Some(_),
                                        ..
                                    }
                                ) {
                                    anyhow::bail!(
                                        "application not connected; use app-connect first"
                                    );
                                }
                                let device = self
                                    .refresh_device_blocking(&device_id)
                                    .map_err(|e| anyhow::anyhow!(e.1))?;
                                ensure!(
                                    device.status.availability == DeviceAvailability::Available,
                                    "device is unavailable: {:?}",
                                    device.status.availability
                                );
                                reader = None;
                                reader_baud = None;
                                reader = Some(
                                    self.backend
                                        .open(&device_id, true)?
                                        .into_monitor(baud, false)?,
                                );
                                reader_baud = Some(baud);
                                decoder = LineDecoder::default();
                            }
                            match &job.action {
                                Action::Application {
                                    command,
                                    timeout_ms,
                                } => {
                                    if connecting && duplex.session.is_some() {
                                        self.emit(
                                            &device_id,
                                            "application_disconnected",
                                            json!({"reason":"renegotiating"}),
                                        );
                                    }
                                    duplex.start_application(
                                        job.id.clone(),
                                        command.as_ref(),
                                        Duration::from_millis(*timeout_ms),
                                    )?;
                                }
                                Action::SerialWrite {
                                    data,
                                    sha256,
                                    baud,
                                    timeout_ms,
                                } => {
                                    duplex.start_input(
                                        job.id.clone(),
                                        data,
                                        Duration::from_millis(*timeout_ms),
                                        json!({"written":data.len(),"sha256":sha256,"baud":baud}),
                                    )?;
                                }
                                _ => unreachable!(),
                            }
                            Ok(())
                        })();
                        if let Err(error) = result {
                            self.finish_stream(
                                &device_id,
                                vec![crate::duplex::Completion {
                                    key: job.id,
                                    lane,
                                    result: Err(error),
                                }],
                                reader.is_some(),
                            );
                        } else {
                            reconnect = None;
                            boot_reopen_at = None;
                        }
                        continue;
                    }
                    reconnect = None;
                    boot_reopen_at = None;
                    let preserves_monitor =
                        matches!(job.action, Action::Monitor) && duplex.session.is_some();
                    if !preserves_monitor {
                        if duplex.session.take().is_some() {
                            self.emit(
                                &device_id,
                                "application_disconnected",
                                json!({"reason":"hardware operation"}),
                            );
                        }
                        // Only this worker owns/open/closes this device. Drop the
                        // monitor before bootloader access, never in an HTTP task.
                        reader = None;
                        reader_baud = None;
                        decoder = LineDecoder::default();
                    }
                    let artifact_path = matches!(job.action, Action::ReadFlash { .. })
                        .then(|| self.artifact_dir.path().join(format!("{}.bin", job.id)));
                    let mut completed_artifact = None;
                    let result = (|| -> Result<Value> {
                        let reuses_monitor = preserves_monitor
                            && reader.is_some()
                            && reader_baud == Some(job.request.monitor_baud);
                        if !reuses_monitor {
                            let device = self
                                .refresh_device_blocking(&device_id)
                                .map_err(|error| anyhow::anyhow!(error.1))?;
                            ensure!(
                                device.status.availability == DeviceAvailability::Available,
                                "device is unavailable: {:?}",
                                device.status.availability
                            );
                        }
                        if matches!(job.action, Action::Monitor) && duplex.session.is_some() {
                            ensure!(
                                reader_baud == Some(job.request.monitor_baud),
                                "active application uses a different baud"
                            );
                            return Ok(json!({"monitor":true,"application":true}));
                        }
                        let mut session =
                            self.backend.open(&device_id, job.request.no_reset_before)?;
                        let result = match &job.action {
                            Action::Probe => serde_json::to_value(session.probe()?)?,
                            Action::Flash {
                                plan,
                                baud,
                                warnings,
                            } => {
                                let info = session.flash(plan, *baud, &mut |progress| {
                                    self.emit(
                                        &device_id,
                                        "progress",
                                        serde_json::to_value(progress).unwrap(),
                                    )
                                })?;
                                self.emit(
                                    &device_id,
                                    "flash_verified",
                                    serde_json::to_value(&info)?,
                                );
                                let mut result = serde_json::to_value(info)?;
                                if !warnings.is_empty() {
                                    result
                                        .as_object_mut()
                                        .context("flash result must be a JSON object")?
                                        .insert("warnings".into(), serde_json::to_value(warnings)?);
                                }
                                result
                            }
                            Action::ReadFlash { offset, size, baud } => {
                                let path = artifact_path
                                    .as_ref()
                                    .context("missing read artifact path")?;
                                let mut output = std::fs::OpenOptions::new()
                                    .write(true)
                                    .create_new(true)
                                    .open(path)?;
                                let info =
                                    session.read_flash(*offset, *size, *baud, &mut output)?;
                                output.flush()?;
                                drop(output);
                                let metadata = inspect_artifact(path)?;
                                ensure!(
                                    metadata.size == u64::from(*size),
                                    "read artifact size mismatch"
                                );
                                completed_artifact = Some(ArtifactRecord {
                                    metadata,
                                    path: path.clone(),
                                });
                                serde_json::to_value(info)?
                            }
                            Action::EraseFlash { baud } => {
                                let info = session.erase_flash(*baud)?;
                                self.emit(&device_id, "flash_erased", serde_json::to_value(&info)?);
                                return Ok(json!({
                                    "erased": true,
                                    "post_state": "download_mode",
                                    "board": info,
                                }));
                            }
                            Action::Reset => json!({"reset": true}),
                            Action::Monitor => json!({"monitor": true}),
                            Action::SerialWrite { .. } | Action::Application { .. } => {
                                unreachable!()
                            }
                        };
                        let reset = !matches!(job.action, Action::Monitor);
                        reader = Some(
                            session
                                .into_monitor(job.request.monitor_baud, reset)
                                .context("operation finished but monitor handoff failed")?,
                        );
                        reader_baud = Some(job.request.monitor_baud);
                        Ok(result)
                    })();
                    let mut store = self.inner.lock().unwrap();
                    let succeeded = result.is_ok();
                    if !succeeded && let Some(path) = &artifact_path {
                        let _ = std::fs::remove_file(path);
                    }
                    if succeeded && let Some(artifact) = completed_artifact.clone() {
                        store.artifacts.insert(job.id.clone(), artifact);
                    }
                    let op = store
                        .operations
                        .iter_mut()
                        .find(|op| op.id == job.id)
                        .unwrap();
                    match result {
                        Ok(result) => {
                            op.status = "succeeded".into();
                            op.result = Some(result);
                            op.artifact = completed_artifact
                                .as_ref()
                                .map(|artifact| artifact.metadata.clone());
                        }
                        Err(error) => {
                            op.status = "failed".into();
                            op.error = Some(format!("{error:#}"));
                        }
                    }
                    let op = op.clone();
                    let state = store.device_mut(&device_id);
                    state.busy = false;
                    state.discard_monitor_read = false;
                    state.status.activity = if reader.is_some() {
                        DeviceActivity::Monitoring
                    } else if succeeded {
                        DeviceActivity::Idle
                    } else {
                        DeviceActivity::Error
                    };
                    store.emit(
                        &device_id,
                        "operation_finished",
                        serde_json::to_value(op).unwrap(),
                    );
                }
                Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {
                    if let Some(serial) = &mut reader {
                        match duplex.tick(serial.as_mut()) {
                            Ok(done) => self.finish_stream(&device_id, done, true),
                            Err(error) => {
                                let had_session = duplex.session.is_some();
                                let done = duplex.abort(&format!("{error:#}"));
                                reader = None;
                                reader_baud = None;
                                decoder = LineDecoder::default();
                                self.finish_stream(&device_id, done, false);
                                if had_session {
                                    self.emit(
                                        &device_id,
                                        "application_disconnected",
                                        json!({"reason":error.to_string()}),
                                    );
                                }
                                continue;
                            }
                        }
                    }
                    if boot_reopen_at.is_some_and(|deadline| Instant::now() >= deadline) {
                        reader = None;
                        boot_reopen_at = None;
                        if let Some(baud) = reader_baud.take() {
                            decoder = LineDecoder::default();
                            reconnect = Some(self.begin_monitor_reconnect(
                                &device_id,
                                baud,
                                "native USB boot handoff",
                            ));
                        }
                        continue;
                    }
                    if let Some(serial) = &mut reader {
                        match serial.read(&mut buffer) {
                            Ok(0) => {
                                reader = None;
                                if let Some(baud) = reader_baud.take() {
                                    decoder = LineDecoder::default();
                                    reconnect = Some(self.begin_monitor_reconnect(
                                        &device_id,
                                        baud,
                                        "serial stream closed",
                                    ));
                                }
                            }
                            Ok(size) => {
                                boot_reopen_at = None;
                                if duplex.session.is_some() {
                                    let connecting = duplex.connecting();
                                    match duplex.feed(&buffer[..size], &mut |output| {
                                        self.application_output(&device_id, &mut decoder, output)
                                    }) {
                                        Ok(done) => {
                                            if connecting
                                                && duplex
                                                    .session
                                                    .as_ref()
                                                    .is_some_and(|s| s.is_valid())
                                            {
                                                self.emit(
                                                    &device_id,
                                                    "application_connected",
                                                    duplex.session.as_ref().unwrap().info.clone(),
                                                );
                                            }
                                            self.finish_stream(&device_id, done, true);
                                        }
                                        Err(error) => {
                                            let done = duplex.abort(&format!("{error:#}"));
                                            self.finish_stream(&device_id, done, true);
                                            self.emit(
                                                &device_id,
                                                "application_disconnected",
                                                json!({"reason":error.to_string()}),
                                            );
                                        }
                                    }
                                    continue;
                                }
                                let reopen_after_boot_entry =
                                    self.reopens_after_boot_entry(&device_id);
                                // Admission and log publication share this lock.
                                // A monitor-replacing operation invalidates this
                                // old read. Serial-write keeps the same stream.
                                let mut store = self.inner.lock().unwrap();
                                if store.device(&device_id).discard_monitor_read {
                                    continue;
                                }
                                store.emit(
                                    &device_id,
                                    "raw",
                                    json!({"base64": BASE64_STANDARD.encode(&buffer[..size])}),
                                );
                                let records = decoder.feed(&buffer[..size]);
                                let reopen_after_entry = reopen_after_boot_entry
                                    && records.last().is_some_and(|record| {
                                        record.text.starts_with("entry 0x")
                                            && decoder.pending_text().is_empty()
                                    });
                                for record in records {
                                    store.emit(
                                        &device_id,
                                        "log",
                                        serde_json::to_value(record).unwrap(),
                                    );
                                }
                                if reopen_after_entry {
                                    boot_reopen_at =
                                        Some(Instant::now() + NATIVE_USB_BOOT_REOPEN_DELAY);
                                }
                            }
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    io::ErrorKind::TimedOut
                                        | io::ErrorKind::WouldBlock
                                        | io::ErrorKind::Interrupted
                                ) => {}
                            Err(error) => {
                                reader = None;
                                boot_reopen_at = None;
                                if let Some(baud) = reader_baud.take() {
                                    decoder = LineDecoder::default();
                                    reconnect = Some(self.begin_monitor_reconnect(
                                        &device_id,
                                        baud,
                                        &error.to_string(),
                                    ));
                                }
                            }
                        }
                    } else if reconnect
                        .as_ref()
                        .is_some_and(|state| Instant::now() >= state.next_attempt)
                    {
                        let state = reconnect.as_ref().unwrap();
                        let baud = state.baud;
                        let attempts = state.attempts;
                        let reopened = (|| -> Result<Box<dyn SerialIo>> {
                            let device = self
                                .refresh_device_blocking(&device_id)
                                .map_err(|error| anyhow::anyhow!(error.1))?;
                            ensure!(
                                device.status.availability == DeviceAvailability::Available,
                                "device is unavailable: {:?}",
                                device.status.availability
                            );
                            self.backend
                                .open(&device_id, true)?
                                .into_monitor(baud, false)
                        })();
                        match reopened {
                            Ok(serial) => {
                                reader = Some(serial);
                                reader_baud = Some(baud);
                                reconnect = None;
                                boot_reopen_at = None;
                                decoder = LineDecoder::default();
                                self.finish_monitor_reconnect(&device_id, baud, attempts + 1);
                            }
                            Err(_) => reconnect.as_mut().unwrap().retry_later(),
                        }
                    } else {
                        thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
struct ApiError(StatusCode, String);
type ApiResult<T> = std::result::Result<T, ApiError>;
impl ApiError {
    fn bad(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

async fn auth(
    State(service): State<Service>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if let Some(token) = &service.token {
        let valid = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some(format!("Bearer {token}").as_str());
        if !valid {
            return ApiError(StatusCode::UNAUTHORIZED, "unauthorized".into()).into_response();
        }
    }
    next.run(request).await
}

pub fn router(service: Service) -> Router {
    Router::new()
        .route("/v1/devices", get(devices))
        .route("/v1/flash", post(flash))
        .route("/v1/erase-flash", post(erase_flash))
        .route("/v1/read-flash", post(read_flash))
        .route("/v1/serial-write", post(serial_write))
        .route("/v1/application", post(application_request))
        .route("/v1/probe", post(probe))
        .route("/v1/reset", post(reset))
        .route("/v1/monitor", post(monitor))
        .route("/v1/operations/{id}", get(operation))
        .route("/v1/operations/{id}/artifact", get(operation_artifact))
        .route("/v1/events", get(events))
        .route("/v1/logs", get(logs))
        .route("/v1/stream", get(stream))
        .route("/v1/wait", post(wait))
        .layer(DefaultBodyLimit::max(48 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(service.clone(), auth))
        .with_state(service)
}

async fn devices(State(service): State<Service>) -> ApiResult<Json<DevicesResponse>> {
    if service.discovering {
        return Ok(Json(service.cached_devices()));
    }
    let device_ids: Vec<_> = service.devices.lock().unwrap().keys().cloned().collect();
    let mut devices = Vec::with_capacity(device_ids.len());
    for device_id in &device_ids {
        devices.push(service.refresh_device(device_id).await?);
    }
    devices.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
    let store = service.inner.lock().unwrap();
    Ok(Json(DevicesResponse {
        devices,
        cursors: device_ids
            .into_iter()
            .map(|device_id| {
                let cursor = store.cursor(&device_id);
                (device_id, cursor)
            })
            .collect(),
    }))
}

async fn read_multipart_field(mut field: Field<'_>, limit: usize) -> ApiResult<Vec<u8>> {
    let mut data = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|error| ApiError::bad(format!("invalid multipart body: {error}")))?
    {
        if data.len().saturating_add(chunk.len()) > limit {
            return Err(ApiError::bad(format!(
                "multipart field exceeds {limit} bytes"
            )));
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

async fn flash(
    State(service): State<Service>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> ApiResult<(StatusCode, Json<Operation>)> {
    let metadata_field = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad(format!("invalid multipart body: {error}")))?
        .ok_or_else(|| ApiError::bad("missing metadata part"))?;
    if metadata_field.name() != Some("metadata") {
        return Err(ApiError::bad("metadata must be the first multipart part"));
    }
    let metadata = read_multipart_field(metadata_field, MAX_FLASH_METADATA_BYTES).await?;
    let upload: FlashUploadManifest = serde_json::from_slice(&metadata)
        .map_err(|error| ApiError::bad(format!("invalid flash metadata: {error}")))?;
    upload
        .validate()
        .map_err(|error| ApiError::bad(format!("{error:#}")))?;
    service.authorize_device(&upload.device_id)?;
    let identity = request_identity(&headers, "flash", &upload)?;

    let staging = tempfile::Builder::new()
        .prefix("upload-")
        .tempdir_in(service.artifact_dir.path())
        .map_err(|error| ApiError::bad(format!("create upload staging: {error}")))?;
    let mut paths = vec![None; upload.segments.len()];
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad(format!("invalid multipart body: {error}")))?
    {
        let name = field
            .name()
            .ok_or_else(|| ApiError::bad("multipart artifact is missing a part name"))?
            .to_owned();
        let index = upload
            .segments
            .iter()
            .position(|segment| segment.part == name)
            .ok_or_else(|| ApiError::bad(format!("unknown artifact part: {name}")))?;
        if paths[index].is_some() {
            return Err(ApiError::bad(format!("duplicate artifact part: {name}")));
        }
        let expected = &upload.segments[index];
        let path = staging.path().join(format!("segment-{index}.bin"));
        let mut output = tokio::fs::File::create(&path)
            .await
            .map_err(|error| ApiError::bad(format!("create artifact staging: {error}")))?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|error| ApiError::bad(format!("invalid multipart body: {error}")))?
        {
            size = size
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| ApiError::bad("artifact size overflow"))?;
            if size > expected.size {
                return Err(ApiError::bad(format!(
                    "artifact size exceeds declaration for {name}"
                )));
            }
            output
                .write_all(&chunk)
                .await
                .map_err(|error| ApiError::bad(format!("stage artifact: {error}")))?;
            hasher.update(&chunk);
        }
        output
            .flush()
            .await
            .map_err(|error| ApiError::bad(format!("flush artifact staging: {error}")))?;
        if size != expected.size {
            return Err(ApiError::bad(format!(
                "artifact size mismatch for {name}: expected {}, received {size}",
                expected.size
            )));
        }
        let digest: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if digest != expected.sha256 {
            return Err(ApiError::bad(format!(
                "artifact SHA256 mismatch for {name}"
            )));
        }
        paths[index] = Some(path);
    }
    if let Some(missing) = paths
        .iter()
        .position(Option::is_none)
        .map(|index| upload.segments[index].part.clone())
    {
        return Err(ApiError::bad(format!("missing artifact part: {missing}")));
    }

    let backend = service.backend.clone();
    let (request, plan, baud, warnings) = tokio::task::spawn_blocking(move || -> Result<_> {
        let _staging = staging;
        let data = paths
            .into_iter()
            .map(|path| std::fs::read(path.expect("all artifact parts were checked")))
            .collect::<io::Result<Vec<_>>>()?;
        let plan = upload.into_plan(data)?;
        backend.validate(&plan)?;
        let request = DeviceRequest {
            device_id: upload.device_id,
            monitor_baud: upload.monitor_baud,
            no_reset_before: upload.no_reset_before,
        };
        Ok((request, plan, upload.flash_baud, upload.warnings))
    })
    .await
    .map_err(|error| ApiError::bad(error.to_string()))?
    .map_err(|error| ApiError::bad(format!("{error:#}")))?;
    Ok((
        StatusCode::ACCEPTED,
        Json(
            service
                .submit(
                    request,
                    Action::Flash {
                        plan,
                        baud,
                        warnings,
                    },
                    identity,
                )
                .await?,
        ),
    ))
}
async fn erase_flash(
    State(service): State<Service>,
    headers: HeaderMap,
    Json(request): Json<EraseFlashRequest>,
) -> ApiResult<(StatusCode, Json<Operation>)> {
    request
        .validate()
        .map_err(|error| ApiError::bad(format!("{error:#}")))?;
    let identity = request_identity(&headers, "erase-flash", &request)?;
    let device_request = request.device_request();
    Ok((
        StatusCode::ACCEPTED,
        Json(
            service
                .submit(
                    device_request,
                    Action::EraseFlash {
                        baud: request.flash_baud,
                    },
                    identity,
                )
                .await?,
        ),
    ))
}
async fn read_flash(
    State(service): State<Service>,
    headers: HeaderMap,
    Json(request): Json<ReadFlashRequest>,
) -> ApiResult<(StatusCode, Json<Operation>)> {
    request
        .validate()
        .map_err(|error| ApiError::bad(format!("{error:#}")))?;
    let identity = request_identity(&headers, "read-flash", &request)?;
    let device_request = request.device_request();
    let action = Action::ReadFlash {
        offset: request.offset,
        size: request.size,
        baud: request.flash_baud,
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(service.submit(device_request, action, identity).await?),
    ))
}
async fn application_request(
    State(service): State<Service>,
    headers: HeaderMap,
    Json(request): Json<crate::wire::ApplicationRequest>,
) -> ApiResult<(StatusCode, Json<Operation>)> {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::bad("missing Idempotency-Key header"))?;
    Ok((
        StatusCode::ACCEPTED,
        Json(service.submit_application_api(request, key).await?),
    ))
}

async fn serial_write(
    State(service): State<Service>,
    headers: HeaderMap,
    Json(request): Json<SerialWriteRequest>,
) -> ApiResult<(StatusCode, Json<Operation>)> {
    service.authorize_device(&request.device_id)?;
    let identity = request_identity(&headers, "serial-write", &request)?;
    let data = request
        .decode()
        .map_err(|error| ApiError::bad(format!("{error:#}")))?;
    let device_request = request.device_request();
    let action = Action::SerialWrite {
        data,
        sha256: request.sha256,
        baud: request.baud,
        timeout_ms: request.timeout_ms,
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(service.submit(device_request, action, identity).await?),
    ))
}
async fn probe(
    State(service): State<Service>,
    headers: HeaderMap,
    Json(request): Json<DeviceRequest>,
) -> ApiResult<(StatusCode, Json<Operation>)> {
    let identity = request_identity(&headers, "probe", &request)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(service.submit(request, Action::Probe, identity).await?),
    ))
}
async fn reset(
    State(service): State<Service>,
    headers: HeaderMap,
    Json(request): Json<DeviceRequest>,
) -> ApiResult<(StatusCode, Json<Operation>)> {
    let identity = request_identity(&headers, "reset", &request)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(service.submit(request, Action::Reset, identity).await?),
    ))
}
async fn monitor(
    State(service): State<Service>,
    headers: HeaderMap,
    Json(request): Json<DeviceRequest>,
) -> ApiResult<(StatusCode, Json<Operation>)> {
    let identity = request_identity(&headers, "monitor", &request)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(service.submit(request, Action::Monitor, identity).await?),
    ))
}
async fn operation(
    State(service): State<Service>,
    Path(id): Path<String>,
) -> ApiResult<Json<Operation>> {
    service
        .inner
        .lock()
        .unwrap()
        .operations
        .iter()
        .find(|op| op.id == id)
        .cloned()
        .map(Json)
        .ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                "operation not found (expired or daemon restarted)".into(),
            )
        })
}
async fn operation_artifact(
    State(service): State<Service>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let artifact = service
        .inner
        .lock()
        .unwrap()
        .artifacts
        .get(&id)
        .cloned()
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "operation artifact not found".into()))?;
    let mut file = tokio::fs::File::open(&artifact.path)
        .await
        .map_err(|error| {
            ApiError(
                StatusCode::NOT_FOUND,
                format!("operation artifact unavailable: {error}"),
            )
        })?;
    let stream = async_stream::stream! {
        use tokio::io::AsyncReadExt;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            match file.read(&mut buffer).await {
                Ok(0) => break,
                Ok(read) => yield Ok::<Bytes, io::Error>(Bytes::copy_from_slice(&buffer[..read])),
                Err(error) => {
                    yield Err(error);
                    break;
                }
            }
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, &artifact.metadata.content_type)
        .header(header::CONTENT_LENGTH, artifact.metadata.size)
        .header(header::CACHE_CONTROL, "no-store")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{id}.bin\""),
        )
        .header("x-idf-remote-sha256", &artifact.metadata.sha256)
        .body(Body::from_stream(stream))
        .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}
async fn events(
    State(service): State<Service>,
    Query(query): Query<EventPollQuery>,
) -> ApiResult<Json<EventBatch>> {
    service.authorize_device(&query.device_id)?;
    if query.wait_ms > 1000 {
        return Err(ApiError::bad("event wait must be 0..1000 ms"));
    }
    let cursor = Cursor {
        epoch: query.epoch,
        after: query.after,
    };
    let changed = service
        .inner
        .lock()
        .unwrap()
        .device(&query.device_id)
        .changed
        .clone();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(query.wait_ms);
    loop {
        // Register before checking the ring so publication cannot fall between
        // the empty check and the wait. Every subscriber is notified.
        let notified = changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let batch = service
            .inner
            .lock()
            .unwrap()
            .events_after(&query.device_id, &cursor)?;
        if !batch.events.is_empty()
            || tokio::time::Instant::now() >= deadline
            || !service.running.load(Ordering::Relaxed)
        {
            return Ok(Json(batch));
        }
        let _ = tokio::time::timeout_at(deadline, notified).await;
    }
}

#[derive(serde::Deserialize)]
struct EventPollQuery {
    device_id: DeviceId,
    epoch: String,
    after: u64,
    #[serde(default)]
    wait_ms: u64,
}
async fn logs(
    State(service): State<Service>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Json<EventBatch>> {
    service.authorize_device(&query.device_id)?;
    let cursor = query.cursor();
    let mut batch = service
        .inner
        .lock()
        .unwrap()
        .events_after(&query.device_id, &cursor)?;
    batch.events.retain(|event| event.kind == "log");
    Ok(Json(batch))
}
async fn wait(
    State(service): State<Service>,
    Json(request): Json<WaitRequest>,
) -> ApiResult<Json<Event>> {
    service.authorize_device(&request.device_id)?;
    if request.timeout_ms == 0 || request.timeout_ms > 300000 || request.pattern.len() > 4096 {
        return Err(ApiError::bad("invalid wait bounds"));
    }
    let pattern = Regex::new(&request.pattern).map_err(|error| ApiError::bad(error.to_string()))?;
    if pattern.is_match("") {
        return Err(ApiError::bad("wait pattern must not match empty output"));
    }
    let mut cursor = request.cursor;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(request.timeout_ms);
    loop {
        if !service.running.load(Ordering::Relaxed) {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "daemon shutting down".into(),
            ));
        }
        let batch = service
            .inner
            .lock()
            .unwrap()
            .events_after(&request.device_id, &cursor)?;
        for event in batch.events {
            if event.kind == "log"
                && event.data["text"]
                    .as_str()
                    .is_some_and(|text| pattern.is_match(text))
            {
                return Ok(Json(event));
            }
        }
        cursor = batch.cursor;
        if tokio::time::Instant::now() >= deadline {
            return Err(ApiError(StatusCode::REQUEST_TIMEOUT, "wait_timeout".into()));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
async fn stream(State(service): State<Service>, Query(query): Query<EventQuery>) -> Response {
    if let Err(error) = service.authorize_device(&query.device_id) {
        return error.into_response();
    }
    let mut cursor = query.cursor();
    if let Err(error) = service
        .inner
        .lock()
        .unwrap()
        .events_after(&query.device_id, &cursor)
    {
        return error.into_response();
    }
    let output = async_stream::stream! {
        while service.running.load(Ordering::Relaxed) {
            let batch = service
                .inner
                .lock()
                .unwrap()
                .events_after(&query.device_id, &cursor);
            match batch {
                Ok(batch) => {
                    for event in batch.events {
                        yield Ok::<_, Infallible>(SseEvent::default().id(event.seq.to_string()).event(event.kind.clone()).json_data(event).unwrap());
                    }
                    cursor = batch.cursor;
                }
                Err(error) => {
                    yield Ok(SseEvent::default().event("gap").data(error.1)); break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    Sse::new(output)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(1)))
        .into_response()
}

pub fn serve(
    bind: SocketAddr,
    backend: Arc<dyn DeviceBackend>,
    device: DeviceDescriptor,
    token: Option<String>,
    running: Arc<AtomicBool>,
) -> Result<()> {
    serve_many(bind, backend, vec![device], token, running)
}

pub fn serve_many(
    bind: SocketAddr,
    backend: Arc<dyn DeviceBackend>,
    devices: Vec<DeviceDescriptor>,
    token: Option<String>,
    running: Arc<AtomicBool>,
) -> Result<()> {
    ensure!(
        bind.ip().is_loopback() || token.as_ref().is_some_and(|token| !token.is_empty()),
        "non-loopback bind requires --token-file"
    );
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(bind).await?;
        let address = listener.local_addr()?;
        let device_ids = devices
            .iter()
            .map(|device| device.id.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let (service, worker) = Service::start_many(backend, devices, token, running.clone())?;
        eprintln!("idf-remote listening on http://{address}; allowed devices: {device_ids}");
        let shutdown = running.clone();
        let result = axum::serve(listener, router(service))
            .with_graceful_shutdown(async move {
                while shutdown.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await;
        running.store(false, Ordering::Relaxed);
        worker.join();
        result.context("HTTP server")
    })
}

pub fn serve_dynamic(
    bind: SocketAddr,
    backend: Arc<dyn DeviceBackend>,
    token: Option<String>,
    running: Arc<AtomicBool>,
) -> Result<()> {
    ensure!(
        bind.ip().is_loopback() || token.as_ref().is_some_and(|token| !token.is_empty()),
        "non-loopback bind requires --token-file"
    );
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(bind).await?;
        let address = listener.local_addr()?;
        let (service, worker) = Service::start_discovering(backend, token, running.clone())?;
        eprintln!("idf-remote listening on http://{address}; dynamically discovering USB devices");
        let shutdown = running.clone();
        let result = axum::serve(listener, router(service))
            .with_graceful_shutdown(async move {
                while shutdown.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await;
        running.store(false, Ordering::Relaxed);
        worker.join();
        result.context("HTTP server")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::DeviceSession;
    use crate::device::TransportDescriptor;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use std::collections::{BTreeMap, HashSet};
    use std::sync::{Condvar, atomic::AtomicUsize};
    use tower::ServiceExt;

    struct FailingBackend {
        gate: Mutex<mpsc::Receiver<()>>,
        changed: AtomicBool,
        availability: DeviceAvailability,
        capabilities: Vec<DeviceCapability>,
        open_calls: AtomicUsize,
        refresh_delay: Duration,
    }
    impl DeviceBackend for FailingBackend {
        fn devices(&self) -> Result<Vec<DeviceDescriptor>> {
            Ok(vec![self.refresh(&test_device_id())?])
        }
        fn refresh(&self, _: &DeviceId) -> Result<DeviceDescriptor> {
            thread::sleep(self.refresh_delay);
            let availability = if self.changed.load(Ordering::Relaxed) {
                DeviceAvailability::IdentityMismatch
            } else {
                self.availability
            };
            Ok(test_descriptor(availability, self.capabilities.clone()))
        }
        fn validate(&self, _: &PreparedPlan) -> Result<()> {
            Ok(())
        }
        fn open(&self, _: &DeviceId, _: bool) -> Result<Box<dyn crate::backend::DeviceSession>> {
            self.open_calls.fetch_add(1, Ordering::Relaxed);
            self.gate
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2))?;
            anyhow::bail!("injected open failure")
        }
    }
    fn test_device_id() -> DeviceId {
        DeviceId::new("dev_test").unwrap()
    }
    fn test_descriptor(
        availability: DeviceAvailability,
        capabilities: Vec<DeviceCapability>,
    ) -> DeviceDescriptor {
        descriptor_for(test_device_id(), "test-address", availability, capabilities)
    }

    fn descriptor_for(
        id: DeviceId,
        address: &str,
        availability: DeviceAvailability,
        capabilities: Vec<DeviceCapability>,
    ) -> DeviceDescriptor {
        DeviceDescriptor {
            id,
            display_name: "Test device".into(),
            transport: TransportDescriptor {
                kind: "test".into(),
                address: Some(address.into()),
                metadata: BTreeMap::from([("serial_number".into(), "expected".into())]),
            },
            status: DeviceStatus {
                availability,
                activity: DeviceActivity::Idle,
            },
            capabilities,
        }
    }
    fn all_capabilities() -> Vec<DeviceCapability> {
        vec![
            DeviceCapability::Probe,
            DeviceCapability::Flash,
            DeviceCapability::ReadFlash,
            DeviceCapability::EraseFlash,
            DeviceCapability::SerialWrite,
            DeviceCapability::Reset,
            DeviceCapability::Monitor,
        ]
    }

    struct ReadBackend {
        payload: Vec<u8>,
        open_calls: Arc<AtomicUsize>,
        erase_calls: Arc<AtomicUsize>,
        fail_monitor_handoff: bool,
    }
    impl DeviceBackend for ReadBackend {
        fn devices(&self) -> Result<Vec<DeviceDescriptor>> {
            Ok(vec![test_descriptor(
                DeviceAvailability::Available,
                all_capabilities(),
            )])
        }
        fn refresh(&self, _: &DeviceId) -> Result<DeviceDescriptor> {
            Ok(test_descriptor(
                DeviceAvailability::Available,
                all_capabilities(),
            ))
        }
        fn validate(&self, _: &PreparedPlan) -> Result<()> {
            Ok(())
        }
        fn open(&self, _: &DeviceId, _: bool) -> Result<Box<dyn crate::backend::DeviceSession>> {
            self.open_calls.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(ReadSession {
                payload: self.payload.clone(),
                erase_calls: self.erase_calls.clone(),
                fail_monitor_handoff: self.fail_monitor_handoff,
            }))
        }
    }

    struct ReadSession {
        payload: Vec<u8>,
        erase_calls: Arc<AtomicUsize>,
        fail_monitor_handoff: bool,
    }
    impl crate::backend::DeviceSession for ReadSession {
        fn probe(&mut self) -> Result<crate::backend::BoardInfo> {
            anyhow::bail!("unexpected probe")
        }
        fn flash(
            &mut self,
            _: &PreparedPlan,
            _: u32,
            _: &mut dyn FnMut(crate::backend::Progress),
        ) -> Result<crate::backend::BoardInfo> {
            Ok(crate::backend::BoardInfo {
                chip: "esp32s3".into(),
                flash_size_bytes: 4 * 1024 * 1024,
                revision: Some((0, 2)),
                mac_address: Some("00:11:22:33:44:55".into()),
            })
        }
        fn read_flash(
            &mut self,
            _: u32,
            size: u32,
            _: u32,
            output: &mut dyn Write,
        ) -> Result<crate::backend::BoardInfo> {
            ensure!(self.payload.len() == size as usize, "unexpected read size");
            output.write_all(&self.payload)?;
            Ok(crate::backend::BoardInfo {
                chip: "esp32s3".into(),
                flash_size_bytes: 4 * 1024 * 1024,
                revision: Some((0, 2)),
                mac_address: Some("00:11:22:33:44:55".into()),
            })
        }
        fn erase_flash(&mut self, _: u32) -> Result<crate::backend::BoardInfo> {
            self.erase_calls.fetch_add(1, Ordering::Relaxed);
            Ok(crate::backend::BoardInfo {
                chip: "esp32s3".into(),
                flash_size_bytes: 4 * 1024 * 1024,
                revision: Some((0, 2)),
                mac_address: Some("00:11:22:33:44:55".into()),
            })
        }
        fn into_monitor(
            self: Box<Self>,
            _: u32,
            _: bool,
        ) -> Result<Box<dyn crate::backend::SerialIo>> {
            ensure!(
                !self.fail_monitor_handoff,
                "injected monitor handoff failure"
            );
            Ok(Box::new(IdleSerial))
        }
    }

    struct IdleSerial;
    impl Read for IdleSerial {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::TimedOut))
        }
    }
    impl Write for IdleSerial {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct ParallelBackend {
        devices: HashMap<DeviceId, DeviceDescriptor>,
        started: Mutex<HashSet<DeviceId>>,
        started_changed: Condvar,
    }

    impl DeviceBackend for ParallelBackend {
        fn devices(&self) -> Result<Vec<DeviceDescriptor>> {
            Ok(self.devices.values().cloned().collect())
        }

        fn refresh(&self, device_id: &DeviceId) -> Result<DeviceDescriptor> {
            self.devices
                .get(device_id)
                .cloned()
                .context("unknown parallel test device")
        }

        fn validate(&self, _: &PreparedPlan) -> Result<()> {
            Ok(())
        }

        fn open(
            &self,
            device_id: &DeviceId,
            _: bool,
        ) -> Result<Box<dyn crate::backend::DeviceSession>> {
            let deadline = Instant::now() + Duration::from_secs(1);
            let mut started = self.started.lock().unwrap();
            started.insert(device_id.clone());
            self.started_changed.notify_all();
            while started.len() < self.devices.len() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                ensure!(
                    !remaining.is_zero(),
                    "device workers did not run concurrently"
                );
                let (next, timeout) = self
                    .started_changed
                    .wait_timeout(started, remaining)
                    .unwrap();
                started = next;
                ensure!(
                    !timeout.timed_out(),
                    "device workers did not run concurrently"
                );
            }
            self.started_changed.notify_all();
            Ok(Box::new(ParallelSession))
        }
    }

    struct ParallelSession;

    impl DeviceSession for ParallelSession {
        fn probe(&mut self) -> Result<crate::backend::BoardInfo> {
            Ok(crate::backend::BoardInfo {
                chip: "esp32s3".into(),
                flash_size_bytes: 4 * 1024 * 1024,
                revision: Some((0, 2)),
                mac_address: None,
            })
        }

        fn flash(
            &mut self,
            _: &PreparedPlan,
            _: u32,
            _: &mut dyn FnMut(crate::backend::Progress),
        ) -> Result<crate::backend::BoardInfo> {
            anyhow::bail!("unexpected flash")
        }

        fn read_flash(
            &mut self,
            _: u32,
            _: u32,
            _: u32,
            _: &mut dyn Write,
        ) -> Result<crate::backend::BoardInfo> {
            anyhow::bail!("unexpected read-flash")
        }

        fn erase_flash(&mut self, _: u32) -> Result<crate::backend::BoardInfo> {
            anyhow::bail!("unexpected erase-flash")
        }

        fn into_monitor(
            self: Box<Self>,
            _: u32,
            _: bool,
        ) -> Result<Box<dyn crate::backend::SerialIo>> {
            Ok(Box::new(IdleSerial))
        }
    }

    struct DynamicBackend {
        visible: Mutex<Vec<DeviceDescriptor>>,
    }

    impl DeviceBackend for DynamicBackend {
        fn devices(&self) -> Result<Vec<DeviceDescriptor>> {
            Ok(self.visible.lock().unwrap().clone())
        }

        fn refresh(&self, device_id: &DeviceId) -> Result<DeviceDescriptor> {
            self.visible
                .lock()
                .unwrap()
                .iter()
                .find(|device| &device.id == device_id)
                .cloned()
                .context("dynamic test device is not visible")
        }

        fn validate(&self, _: &PreparedPlan) -> Result<()> {
            Ok(())
        }

        fn open(&self, _: &DeviceId, _: bool) -> Result<Box<dyn DeviceSession>> {
            anyhow::bail!("unexpected dynamic test open")
        }
    }

    struct SerialBackend {
        open_calls: Arc<AtomicUsize>,
        refresh_calls: Arc<AtomicUsize>,
        writes: Arc<Mutex<Vec<u8>>>,
        fail_write: Arc<AtomicBool>,
        read_gate: Option<Arc<ReadGate>>,
    }
    impl DeviceBackend for SerialBackend {
        fn devices(&self) -> Result<Vec<DeviceDescriptor>> {
            Ok(vec![test_descriptor(
                DeviceAvailability::Available,
                all_capabilities(),
            )])
        }
        fn refresh(&self, _: &DeviceId) -> Result<DeviceDescriptor> {
            self.refresh_calls.fetch_add(1, Ordering::Relaxed);
            Ok(test_descriptor(
                DeviceAvailability::Available,
                all_capabilities(),
            ))
        }
        fn validate(&self, _: &PreparedPlan) -> Result<()> {
            Ok(())
        }
        fn open(&self, _: &DeviceId, _: bool) -> Result<Box<dyn crate::backend::DeviceSession>> {
            self.open_calls.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(SerialSession {
                writes: self.writes.clone(),
                fail_write: self.fail_write.clone(),
                read_gate: self.read_gate.clone(),
            }))
        }
    }

    struct SerialSession {
        writes: Arc<Mutex<Vec<u8>>>,
        fail_write: Arc<AtomicBool>,
        read_gate: Option<Arc<ReadGate>>,
    }
    impl crate::backend::DeviceSession for SerialSession {
        fn probe(&mut self) -> Result<crate::backend::BoardInfo> {
            anyhow::bail!("unexpected probe")
        }
        fn flash(
            &mut self,
            _: &PreparedPlan,
            _: u32,
            _: &mut dyn FnMut(crate::backend::Progress),
        ) -> Result<crate::backend::BoardInfo> {
            anyhow::bail!("unexpected flash")
        }
        fn read_flash(
            &mut self,
            _: u32,
            _: u32,
            _: u32,
            _: &mut dyn Write,
        ) -> Result<crate::backend::BoardInfo> {
            anyhow::bail!("unexpected read-flash")
        }
        fn erase_flash(&mut self, _: u32) -> Result<crate::backend::BoardInfo> {
            anyhow::bail!("unexpected erase-flash")
        }
        fn into_monitor(
            self: Box<Self>,
            _: u32,
            reset: bool,
        ) -> Result<Box<dyn crate::backend::SerialIo>> {
            ensure!(!reset, "serial-write monitor must not reset");
            Ok(Box::new(SharedSerial {
                writes: self.writes,
                fail_write: self.fail_write,
                read_gate: self.read_gate,
            }))
        }
    }

    struct SharedSerial {
        writes: Arc<Mutex<Vec<u8>>>,
        fail_write: Arc<AtomicBool>,
        read_gate: Option<Arc<ReadGate>>,
    }
    struct ReadGate(Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>);
    impl Read for SharedSerial {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if let Some(gate) = &self.read_gate
                && let Some((started, release)) = gate.0.lock().unwrap().take()
            {
                started.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(2)).unwrap();
                buffer[..5].copy_from_slice(b"echo\n");
                return Ok(5);
            }
            thread::sleep(Duration::from_millis(1));
            Err(io::Error::from(io::ErrorKind::TimedOut))
        }
    }
    impl Write for SharedSerial {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if self.fail_write.load(Ordering::Relaxed) {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            self.writes.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct ReconnectBackend {
        open_modes: Arc<Mutex<Vec<bool>>>,
        monitor_resets: Arc<Mutex<Vec<bool>>>,
        disconnect_seen: Arc<AtomicBool>,
        reconnect_refreshes: AtomicUsize,
        disconnected_refreshes: usize,
        silent_boot_handoff: bool,
        continued_boot_output: bool,
    }

    impl DeviceBackend for ReconnectBackend {
        fn devices(&self) -> Result<Vec<DeviceDescriptor>> {
            Ok(vec![self.refresh(&test_device_id())?])
        }

        fn refresh(&self, _: &DeviceId) -> Result<DeviceDescriptor> {
            let reconnect_refresh = self.disconnect_seen.load(Ordering::Relaxed)
                && self.reconnect_refreshes.fetch_add(1, Ordering::Relaxed)
                    < self.disconnected_refreshes;
            let mut descriptor = test_descriptor(
                if reconnect_refresh {
                    DeviceAvailability::Disconnected
                } else {
                    DeviceAvailability::Available
                },
                all_capabilities(),
            );
            if self.silent_boot_handoff || self.continued_boot_output {
                descriptor
                    .transport
                    .metadata
                    .insert("reopen_after_boot_entry".into(), "true".into());
            }
            Ok(descriptor)
        }

        fn validate(&self, _: &PreparedPlan) -> Result<()> {
            Ok(())
        }

        fn open(&self, _: &DeviceId, no_reset_before: bool) -> Result<Box<dyn DeviceSession>> {
            let mut modes = self.open_modes.lock().unwrap();
            let first = modes.is_empty();
            modes.push(no_reset_before);
            Ok(Box::new(ReconnectSession {
                first,
                silent_boot_handoff: self.silent_boot_handoff,
                continued_boot_output: self.continued_boot_output,
                disconnect_seen: self.disconnect_seen.clone(),
                monitor_resets: self.monitor_resets.clone(),
            }))
        }
    }

    struct ReconnectSession {
        first: bool,
        silent_boot_handoff: bool,
        continued_boot_output: bool,
        disconnect_seen: Arc<AtomicBool>,
        monitor_resets: Arc<Mutex<Vec<bool>>>,
    }

    impl DeviceSession for ReconnectSession {
        fn probe(&mut self) -> Result<crate::backend::BoardInfo> {
            anyhow::bail!("unexpected probe")
        }

        fn flash(
            &mut self,
            _: &PreparedPlan,
            _: u32,
            _: &mut dyn FnMut(crate::backend::Progress),
        ) -> Result<crate::backend::BoardInfo> {
            anyhow::bail!("unexpected flash")
        }

        fn read_flash(
            &mut self,
            _: u32,
            _: u32,
            _: u32,
            _: &mut dyn Write,
        ) -> Result<crate::backend::BoardInfo> {
            anyhow::bail!("unexpected read-flash")
        }

        fn erase_flash(&mut self, _: u32) -> Result<crate::backend::BoardInfo> {
            anyhow::bail!("unexpected erase-flash")
        }

        fn into_monitor(self: Box<Self>, _: u32, reset: bool) -> Result<Box<dyn SerialIo>> {
            self.monitor_resets.lock().unwrap().push(reset);
            if self.first {
                if self.silent_boot_handoff {
                    Ok(Box::new(RecoverySerial::BootEntry { emitted: false }))
                } else if self.continued_boot_output {
                    Ok(Box::new(RecoverySerial::BootThenApp { stage: 0 }))
                } else {
                    Ok(Box::new(RecoverySerial::First {
                        disconnect_seen: self.disconnect_seen,
                    }))
                }
            } else {
                Ok(Box::new(RecoverySerial::Recovered { emitted: false }))
            }
        }
    }

    enum RecoverySerial {
        First { disconnect_seen: Arc<AtomicBool> },
        BootEntry { emitted: bool },
        BootThenApp { stage: u8 },
        Recovered { emitted: bool },
    }

    impl Read for RecoverySerial {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            match self {
                Self::First { disconnect_seen } => {
                    disconnect_seen.store(true, Ordering::Relaxed);
                    Err(io::Error::from(io::ErrorKind::BrokenPipe))
                }
                Self::BootEntry { emitted } if !*emitted => {
                    *emitted = true;
                    let message = b"entry 0x403c8908\n";
                    buffer[..message.len()].copy_from_slice(message);
                    Ok(message.len())
                }
                Self::BootEntry { .. } => {
                    thread::sleep(Duration::from_millis(5));
                    Err(io::Error::from(io::ErrorKind::TimedOut))
                }
                Self::BootThenApp { stage } if *stage == 0 => {
                    *stage = 1;
                    let message = b"entry 0x403c8908\n";
                    buffer[..message.len()].copy_from_slice(message);
                    Ok(message.len())
                }
                Self::BootThenApp { stage } if *stage == 1 => {
                    *stage = 2;
                    let message = b"I (2) test: same monitor continued\n";
                    buffer[..message.len()].copy_from_slice(message);
                    Ok(message.len())
                }
                Self::BootThenApp { .. } => {
                    thread::sleep(Duration::from_millis(5));
                    Err(io::Error::from(io::ErrorKind::TimedOut))
                }
                Self::Recovered { emitted } if !*emitted => {
                    *emitted = true;
                    let message = b"I (1) test: recovered monitor\n";
                    buffer[..message.len()].copy_from_slice(message);
                    Ok(message.len())
                }
                Self::Recovered { .. } => Err(io::Error::from(io::ErrorKind::TimedOut)),
            }
        }
    }

    impl Write for RecoverySerial {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn backend(rx: mpsc::Receiver<()>) -> FailingBackend {
        FailingBackend {
            gate: Mutex::new(rx),
            changed: AtomicBool::new(false),
            availability: DeviceAvailability::Available,
            capabilities: all_capabilities(),
            open_calls: AtomicUsize::new(0),
            refresh_delay: Duration::ZERO,
        }
    }
    fn request() -> DeviceRequest {
        request_for(test_device_id())
    }

    fn request_for(device_id: DeviceId) -> DeviceRequest {
        DeviceRequest {
            device_id,
            monitor_baud: 115200,
            no_reset_before: false,
        }
    }
    fn flash_manifest(data: &[u8]) -> FlashUploadManifest {
        FlashUploadManifest::from_plan(
            test_device_id(),
            &PreparedPlan {
                chip: "esp32s3".into(),
                flash_settings: crate::plan::FlashSettings::default(),
                segments: vec![crate::plan::PreparedSegment {
                    offset: 0x10000,
                    data: data.to_vec(),
                    original_size: data.len(),
                }],
            },
            460800,
            115200,
            false,
        )
        .unwrap()
    }
    fn multipart_body(
        boundary: &str,
        manifest: &FlashUploadManifest,
        parts: &[(&str, &[u8])],
        finish: bool,
    ) -> Vec<u8> {
        let mut body = Vec::new();
        write!(
            body,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"metadata\"\r\nContent-Type: application/json\r\n\r\n"
        )
        .unwrap();
        serde_json::to_writer(&mut body, manifest).unwrap();
        body.extend_from_slice(b"\r\n");
        for (name, data) in parts {
            write!(
                body,
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"artifact.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .unwrap();
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        }
        if finish {
            write!(body, "--{boundary}--\r\n").unwrap();
        }
        body
    }
    fn flash_request(key: &str, boundary: &str, body: Vec<u8>) -> Request<Body> {
        Request::post("/v1/flash")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .header("idempotency-key", key)
            .body(Body::from(body))
            .unwrap()
    }
    fn wait_done(service: &Service) -> Operation {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let store = service.inner.lock().unwrap();
            if !store.device(&test_device_id()).busy {
                return store.operations.back().unwrap().clone();
            }
            drop(store);
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_operation(service: &Service, operation_id: &str) -> Operation {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let operation = service
                .inner
                .lock()
                .unwrap()
                .operations
                .iter()
                .find(|operation| operation.id == operation_id)
                .cloned()
                .expect("submitted operation exists");
            if operation.status != "running" {
                return operation;
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_event(service: &Service, kind: &str) -> Event {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(event) = service
                .inner
                .lock()
                .unwrap()
                .device(&test_device_id())
                .events
                .iter()
                .find(|(event, _)| event.kind == kind)
                .map(|(event, _)| event.clone())
            {
                return event;
            }
            assert!(std::time::Instant::now() < deadline, "missing {kind} event");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_log_text(service: &Service, text: &str) -> Event {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(event) = service
                .inner
                .lock()
                .unwrap()
                .device(&test_device_id())
                .events
                .iter()
                .find(|(event, _)| event.kind == "log" && event.data["text"] == text)
                .map(|(event, _)| event.clone())
            {
                return event;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "missing log text {text}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn submit_sync(
        service: &Service,
        request: DeviceRequest,
        action: Action,
    ) -> ApiResult<Operation> {
        static NEXT_KEY: AtomicUsize = AtomicUsize::new(1);
        let value = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(service.submit(
                request,
                action,
                RequestIdentity {
                    key: format!("test-{value}"),
                    fingerprint: format!("fingerprint-{value}"),
                },
            ))
    }
    #[test]
    fn busy_is_rejected_and_failure_releases_the_device() {
        let (tx, rx) = mpsc::channel();
        let backend = Arc::new(backend(rx));
        let running = Arc::new(AtomicBool::new(true));
        let device = test_descriptor(DeviceAvailability::Available, all_capabilities());
        let (service, worker) = Service::start(backend, device, None, running.clone()).unwrap();
        submit_sync(&service, request(), Action::Reset).unwrap();
        assert_eq!(
            submit_sync(&service, request(), Action::Reset)
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        let mut wrong = request();
        wrong.device_id = DeviceId::new("another-device").unwrap();
        assert_eq!(
            submit_sync(&service, wrong, Action::Reset).unwrap_err().0,
            StatusCode::FORBIDDEN
        );
        tx.send(()).unwrap();
        assert_eq!(wait_done(&service).status, "failed");
        submit_sync(&service, request(), Action::Reset).unwrap();
        tx.send(()).unwrap();
        assert_eq!(wait_done(&service).status, "failed");
        running.store(false, Ordering::Relaxed);
        worker.join();
    }

    #[test]
    fn discovery_starts_empty_and_tracks_hotplug_without_restart() {
        let first_id = DeviceId::new("dev_dynamic_first").unwrap();
        let second_id = DeviceId::new("dev_dynamic_second").unwrap();
        let first = descriptor_for(
            first_id.clone(),
            "first-address",
            DeviceAvailability::Available,
            all_capabilities(),
        );
        let second = descriptor_for(
            second_id.clone(),
            "second-address",
            DeviceAvailability::Available,
            all_capabilities(),
        );
        let backend = Arc::new(DynamicBackend {
            visible: Mutex::new(Vec::new()),
        });
        let running = Arc::new(AtomicBool::new(true));
        let (service, worker) = Service::start_discovering_with_interval(
            backend.clone(),
            None,
            running.clone(),
            Duration::from_millis(5),
        )
        .unwrap();
        assert!(service.cached_devices().devices.is_empty());

        let wait_for = |expected: &[(DeviceId, DeviceAvailability)]| {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let response = service.cached_devices();
                let matches = expected.iter().all(|(id, availability)| {
                    response
                        .devices
                        .iter()
                        .find(|device| &device.id == id)
                        .is_some_and(|device| device.status.availability == *availability)
                });
                if matches && response.devices.len() == expected.len() {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "dynamic inventory did not converge"
                );
                thread::sleep(Duration::from_millis(5));
            }
        };

        backend.visible.lock().unwrap().push(first.clone());
        wait_for(&[(first_id.clone(), DeviceAvailability::Available)]);
        backend.visible.lock().unwrap().push(second.clone());
        wait_for(&[
            (first_id.clone(), DeviceAvailability::Available),
            (second_id.clone(), DeviceAvailability::Available),
        ]);
        backend
            .visible
            .lock()
            .unwrap()
            .retain(|device| device.id != first_id);
        wait_for(&[
            (first_id.clone(), DeviceAvailability::Disconnected),
            (second_id.clone(), DeviceAvailability::Available),
        ]);
        backend.visible.lock().unwrap().push(first);
        wait_for(&[
            (first_id, DeviceAvailability::Available),
            (second_id, DeviceAvailability::Available),
        ]);

        running.store(false, Ordering::Relaxed);
        worker.join();
    }

    #[test]
    fn different_devices_run_concurrently_and_keep_independent_state() {
        let first_id = DeviceId::new("dev_first").unwrap();
        let second_id = DeviceId::new("dev_second").unwrap();
        let first = descriptor_for(
            first_id.clone(),
            "first-address",
            DeviceAvailability::Available,
            all_capabilities(),
        );
        let second = descriptor_for(
            second_id.clone(),
            "second-address",
            DeviceAvailability::Available,
            all_capabilities(),
        );
        let backend = Arc::new(ParallelBackend {
            devices: HashMap::from([
                (first_id.clone(), first.clone()),
                (second_id.clone(), second.clone()),
            ]),
            started: Mutex::new(HashSet::new()),
            started_changed: Condvar::new(),
        });
        let running = Arc::new(AtomicBool::new(true));
        let (service, worker) =
            Service::start_many(backend, vec![first, second], None, running.clone()).unwrap();
        let app = router(service.clone());
        let request = |device_id: DeviceId, key: &str| {
            Request::post("/v1/probe")
                .header("content-type", "application/json")
                .header("idempotency-key", key)
                .body(Body::from(
                    serde_json::to_vec(&request_for(device_id)).unwrap(),
                ))
                .unwrap()
        };
        let (first_response, second_response) =
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                tokio::join!(
                    app.clone()
                        .oneshot(request(first_id.clone(), "parallel-first")),
                    app.clone()
                        .oneshot(request(second_id.clone(), "parallel-second")),
                )
            });
        let first_response = first_response.unwrap();
        let second_response = second_response.unwrap();
        assert_eq!(first_response.status(), StatusCode::ACCEPTED);
        assert_eq!(second_response.status(), StatusCode::ACCEPTED);
        let first_operation: Operation = serde_json::from_slice(
            &tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(to_bytes(first_response.into_body(), usize::MAX))
                .unwrap(),
        )
        .unwrap();
        let second_operation: Operation = serde_json::from_slice(
            &tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(to_bytes(second_response.into_body(), usize::MAX))
                .unwrap(),
        )
        .unwrap();
        let first_result = wait_operation(&service, &first_operation.id);
        let second_result = wait_operation(&service, &second_operation.id);
        assert_eq!(first_result.status, "succeeded");
        assert_eq!(second_result.status, "succeeded");
        assert_eq!(first_result.device_id, first_id);
        assert_eq!(second_result.device_id, second_id);

        let event_response = tokio::runtime::Runtime::new().unwrap().block_on(
            app.oneshot(
                Request::get(format!(
                    "/v1/events?device_id={}&epoch={}&after=0",
                    first_id, first_result.start_cursor.epoch
                ))
                .body(Body::empty())
                .unwrap(),
            ),
        );
        let event_response = event_response.unwrap();
        assert_eq!(event_response.status(), StatusCode::OK);
        let event_batch: EventBatch = serde_json::from_slice(
            &tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(to_bytes(event_response.into_body(), usize::MAX))
                .unwrap(),
        )
        .unwrap();
        assert!(event_batch.events.iter().any(|event| {
            event.kind == "operation_finished" && event.data["device_id"] == first_id.as_str()
        }));
        assert!(event_batch.events.iter().all(|event| {
            event.kind != "operation_finished" || event.data["device_id"] != second_id.as_str()
        }));

        let mut store = service.inner.lock().unwrap();
        assert_eq!(
            store.device(&first_id).status.activity,
            DeviceActivity::Monitoring
        );
        assert_eq!(
            store.device(&second_id).status.activity,
            DeviceActivity::Monitoring
        );
        store.emit(&first_id, "log", json!({"text":"first only"}));
        assert!(
            store
                .device(&first_id)
                .events
                .iter()
                .any(|(event, _)| event.data["text"] == "first only")
        );
        assert!(
            store
                .device(&second_id)
                .events
                .iter()
                .all(|(event, _)| event.data["text"] != "first only")
        );
        drop(store);

        running.store(false, Ordering::Relaxed);
        worker.join();
    }

    #[test]
    fn monitor_disconnect_retries_until_the_same_device_is_available() {
        let open_modes = Arc::new(Mutex::new(Vec::new()));
        let monitor_resets = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(ReconnectBackend {
            open_modes: open_modes.clone(),
            monitor_resets: monitor_resets.clone(),
            disconnect_seen: Arc::new(AtomicBool::new(false)),
            reconnect_refreshes: AtomicUsize::new(0),
            disconnected_refreshes: 2,
            silent_boot_handoff: false,
            continued_boot_output: false,
        });
        let running = Arc::new(AtomicBool::new(true));
        let (service, worker) = Service::start(
            backend,
            test_descriptor(DeviceAvailability::Available, all_capabilities()),
            None,
            running.clone(),
        )
        .unwrap();

        submit_sync(&service, request(), Action::Monitor).unwrap();
        assert_eq!(wait_done(&service).status, "succeeded");
        let reconnected = wait_for_event(&service, "reconnected");
        assert_eq!(reconnected.data["attempts"], 3);
        let log = wait_for_log_text(&service, "I (1) test: recovered monitor");
        assert_eq!(log.data["text"], "I (1) test: recovered monitor");

        let store = service.inner.lock().unwrap();
        assert_eq!(store.operations.len(), 1);
        assert_eq!(
            store.device(&test_device_id()).status,
            DeviceStatus {
                availability: DeviceAvailability::Available,
                activity: DeviceActivity::Monitoring,
            }
        );
        assert!(
            store
                .device(&test_device_id())
                .events
                .iter()
                .any(|(event, _)| event.kind == "reconnecting")
        );
        assert!(
            store
                .device(&test_device_id())
                .events
                .iter()
                .all(|(event, _)| event.kind != "disconnected")
        );
        drop(store);
        assert_eq!(*open_modes.lock().unwrap(), [false, true]);
        assert_eq!(*monitor_resets.lock().unwrap(), [false, false]);

        running.store(false, Ordering::Relaxed);
        worker.join();
    }

    #[test]
    fn native_usb_boot_entry_silence_reopens_the_monitor_without_reset() {
        let open_modes = Arc::new(Mutex::new(Vec::new()));
        let monitor_resets = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(ReconnectBackend {
            open_modes: open_modes.clone(),
            monitor_resets: monitor_resets.clone(),
            disconnect_seen: Arc::new(AtomicBool::new(false)),
            reconnect_refreshes: AtomicUsize::new(0),
            disconnected_refreshes: 0,
            silent_boot_handoff: true,
            continued_boot_output: false,
        });
        let running = Arc::new(AtomicBool::new(true));
        let mut descriptor = test_descriptor(DeviceAvailability::Available, all_capabilities());
        descriptor
            .transport
            .metadata
            .insert("reopen_after_boot_entry".into(), "true".into());
        let (service, worker) = Service::start(backend, descriptor, None, running.clone()).unwrap();

        submit_sync(&service, request(), Action::Monitor).unwrap();
        assert_eq!(wait_done(&service).status, "succeeded");
        let entry = wait_for_event(&service, "log");
        assert_eq!(entry.data["text"], "entry 0x403c8908");
        let reconnecting = wait_for_event(&service, "reconnecting");
        assert_eq!(reconnecting.data["reason"], "native USB boot handoff");
        let reconnected = wait_for_event(&service, "reconnected");
        assert_eq!(reconnected.data["attempts"], 1);
        let log = wait_for_log_text(&service, "I (1) test: recovered monitor");
        assert_eq!(log.data["text"], "I (1) test: recovered monitor");
        assert_eq!(*open_modes.lock().unwrap(), [false, true]);
        assert_eq!(*monitor_resets.lock().unwrap(), [false, false]);

        running.store(false, Ordering::Relaxed);
        worker.join();
    }

    #[test]
    fn native_usb_continued_output_cancels_the_boot_handoff_reopen() {
        let open_modes = Arc::new(Mutex::new(Vec::new()));
        let monitor_resets = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(ReconnectBackend {
            open_modes: open_modes.clone(),
            monitor_resets: monitor_resets.clone(),
            disconnect_seen: Arc::new(AtomicBool::new(false)),
            reconnect_refreshes: AtomicUsize::new(0),
            disconnected_refreshes: 0,
            silent_boot_handoff: false,
            continued_boot_output: true,
        });
        let running = Arc::new(AtomicBool::new(true));
        let mut descriptor = test_descriptor(DeviceAvailability::Available, all_capabilities());
        descriptor
            .transport
            .metadata
            .insert("reopen_after_boot_entry".into(), "true".into());
        let (service, worker) = Service::start(backend, descriptor, None, running.clone()).unwrap();

        submit_sync(&service, request(), Action::Monitor).unwrap();
        assert_eq!(wait_done(&service).status, "succeeded");
        wait_for_log_text(&service, "I (2) test: same monitor continued");
        thread::sleep(NATIVE_USB_BOOT_REOPEN_DELAY + Duration::from_millis(50));
        assert_eq!(*open_modes.lock().unwrap(), [false]);
        assert_eq!(*monitor_resets.lock().unwrap(), [false]);
        assert!(
            service
                .inner
                .lock()
                .unwrap()
                .device(&test_device_id())
                .events
                .iter()
                .all(|(event, _)| event.kind != "reconnecting")
        );

        running.store(false, Ordering::Relaxed);
        worker.join();
    }

    #[test]
    fn concurrent_idempotent_replays_share_one_operation_and_conflicts_are_rejected() {
        let (tx, rx) = mpsc::channel();
        let backend = Arc::new(backend(rx));
        let running = Arc::new(AtomicBool::new(true));
        let device = test_descriptor(DeviceAvailability::Available, all_capabilities());
        let (service, worker) =
            Service::start(backend.clone(), device, None, running.clone()).unwrap();
        let identity = RequestIdentity {
            key: "same-request".into(),
            fingerprint: "same-fingerprint".into(),
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (first, replay) = runtime.block_on(async {
            tokio::join!(
                service.submit(request(), Action::Reset, identity.clone()),
                service.submit(request(), Action::Reset, identity.clone())
            )
        });
        let first = first.unwrap();
        let replay = replay.unwrap();
        assert_eq!(first.id, replay.id);
        assert_eq!(first.request_key, "same-request");
        assert_eq!(service.inner.lock().unwrap().operations.len(), 1);

        let conflict = runtime
            .block_on(service.submit(
                request(),
                Action::Reset,
                RequestIdentity {
                    key: identity.key,
                    fingerprint: "different-fingerprint".into(),
                },
            ))
            .unwrap_err();
        assert_eq!(conflict.0, StatusCode::CONFLICT);
        assert!(conflict.1.contains("idempotency_key_conflict"));

        tx.send(()).unwrap();
        assert_eq!(wait_done(&service).status, "failed");
        assert_eq!(backend.open_calls.load(Ordering::Relaxed), 1);
        running.store(false, Ordering::Relaxed);
        worker.join();
    }
    #[test]
    fn multipart_flash_is_verified_before_semantic_idempotent_admission() {
        let data = b"firmware bytes";
        let manifest = flash_manifest(data);
        let (tx, rx) = mpsc::channel();
        let backend = Arc::new(backend(rx));
        let running = Arc::new(AtomicBool::new(true));
        let device = test_descriptor(DeviceAvailability::Available, all_capabilities());
        let (service, worker) =
            Service::start(backend.clone(), device, None, running.clone()).unwrap();
        let app = router(service.clone());
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let submit = |boundary: &str, value: &FlashUploadManifest| {
                flash_request(
                    "multipart-replay",
                    boundary,
                    multipart_body(boundary, value, &[("segment-0", data)], true),
                )
            };
            let first = app
                .clone()
                .oneshot(submit("first-boundary", &manifest))
                .await
                .unwrap();
            assert_eq!(first.status(), StatusCode::ACCEPTED);
            let first: Operation =
                serde_json::from_slice(&to_bytes(first.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();

            let replay = app
                .clone()
                .oneshot(submit("different-boundary", &manifest))
                .await
                .unwrap();
            assert_eq!(replay.status(), StatusCode::ACCEPTED);
            let replay: Operation =
                serde_json::from_slice(&to_bytes(replay.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            assert_eq!(first.id, replay.id);

            let mut changed = manifest.clone();
            changed.chip = "esp32c3".into();
            let conflict = app
                .oneshot(submit("third-boundary", &changed))
                .await
                .unwrap();
            assert_eq!(conflict.status(), StatusCode::CONFLICT);
        });
        assert_eq!(service.inner.lock().unwrap().operations.len(), 1);
        tx.send(()).unwrap();
        assert_eq!(wait_done(&service).status, "failed");
        assert_eq!(backend.open_calls.load(Ordering::Relaxed), 1);
        running.store(false, Ordering::Relaxed);
        worker.join();
    }

    #[test]
    fn flash_operation_result_retains_idf_import_warnings() {
        let data = b"firmware bytes";
        let manifest = flash_manifest(data).with_warnings(vec![
            crate::wire::FlashWarning::EmptyIdfImageSkipped {
                image: Some("assets".into()),
                offset: 0x628000,
            },
        ]);
        let running = Arc::new(AtomicBool::new(true));
        let backend = Arc::new(ReadBackend {
            payload: Vec::new(),
            open_calls: Arc::new(AtomicUsize::new(0)),
            erase_calls: Arc::new(AtomicUsize::new(0)),
            fail_monitor_handoff: false,
        });
        let device = test_descriptor(DeviceAvailability::Available, all_capabilities());
        let (service, worker) = Service::start(backend, device, None, running.clone()).unwrap();
        let app = router(service.clone());
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let response = app
                .oneshot(flash_request(
                    "warning-result",
                    "warning-boundary",
                    multipart_body("warning-boundary", &manifest, &[("segment-0", data)], true),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        });
        let operation = wait_done(&service);
        assert_eq!(operation.status, "succeeded");
        assert_eq!(
            operation.result.unwrap()["warnings"],
            json!([{
                "code": "empty_idf_image_skipped",
                "image": "assets",
                "offset": 0x628000,
            }])
        );
        running.store(false, Ordering::Relaxed);
        worker.join();
    }
    #[test]
    fn invalid_multipart_flash_never_reserves_a_key_or_opens_hardware() {
        let data = b"firmware bytes";
        let manifest = flash_manifest(data);
        let (_tx, rx) = mpsc::channel();
        let backend = Arc::new(backend(rx));
        let running = Arc::new(AtomicBool::new(true));
        let device = test_descriptor(DeviceAvailability::Available, all_capabilities());
        let (service, worker) =
            Service::start(backend.clone(), device, None, running.clone()).unwrap();
        let app = router(service.clone());
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let mut bad_digest = manifest.clone();
            bad_digest.segments[0].sha256 = "0".repeat(64);
            let cases = [
                multipart_body("bad-digest", &bad_digest, &[("segment-0", data)], true),
                multipart_body("missing", &manifest, &[], true),
                multipart_body("unknown", &manifest, &[("not-declared", data)], true),
                multipart_body(
                    "too-large",
                    &manifest,
                    &[("segment-0", b"firmware bytes plus one")],
                    true,
                ),
                multipart_body("truncated", &manifest, &[("segment-0", data)], false),
            ];
            for (index, body) in cases.into_iter().enumerate() {
                let boundary =
                    ["bad-digest", "missing", "unknown", "too-large", "truncated"][index];
                let response = app
                    .clone()
                    .oneshot(flash_request(&format!("invalid-{index}"), boundary, body))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::BAD_REQUEST, "case {index}");
            }
        });
        let store = service.inner.lock().unwrap();
        assert!(store.operations.is_empty());
        assert!(store.idempotency.is_empty());
        assert_eq!(backend.open_calls.load(Ordering::Relaxed), 0);
        drop(store);
        assert_eq!(
            std::fs::read_dir(service.artifact_dir.path())
                .unwrap()
                .count(),
            0
        );
        running.store(false, Ordering::Relaxed);
        worker.join();
    }
    #[test]
    fn slow_multipart_upload_does_not_block_the_async_executor() {
        let data = vec![7; 16 * 1024];
        let manifest = flash_manifest(&data);
        let boundary = "slow-boundary";
        let body = multipart_body(boundary, &manifest, &[("segment-0", &data)], true);
        let split = body.len() / 2;
        let first = Bytes::copy_from_slice(&body[..split]);
        let second = Bytes::copy_from_slice(&body[split..]);
        let stream = async_stream::stream! {
            yield Ok::<_, Infallible>(first);
            tokio::time::sleep(Duration::from_millis(200)).await;
            yield Ok::<_, Infallible>(second);
        };
        let request = Request::post("/v1/flash")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .header("idempotency-key", "slow-upload")
            .body(Body::from_stream(stream))
            .unwrap();

        let (tx, rx) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let device = test_descriptor(DeviceAvailability::Available, all_capabilities());
        let (service, worker) =
            Service::start(Arc::new(backend(rx)), device, None, running.clone()).unwrap();
        let app = router(service.clone());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let response = app.oneshot(request);
                let timer = async {
                    tokio::time::timeout(Duration::from_millis(100), async {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    })
                    .await
                };
                let (response, timer) = tokio::join!(response, timer);
                assert!(
                    timer.is_ok(),
                    "multipart receive blocked the async executor"
                );
                assert_eq!(response.unwrap().status(), StatusCode::ACCEPTED);
            });
        tx.send(()).unwrap();
        assert_eq!(wait_done(&service).status, "failed");
        running.store(false, Ordering::Relaxed);
        worker.join();
    }
    #[test]
    fn http_idempotency_header_replays_canonical_request() {
        let (tx, rx) = mpsc::channel();
        let backend = Arc::new(backend(rx));
        let running = Arc::new(AtomicBool::new(true));
        let device = test_descriptor(DeviceAvailability::Available, all_capabilities());
        let (service, worker) =
            Service::start(backend.clone(), device, None, running.clone()).unwrap();
        let app = router(service.clone());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let body = serde_json::to_vec(&request()).unwrap();
            let missing = app
                .clone()
                .oneshot(
                    Request::post("/v1/reset")
                        .header("content-type", "application/json")
                        .body(Body::from(body.clone()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

            let submit = |body: Vec<u8>| {
                Request::post("/v1/reset")
                    .header("content-type", "application/json")
                    .header("idempotency-key", "http-replay")
                    .body(Body::from(body))
                    .unwrap()
            };
            let first = app.clone().oneshot(submit(body.clone())).await.unwrap();
            assert_eq!(first.status(), StatusCode::ACCEPTED);
            let first: Operation =
                serde_json::from_slice(&to_bytes(first.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            let replay = app.clone().oneshot(submit(body)).await.unwrap();
            assert_eq!(replay.status(), StatusCode::ACCEPTED);
            let replay: Operation =
                serde_json::from_slice(&to_bytes(replay.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            assert_eq!(first.id, replay.id);
            assert_eq!(replay.request_key, "http-replay");

            let mut changed = request();
            changed.monitor_baud = 230400;
            let conflict = app
                .oneshot(submit(serde_json::to_vec(&changed).unwrap()))
                .await
                .unwrap();
            assert_eq!(conflict.status(), StatusCode::CONFLICT);
        });
        assert_eq!(service.inner.lock().unwrap().operations.len(), 1);
        tx.send(()).unwrap();
        assert_eq!(wait_done(&service).status, "failed");
        assert_eq!(backend.open_calls.load(Ordering::Relaxed), 1);
        running.store(false, Ordering::Relaxed);
        worker.join();
    }
    #[test]
    fn changed_usb_identity_never_opens_a_port() {
        let (_tx, rx) = mpsc::channel();
        let backend = Arc::new(backend(rx));
        let running = Arc::new(AtomicBool::new(true));
        let device = test_descriptor(DeviceAvailability::Available, all_capabilities());
        let (service, worker) =
            Service::start(backend.clone(), device, None, running.clone()).unwrap();
        backend.changed.store(true, Ordering::Relaxed);
        assert!(
            submit_sync(&service, request(), Action::Reset)
                .unwrap_err()
                .1
                .contains("device_identity_mismatch")
        );
        assert_eq!(backend.open_calls.load(Ordering::Relaxed), 0);
        running.store(false, Ordering::Relaxed);
        worker.join();
    }
    #[test]
    fn lifecycle_and_capabilities_are_rejected_before_open() {
        for (availability, expected) in [
            (
                DeviceAvailability::PermissionRequired,
                "device_permission_required",
            ),
            (DeviceAvailability::Disconnected, "device_disconnected"),
            (
                DeviceAvailability::IdentityMismatch,
                "device_identity_mismatch",
            ),
            (DeviceAvailability::Ambiguous, "device_ambiguous"),
        ] {
            let (_tx, rx) = mpsc::channel();
            let mut unavailable = backend(rx);
            unavailable.availability = availability;
            let running = Arc::new(AtomicBool::new(true));
            let device = test_descriptor(availability, all_capabilities());
            let (service, worker) =
                Service::start(Arc::new(unavailable), device, None, running.clone()).unwrap();
            assert!(
                submit_sync(&service, request(), Action::Reset)
                    .unwrap_err()
                    .1
                    .contains(expected)
            );
            running.store(false, Ordering::Relaxed);
            worker.join();
        }

        let (_tx, rx) = mpsc::channel();
        let mut probe_only = backend(rx);
        probe_only.capabilities = vec![DeviceCapability::Probe];
        let running = Arc::new(AtomicBool::new(true));
        let device = test_descriptor(DeviceAvailability::Available, vec![DeviceCapability::Probe]);
        let (service, worker) =
            Service::start(Arc::new(probe_only), device, None, running.clone()).unwrap();
        assert_eq!(
            submit_sync(&service, request(), Action::Reset)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
        running.store(false, Ordering::Relaxed);
        worker.join();
    }
    #[test]
    fn http_contract_exposes_descriptors_and_accepts_only_device_ids() {
        let (_tx, rx) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let device = test_descriptor(DeviceAvailability::Available, all_capabilities());
        let (service, worker) =
            Service::start(Arc::new(backend(rx)), device, None, running.clone()).unwrap();
        let app = router(service);
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let response = app
                .clone()
                .oneshot(Request::get("/v1/devices").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            assert_eq!(body["devices"][0]["id"], "dev_test");
            assert_eq!(body["devices"][0]["transport"]["address"], "test-address");
            assert_eq!(body["devices"][0]["status"]["availability"], "available");
            assert_eq!(body["devices"][0]["status"]["activity"], "idle");

            let legacy = app
                .clone()
                .oneshot(
                    Request::post("/v1/reset")
                        .header("content-type", "application/json")
                        .header("idempotency-key", "forbidden-reset")
                        .body(Body::from(r#"{"port":"test-address"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(legacy.status(), StatusCode::UNPROCESSABLE_ENTITY);

            let forbidden = app
                .oneshot(
                    Request::post("/v1/reset")
                        .header("content-type", "application/json")
                        .header("idempotency-key", "forbidden-reset")
                        .body(Body::from(
                            r#"{"device_id":"dev_forbidden","monitor_baud":115200}"#,
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
        });
        running.store(false, Ordering::Relaxed);
        worker.join();
    }
    #[test]
    fn slow_device_refresh_does_not_block_the_async_executor() {
        let (_tx, rx) = mpsc::channel();
        let mut slow = backend(rx);
        slow.refresh_delay = Duration::from_millis(200);
        let running = Arc::new(AtomicBool::new(true));
        let device = test_descriptor(DeviceAvailability::Available, all_capabilities());
        let (service, worker) =
            Service::start(Arc::new(slow), device, None, running.clone()).unwrap();
        let app = router(service);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let response =
                    app.oneshot(Request::get("/v1/devices").body(Body::empty()).unwrap());
                let timer = async {
                    tokio::time::timeout(Duration::from_millis(100), async {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    })
                    .await
                };
                let (response, timer) = tokio::join!(response, timer);
                assert!(timer.is_ok(), "device refresh blocked the async executor");
                assert_eq!(response.unwrap().status(), StatusCode::OK);
            });
        running.store(false, Ordering::Relaxed);
        worker.join();
    }
    #[test]
    fn serial_write_reuses_monitor_and_drops_it_after_write_failure() {
        let open_calls = Arc::new(AtomicUsize::new(0));
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let writes = Arc::new(Mutex::new(Vec::new()));
        let fail_write = Arc::new(AtomicBool::new(false));
        let backend = Arc::new(SerialBackend {
            open_calls: open_calls.clone(),
            refresh_calls: refresh_calls.clone(),
            writes: writes.clone(),
            fail_write: fail_write.clone(),
            read_gate: None,
        });
        let running = Arc::new(AtomicBool::new(true));
        let device = test_descriptor(DeviceAvailability::Available, all_capabilities());
        let (service, worker) = Service::start(backend, device, None, running.clone()).unwrap();
        let app = router(service.clone());

        submit_sync(&service, request(), Action::Monitor).unwrap();
        assert_eq!(wait_done(&service).status, "succeeded");
        assert_eq!(open_calls.load(Ordering::Relaxed), 1);
        let monitor_refreshes = refresh_calls.load(Ordering::Relaxed);

        let payload = b"private serial command\r\n";
        let request =
            SerialWriteRequest::from_bytes(test_device_id(), payload, 115200, 2_000).unwrap();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let mut invalid = request.clone();
            invalid.sha256 = "0".repeat(64);
            let response = app
                .clone()
                .oneshot(
                    Request::post("/v1/serial-write")
                        .header("content-type", "application/json")
                        .header("idempotency-key", "serial-write-valid")
                        .body(Body::from(serde_json::to_vec(&invalid).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);

            let response = app
                .clone()
                .oneshot(
                    Request::post("/v1/serial-write")
                        .header("content-type", "application/json")
                        .header("idempotency-key", "serial-write-valid")
                        .body(Body::from(serde_json::to_vec(&request).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        });
        let operation = wait_done(&service);
        assert_eq!(operation.status, "succeeded");
        assert_eq!(operation.result.as_ref().unwrap()["written"], payload.len());
        assert_eq!(open_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            refresh_calls.load(Ordering::Relaxed),
            monitor_refreshes,
            "writing through an active monitor must not rescan USB"
        );
        assert_eq!(&*writes.lock().unwrap(), payload);
        let store = service.inner.lock().unwrap();
        let events = serde_json::to_string(&store.device(&test_device_id()).events).unwrap();
        drop(store);
        assert!(!events.contains("private serial command"));

        fail_write.store(true, Ordering::Relaxed);
        let failed =
            SerialWriteRequest::from_bytes(test_device_id(), b"fail", 115200, 2_000).unwrap();
        submit_sync(
            &service,
            failed.device_request(),
            Action::SerialWrite {
                data: failed.decode().unwrap(),
                sha256: failed.sha256,
                baud: failed.baud,
                timeout_ms: failed.timeout_ms,
            },
        )
        .unwrap();
        assert_eq!(wait_done(&service).status, "failed");
        assert_eq!(
            service
                .inner
                .lock()
                .unwrap()
                .device(&test_device_id())
                .status
                .activity,
            DeviceActivity::Error
        );

        fail_write.store(false, Ordering::Relaxed);
        let recovered =
            SerialWriteRequest::from_bytes(test_device_id(), b"recovered", 115200, 2_000).unwrap();
        submit_sync(
            &service,
            recovered.device_request(),
            Action::SerialWrite {
                data: recovered.decode().unwrap(),
                sha256: recovered.sha256,
                baud: recovered.baud,
                timeout_ms: recovered.timeout_ms,
            },
        )
        .unwrap();
        assert_eq!(wait_done(&service).status, "succeeded");
        assert_eq!(open_calls.load(Ordering::Relaxed), 2);
        assert!(writes.lock().unwrap().ends_with(b"recovered"));

        running.store(false, Ordering::Relaxed);
        worker.join();
    }

    #[test]
    fn in_flight_read_survives_serial_write_admission_but_not_monitor_replacement() {
        for serial_write in [true, false] {
            let (started_tx, started_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let backend = Arc::new(SerialBackend {
                open_calls: Arc::new(AtomicUsize::new(0)),
                refresh_calls: Arc::new(AtomicUsize::new(0)),
                writes: Arc::new(Mutex::new(Vec::new())),
                fail_write: Arc::new(AtomicBool::new(false)),
                read_gate: Some(Arc::new(ReadGate(Mutex::new(Some((
                    started_tx, release_rx,
                )))))),
            });
            let running = Arc::new(AtomicBool::new(true));
            let (service, worker) = Service::start(
                backend,
                test_descriptor(DeviceAvailability::Available, all_capabilities()),
                None,
                running.clone(),
            )
            .unwrap();
            submit_sync(&service, request(), Action::Monitor).unwrap();
            assert_eq!(wait_done(&service).status, "succeeded");
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let action = if serial_write {
                Action::SerialWrite {
                    data: b"next".to_vec(),
                    sha256: "test".into(),
                    baud: 115200,
                    timeout_ms: 2000,
                }
            } else {
                Action::Monitor
            };
            submit_sync(&service, request(), action).unwrap();
            // Publish bytes from the read which was blocked before admission.
            release_tx.send(()).unwrap();
            assert_eq!(wait_done(&service).status, "succeeded");
            let has_echo = service
                .inner
                .lock()
                .unwrap()
                .device(&test_device_id())
                .events
                .iter()
                .any(|(event, _)| event.kind == "log" && event.data["text"] == "echo");
            running.store(false, Ordering::Relaxed);
            worker.join();
            assert_eq!(
                has_echo, serial_write,
                "serial writes continue the stream; replacing the monitor starts a new boundary"
            );
        }
    }

    #[tokio::test]
    async fn event_long_poll_wakes_all_clients_and_keeps_cursor_validation() {
        let (_, rx) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let (service, worker) = Service::start(
            Arc::new(backend(rx)),
            test_descriptor(DeviceAvailability::Available, all_capabilities()),
            None,
            running.clone(),
        )
        .unwrap();
        let cursor = service.inner.lock().unwrap().cursor(&test_device_id());
        let path = format!(
            "/v1/events?device_id={}&epoch={}&after={}&wait_ms=1000",
            test_device_id(),
            cursor.epoch,
            cursor.after
        );
        let first =
            router(service.clone()).oneshot(Request::get(&path).body(Body::empty()).unwrap());
        let second =
            router(service.clone()).oneshot(Request::get(&path).body(Body::empty()).unwrap());
        let publish = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            service.emit(&test_device_id(), "raw", json!({"base64":"YQ=="}));
        };
        let (a, b, _) = tokio::time::timeout(Duration::from_millis(200), async {
            tokio::join!(first, second, publish)
        })
        .await
        .expect("publication must wake both clients without waiting the full second");
        for response in [a.unwrap(), b.unwrap()] {
            assert_eq!(response.status(), StatusCode::OK);
            let batch: EventBatch =
                serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap())
                    .unwrap();
            assert_eq!(batch.events.len(), 1);
            assert_eq!(batch.cursor.after, cursor.after + 1);
        }
        let response = router(service.clone())
            .oneshot(
                Request::get(format!(
                    "/v1/events?device_id={}&epoch=wrong&after=0&wait_ms=1000",
                    test_device_id()
                ))
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let response = router(service.clone())
            .oneshot(
                Request::get(format!(
                    "/v1/events?device_id={}&epoch={}&after={}&wait_ms=1",
                    test_device_id(),
                    cursor.epoch,
                    cursor.after + 1
                ))
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        let batch: EventBatch =
            serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert!(batch.events.is_empty());
        running.store(false, Ordering::Relaxed);
        worker.join();
    }
    #[test]
    fn erase_flash_requires_confirmation_and_leaves_download_mode_idle() {
        let open_calls = Arc::new(AtomicUsize::new(0));
        let erase_calls = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(AtomicBool::new(true));
        let device = test_descriptor(DeviceAvailability::Available, all_capabilities());
        let (service, worker) = Service::start(
            Arc::new(ReadBackend {
                payload: Vec::new(),
                open_calls: open_calls.clone(),
                erase_calls: erase_calls.clone(),
                fail_monitor_handoff: false,
            }),
            device,
            None,
            running.clone(),
        )
        .unwrap();
        let app = router(service.clone());
        let mut request = EraseFlashRequest {
            device_id: test_device_id(),
            confirmation: "wrong".into(),
            flash_baud: 460800,
            no_reset_before: false,
        };
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/v1/erase-flash")
                        .header("content-type", "application/json")
                        .header("idempotency-key", "erase-valid")
                        .body(Body::from(serde_json::to_vec(&request).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            request.confirmation = crate::wire::ERASE_FLASH_CONFIRMATION.into();
            let response = app
                .oneshot(
                    Request::post("/v1/erase-flash")
                        .header("content-type", "application/json")
                        .header("idempotency-key", "erase-valid")
                        .body(Body::from(serde_json::to_vec(&request).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        });
        let operation = wait_done(&service);
        assert_eq!(operation.status, "succeeded");
        assert_eq!(operation.result.as_ref().unwrap()["erased"], true);
        assert_eq!(
            operation.result.as_ref().unwrap()["post_state"],
            "download_mode"
        );
        assert_eq!(
            operation.result.as_ref().unwrap()["board"]["chip"],
            "esp32s3"
        );
        assert_eq!(open_calls.load(Ordering::Relaxed), 1);
        assert_eq!(erase_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            service
                .inner
                .lock()
                .unwrap()
                .device(&test_device_id())
                .status
                .activity,
            DeviceActivity::Idle
        );
        assert!(
            service
                .inner
                .lock()
                .unwrap()
                .device(&test_device_id())
                .events
                .iter()
                .any(|(event, _)| event.kind == "flash_erased")
        );
        running.store(false, Ordering::Relaxed);
        worker.join();
    }
    #[test]
    fn read_flash_stores_bytes_outside_json_and_streams_the_artifact() {
        let payload = b"private-flash-bytes".to_vec();
        let open_calls = Arc::new(AtomicUsize::new(0));
        let backend = Arc::new(ReadBackend {
            payload: payload.clone(),
            open_calls: open_calls.clone(),
            erase_calls: Arc::new(AtomicUsize::new(0)),
            fail_monitor_handoff: false,
        });
        let running = Arc::new(AtomicBool::new(true));
        let device = test_descriptor(DeviceAvailability::Available, all_capabilities());
        let (service, worker) = Service::start(backend, device, None, running.clone()).unwrap();
        let app = router(service.clone());
        let operation_id = tokio::runtime::Runtime::new().unwrap().block_on(async {
            let invalid = ReadFlashRequest {
                device_id: test_device_id(),
                offset: 0,
                size: 0,
                flash_baud: 460800,
                monitor_baud: 115200,
                no_reset_before: false,
            };
            let response = app
                .clone()
                .oneshot(
                    Request::post("/v1/read-flash")
                        .header("content-type", "application/json")
                        .header("idempotency-key", "read-valid")
                        .body(Body::from(serde_json::to_vec(&invalid).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(open_calls.load(Ordering::Relaxed), 0);

            let request = ReadFlashRequest {
                size: payload.len() as u32,
                ..invalid
            };
            let response = app
                .clone()
                .oneshot(
                    Request::post("/v1/read-flash")
                        .header("content-type", "application/json")
                        .header("idempotency-key", "read-valid")
                        .body(Body::from(serde_json::to_vec(&request).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let submitted: Operation =
                serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                    .unwrap();
            submitted.id
        });

        let operation = wait_done(&service);
        assert_eq!(operation.id, operation_id);
        let metadata = operation.artifact.as_ref().unwrap();
        assert_eq!(metadata.size, payload.len() as u64);
        assert_eq!(metadata.content_type, "application/octet-stream");
        assert_eq!(metadata.sha256.len(), 64);
        let store = service.inner.lock().unwrap();
        let event_json = serde_json::to_string(&store.device(&test_device_id()).events).unwrap();
        drop(store);
        assert!(!event_json.contains("private-flash-bytes"));
        assert_eq!(open_calls.load(Ordering::Relaxed), 1);

        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let response = app
                .oneshot(
                    Request::get(format!("/v1/operations/{operation_id}/artifact"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[header::CONTENT_LENGTH],
                payload.len().to_string()
            );
            assert_eq!(response.headers()["x-idf-remote-sha256"], metadata.sha256);
            assert_eq!(
                to_bytes(response.into_body(), 1024 * 1024).await.unwrap(),
                payload
            );
        });
        running.store(false, Ordering::Relaxed);
        worker.join();
    }
    #[test]
    fn read_artifact_is_not_published_when_monitor_handoff_fails() {
        let payload = b"unpublished-flash-bytes".to_vec();
        let running = Arc::new(AtomicBool::new(true));
        let device = test_descriptor(DeviceAvailability::Available, all_capabilities());
        let (service, worker) = Service::start(
            Arc::new(ReadBackend {
                payload: payload.clone(),
                open_calls: Arc::new(AtomicUsize::new(0)),
                erase_calls: Arc::new(AtomicUsize::new(0)),
                fail_monitor_handoff: true,
            }),
            device,
            None,
            running.clone(),
        )
        .unwrap();
        let app = router(service.clone());
        let submitted = submit_sync(
            &service,
            ReadFlashRequest {
                device_id: test_device_id(),
                offset: 0,
                size: payload.len() as u32,
                flash_baud: 460800,
                monitor_baud: 115200,
                no_reset_before: false,
            }
            .device_request(),
            Action::ReadFlash {
                offset: 0,
                size: payload.len() as u32,
                baud: 460800,
            },
        )
        .unwrap();

        let operation = wait_done(&service);
        assert_eq!(operation.id, submitted.id);
        assert_eq!(operation.status, "failed");
        assert!(operation.artifact.is_none());
        assert!(service.inner.lock().unwrap().artifacts.is_empty());
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let response = app
                .oneshot(
                    Request::get(format!("/v1/operations/{}/artifact", operation.id))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        });
        running.store(false, Ordering::Relaxed);
        worker.join();
    }
    #[test]
    fn refuses_non_loopback_without_authentication_before_starting_hardware() {
        let (_tx, rx) = mpsc::channel();
        let error = serve(
            "0.0.0.0:0".parse().unwrap(),
            Arc::new(backend(rx)),
            test_descriptor(DeviceAvailability::Available, all_capabilities()),
            None,
            Arc::new(AtomicBool::new(true)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("requires --token-file"));
    }
    #[test]
    fn cursors_reject_restarts_and_explicitly_report_eviction() {
        let device_id = test_device_id();
        let mut store = Store::new([(device_id.clone(), DeviceStatus::AVAILABLE_IDLE)]);
        let old = store.cursor(&device_id);
        for _ in 0..MAX_EVENTS + 1 {
            store.emit(&device_id, "log", json!({"text":"hello"}));
        }
        assert_eq!(
            store.events_after(&device_id, &old).unwrap_err().0,
            StatusCode::GONE
        );
        assert_eq!(
            store
                .events_after(&device_id, &Cursor::default())
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        assert!(
            store
                .events_after(&device_id, &store.cursor(&device_id))
                .unwrap()
                .events
                .is_empty()
        );
    }
    #[test]
    fn operation_eviction_removes_its_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_path = directory.path().join("old.bin");
        std::fs::write(&artifact_path, b"old artifact").unwrap();
        let mut store = Store::new([(test_device_id(), DeviceStatus::AVAILABLE_IDLE)]);
        let operation = |id: String| Operation {
            device_id: test_device_id(),
            request_key: format!("key-{id}"),
            id,
            status: "succeeded".into(),
            start_cursor: Cursor::default(),
            result: None,
            error: None,
            artifact: None,
        };
        let mut active = operation("active".into());
        active.status = "running".into();
        store.push_operation(active);
        store.idempotency.insert(
            "key-active".into(),
            IdempotencyRecord {
                fingerprint: "active-fingerprint".into(),
                operation_id: "active".into(),
            },
        );
        store.push_operation(operation("old".into()));
        store.idempotency.insert(
            "key-old".into(),
            IdempotencyRecord {
                fingerprint: "old-fingerprint".into(),
                operation_id: "old".into(),
            },
        );
        store.artifacts.insert(
            "old".into(),
            ArtifactRecord {
                metadata: ArtifactMetadata {
                    size: 12,
                    sha256: "test".into(),
                    content_type: "application/octet-stream".into(),
                },
                path: artifact_path.clone(),
            },
        );

        for index in 0..64 {
            store.push_operation(operation(format!("new-{index}")));
        }

        assert!(store.operations.iter().any(|op| op.id == "active"));
        assert!(store.idempotency.contains_key("key-active"));
        assert_eq!(store.operations.len(), 65);
        assert!(!store.artifacts.contains_key("old"));
        assert!(!store.idempotency.contains_key("key-old"));
        assert!(!artifact_path.exists());
    }
}
