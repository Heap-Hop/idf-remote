//! Embedded mode: owns USB directly, without binding any HTTP listener.
//! Stop a daemon using this same port before running this example.
use anyhow::{Context, Result};
use idf_remote::{
    application::Command,
    backend::EspflashBackend,
    server::Service,
    wire::{ApplicationRequest, Operation},
};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

async fn finish(service: &Service, operation: Operation) -> Result<Operation> {
    loop {
        let current = service
            .get_operation(&operation.id)
            .context("operation disappeared")?;
        match current.status.as_str() {
            "succeeded" => return Ok(current),
            "failed" => anyhow::bail!("{}", current.error.unwrap_or_default()),
            _ => tokio::time::sleep(Duration::from_millis(2)).await,
        }
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    let port = std::env::args()
        .nth(1)
        .context("usage: embedded_gateway SERIAL_PORT")?;
    let backend = Arc::new(EspflashBackend::default());
    let device = backend.device_for_address(&port)?;
    let running = Arc::new(AtomicBool::new(true));
    let (service, worker) = Service::start(backend, device.clone(), None, running.clone())?;
    let result: Result<()> = async {
        let request = ApplicationRequest {
            device_id: device.id.clone(),
            monitor_baud: 115200,
            timeout_ms: 2000,
            command: None,
        };
        let connected = finish(
            &service,
            service
                .submit_application(request.clone(), "embedded-connect")
                .await?,
        )
        .await?;
        println!("identity: {}", connected.result.unwrap());
        let request = ApplicationRequest {
            command: Some(Command {
                method: "echo".into(),
                params: json!({"from":"embedded Rust library"}),
            }),
            ..request
        };
        let response = finish(
            &service,
            service.submit_application(request, "embedded-echo").await?,
        )
        .await?;
        println!("response: {}", response.result.unwrap());
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let events = service.application_events(&device.id, &connected.start_cursor)?;
        for event in events.events {
            if matches!(event.kind.as_str(), "log" | "application_event") {
                println!("{}: {}", event.kind, event.data);
            }
        }
        Ok(())
    }
    .await;
    running.store(false, Ordering::Relaxed);
    worker.join();
    result
}
