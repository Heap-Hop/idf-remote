//! JNI example adapter. All protocol and HTTP work lives in idf_remote.
use anyhow::{Context, Result, ensure};
use idf_remote::{
    application::Command,
    backend::android::AndroidUsbBackend,
    device::DeviceId,
    server::{Service, Worker},
    wire::{ApplicationRequest, Cursor, DeviceRequest, Operation, SerialWriteRequest},
};
use jni::{
    JNIEnv,
    objects::{JByteArray, JClass, JString},
    sys::{jint, jlong, jstring},
};
use std::{
    collections::HashMap,
    os::fd::BorrowedFd,
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::runtime::Runtime;
struct Gateway {
    service: Service,
    backend: Arc<AndroidUsbBackend>,
    device: DeviceId,
    cursor: Mutex<Cursor>,
    running: Arc<AtomicBool>,
    worker: Option<Worker>,
    http: tokio::task::JoinHandle<()>,
    runtime: Runtime,
}
impl Drop for Gateway {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        self.http.abort();
        if let Some(worker) = self.worker.take() {
            worker.join();
        }
        self.backend.detach(&self.device);
    }
}
static GATEWAYS: OnceLock<Mutex<HashMap<i64, Arc<Gateway>>>> = OnceLock::new();
static NEXT: AtomicI64 = AtomicI64::new(1);
static REQUEST: AtomicU64 = AtomicU64::new(1);
fn registry() -> &'static Mutex<HashMap<i64, Arc<Gateway>>> {
    GATEWAYS.get_or_init(Default::default)
}
fn gateway(id: i64) -> Result<Arc<Gateway>> {
    registry()
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .context("Android gateway is closed")
}
fn key() -> String {
    format!("android-{}", REQUEST.fetch_add(1, Ordering::Relaxed))
}
fn finish(g: &Gateway, operation: Operation) -> Result<Operation> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let current = g
            .service
            .get_operation(&operation.id)
            .context("operation expired")?;
        match current.status.as_str() {
            "succeeded" => return Ok(current),
            "failed" => anyhow::bail!("{}", current.error.unwrap_or_default()),
            _ => {}
        }
        ensure!(
            Instant::now() < deadline,
            "operation still pending; do not replay commands"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn open(fd: i32, cache: &str) -> Result<i64> {
    ensure!(fd >= 0, "invalid USB fd");
    let backend = Arc::new(AndroidUsbBackend::with_cache_dir(cache));
    // Duplicate only while Java keeps UsbDeviceConnection alive.
    let owned = unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?;
    let device = backend.attach(owned)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    // Bind first: a port collision must not leak a running device worker.
    let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:38473"))?;
    let running = Arc::new(AtomicBool::new(true));
    let (service, worker) = Service::start_many_in(
        backend.clone(),
        vec![device.clone()],
        None,
        running.clone(),
        Some(Path::new(cache)),
    )?;
    let http_service = service.clone();
    let http = runtime.spawn(async move {
        let _ = axum::serve(listener, idf_remote::server::router(http_service)).await;
    });
    let mut g = Gateway {
        service,
        backend,
        device: device.id.clone(),
        cursor: Mutex::new(Cursor {
            epoch: String::new(),
            after: 0,
        }),
        running,
        worker: Some(worker),
        http,
        runtime,
    };
    let op = g.runtime.block_on(g.service.submit_monitor(
        DeviceRequest {
            device_id: device.id,
            monitor_baud: 115200,
            no_reset_before: true,
        },
        &key(),
    ))?;
    *g.cursor.get_mut().unwrap() = op.start_cursor.clone();
    finish(&g, op)?;
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    registry().lock().unwrap().insert(id, Arc::new(g));
    Ok(id)
}
fn throw(env: &mut JNIEnv, e: anyhow::Error) {
    let _ = env.throw_new("java/io/IOException", format!("{e:#}"));
}
fn string_result(env: &mut JNIEnv, result: Result<String>) -> jstring {
    match result.and_then(|s| Ok(env.new_string(s)?.into_raw())) {
        Ok(s) => s,
        Err(e) => {
            throw(env, e);
            std::ptr::null_mut()
        }
    }
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_idfremote_android_Native_open(
    mut env: JNIEnv,
    _: JClass,
    fd: jint,
    cache: JString,
) -> jlong {
    let result = (|| {
        let cache: String = env.get_string(&cache)?.into();
        open(fd, &cache)
    })();
    match result {
        Ok(id) => id,
        Err(e) => {
            throw(&mut env, e);
            0
        }
    }
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_idfremote_android_Native_poll(
    mut env: JNIEnv,
    _: JClass,
    id: jlong,
) -> jstring {
    let result = (|| {
        let g = gateway(id)?;
        let mut cursor = g.cursor.lock().unwrap();
        let batch = g.service.application_events(&g.device, &cursor)?;
        let output = serde_json::to_string(&batch)?;
        *cursor = batch.cursor;
        Ok(output)
    })();
    string_result(&mut env, result)
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_idfremote_android_Native_write(
    mut env: JNIEnv,
    _: JClass,
    id: jlong,
    bytes: JByteArray,
) -> jstring {
    let result = (|| {
        let g = gateway(id)?;
        let bytes = env.convert_byte_array(bytes)?;
        ensure!(bytes.len() <= 4096, "sample input exceeds 4096 bytes");
        let request = SerialWriteRequest::from_bytes(g.device.clone(), &bytes, 115200, 2000)?;
        let op = g
            .runtime
            .block_on(g.service.submit_serial_write(request, &key()))?;
        Ok(serde_json::to_string(&finish(&g, op)?)?)
    })();
    string_result(&mut env, result)
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_idfremote_android_Native_application(
    mut env: JNIEnv,
    _: JClass,
    id: jlong,
    method: JString,
    params: JString,
) -> jstring {
    let result = (|| {
        let g = gateway(id)?;
        let method: String = env.get_string(&method)?.into();
        let params: String = env.get_string(&params)?.into();
        let command = if method.is_empty() {
            None
        } else {
            Some(Command {
                method,
                params: serde_json::from_str(&params)?,
            })
        };
        let request = ApplicationRequest {
            device_id: g.device.clone(),
            monitor_baud: 115200,
            timeout_ms: 3000,
            command,
        };
        let op = g
            .runtime
            .block_on(g.service.submit_application(request, &key()))?;
        Ok(serde_json::to_string(&finish(&g, op)?)?)
    })();
    string_result(&mut env, result)
}
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_idfremote_android_Native_close(_: JNIEnv, _: JClass, id: jlong) {
    let g = registry().lock().unwrap().remove(&id);
    drop(g);
}
