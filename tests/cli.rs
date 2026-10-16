use serde_json::{Value, json};
use std::{fs, process::Command};

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_idfr"))
}

#[test]
fn invalid_request_key_is_rejected_before_server_contact() {
    let output = command()
        .args(["--request-key", "contains spaces", "devices"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("unsupported characters"), "{error}");
    assert!(!error.contains("connect to idf-remote"), "{error}");
}

// Synthetic IDF-format fixture, not executable firmware. Includes IDF's
// string-valued encrypted field and a non-default application offset.
fn build_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut header = vec![0; 24];
    header[0] = 0xe9;
    header[2] = 2;
    header[3] = 0x2f;
    header[12] = 9;
    fs::write(dir.path().join("bootloader.bin"), header).unwrap();
    fs::write(dir.path().join("app.bin"), [1, 2, 3]).unwrap();
    let manifest = json!({
        "write_flash_args": ["--flash_mode", "dio", "--flash_size", "4MB", "--flash_freq", "80m"],
        "flash_settings": {"flash_mode":"dio", "flash_size":"4MB", "flash_freq":"80m"},
        "flash_files": {"0x0":"bootloader.bin", "0x20000":"app.bin"},
        "bootloader": {"offset":"0x0", "file":"bootloader.bin", "encrypted":"false"},
        "app": {"offset":"0x20000", "file":"app.bin", "encrypted":"false"},
        "extra_esptool_args": {"chip":"esp32s3", "before":"default_reset", "after":"hard_reset", "stub":true}
    });
    fs::write(
        dir.path().join("flasher_args.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    dir
}

#[test]
fn exports_relocatable_plan_and_validates_it_from_another_directory() {
    let build = build_fixture();
    let output = command()
        .arg("plan")
        .arg("--build-dir")
        .arg(build.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["segments"].as_array().unwrap().len(), 2);
    let elsewhere = tempfile::tempdir().unwrap();
    let path = elsewhere.path().join("plan.json");
    fs::write(&path, output.stdout).unwrap();
    let checked = command()
        .current_dir(elsewhere.path())
        .arg("plan")
        .arg("--plan")
        .arg(path)
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    assert_eq!(fs::read(build.path().join("app.bin")).unwrap(), [1, 2, 3]);
}

#[test]
fn app_only_does_not_require_other_artifacts() {
    let build = build_fixture();
    fs::remove_file(build.path().join("bootloader.bin")).unwrap();
    let output = command()
        .args(["plan", "--app-only", "--build-dir"])
        .arg(build.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["segments"].as_array().unwrap().len(), 1);
    assert_eq!(plan["segments"][0]["offset"], 0x20000);
}

#[test]
fn named_idf_images_are_repeatable_and_unknown_names_list_choices() {
    let build = build_fixture();
    let output = command()
        .args([
            "plan",
            "--image",
            "app",
            "--image",
            "bootloader",
            "--build-dir",
        ])
        .arg(build.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["segments"].as_array().unwrap().len(), 2);

    let output = command()
        .args(["plan", "--image", "missing", "--build-dir"])
        .arg(build.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("unknown image 'missing'"), "{error}");
    assert!(error.contains("app, bootloader"), "{error}");
}

#[test]
fn full_idf_import_reports_and_filters_an_empty_image() {
    let build = build_fixture();
    fs::write(build.path().join("assets.bin"), []).unwrap();
    let manifest_path = build.path().join("flasher_args.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["flash_files"]["0x300000"] = "assets.bin".into();
    manifest["assets"] = json!({
        "offset": "0x300000",
        "file": "assets.bin",
        "encrypted": "false"
    });
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    let output = command()
        .args(["plan", "--build-dir"])
        .arg(build.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["segments"].as_array().unwrap().len(), 2);
    let warning = String::from_utf8_lossy(&output.stderr);
    assert!(
        warning.contains("skipped empty ESP-IDF image assets"),
        "{warning}"
    );
    assert!(warning.contains("0x300000"), "{warning}");
}

#[test]
fn invalid_flash_input_is_rejected_before_port_access() {
    let build = build_fixture();
    fs::remove_file(build.path().join("app.bin")).unwrap();
    let output = command()
        .args([
            "--json",
            "flash",
            "--port",
            "IDF_REMOTE_TEST_NONEXISTENT_PORT",
            "--build-dir",
        ])
        .arg(build.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("inspect artifact")
    );
    assert!(
        !error["error"]
            .as_str()
            .unwrap()
            .contains("open serial port")
    );
}

#[test]
fn invalid_wait_and_existing_capture_file_are_rejected_before_reset() {
    let output = command()
        .args([
            "reset",
            "--port",
            "IDF_REMOTE_TEST_NONEXISTENT_PORT",
            "--wait",
            "[",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid wait regex"));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("capture.bin");
    fs::write(&path, "keep me").unwrap();
    let output = command()
        .args([
            "reset",
            "--port",
            "IDF_REMOTE_TEST_NONEXISTENT_PORT",
            "--raw-log",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("create raw log"));
    assert_eq!(fs::read_to_string(path).unwrap(), "keep me");
}

#[test]
fn default_build_directory_supports_image_selection() {
    let project = tempfile::tempdir().unwrap();
    let fixture = build_fixture();
    fs::rename(fixture.path(), project.path().join("build")).unwrap();
    for (args, expected_segments) in [
        (vec!["plan"], 2),
        (vec!["plan", "--image", "app"], 1),
        (vec!["plan", "--app-only"], 1),
    ] {
        let output = command()
            .current_dir(project.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            plan["segments"].as_array().unwrap().len(),
            expected_segments
        );
    }
    fs::remove_file(project.path().join("build/app.bin")).unwrap();
    let output = command()
        .current_dir(project.path())
        .arg("flash")
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(error.contains("inspect artifact"), "{error}");
    assert!(!error.contains("connect to idf-remote"), "{error}");
}

#[test]
fn explicit_plan_rejects_build_directory_and_image_options() {
    for subcommand in ["plan", "flash"] {
        for extra in [
            vec!["--build-dir", "build"],
            vec!["--image", "app"],
            vec!["--app-only"],
        ] {
            let output = command()
                .args([subcommand, "--plan", "p.json"])
                .args(extra)
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
        }
    }
}

#[test]
fn read_flash_validates_bounds_and_output_before_contacting_server() {
    let dir = tempfile::tempdir().unwrap();
    let output_path = dir.path().join("flash.bin");
    let invalid = command()
        .args([
            "read-flash",
            "--port",
            "IDF_REMOTE_TEST_NONEXISTENT_PORT",
            "--size",
            "0",
            "--output",
        ])
        .arg(&output_path)
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    let error = String::from_utf8_lossy(&invalid.stderr);
    assert!(error.contains("read size must be positive"), "{error}");
    assert!(!error.contains("connect to idf-remote"), "{error}");

    fs::write(&output_path, b"keep me").unwrap();
    let existing = command()
        .args([
            "read-flash",
            "--port",
            "IDF_REMOTE_TEST_NONEXISTENT_PORT",
            "--size",
            "0x1000",
            "--output",
        ])
        .arg(&output_path)
        .output()
        .unwrap();
    assert!(!existing.status.success());
    let error = String::from_utf8_lossy(&existing.stderr);
    assert!(error.contains("output already exists"), "{error}");
    assert_eq!(fs::read(output_path).unwrap(), b"keep me");
}

#[test]
fn serial_write_validates_payload_before_contacting_server() {
    let empty = command()
        .args([
            "serial-write",
            "--port",
            "IDF_REMOTE_TEST_NONEXISTENT_PORT",
            "--text",
            "",
        ])
        .output()
        .unwrap();
    assert!(!empty.status.success());
    let error = String::from_utf8_lossy(&empty.stderr);
    assert!(error.contains("payload must not be empty"), "{error}");
    assert!(!error.contains("connect to idf-remote"), "{error}");

    let dir = tempfile::tempdir().unwrap();
    let oversized = dir.path().join("oversized.bin");
    fs::write(
        &oversized,
        vec![0; idf_remote::wire::MAX_SERIAL_WRITE_BYTES + 1],
    )
    .unwrap();
    let result = command()
        .args([
            "serial-write",
            "--port",
            "IDF_REMOTE_TEST_NONEXISTENT_PORT",
            "--file",
        ])
        .arg(oversized)
        .output()
        .unwrap();
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains("payload exceeds 64 KiB"), "{error}");
    assert!(!error.contains("connect to idf-remote"), "{error}");
}

#[test]
fn erase_flash_requires_the_exact_destructive_confirmation() {
    let missing = command()
        .args(["erase-flash", "--port", "IDF_REMOTE_TEST_NONEXISTENT_PORT"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    let error = String::from_utf8_lossy(&missing.stderr);
    assert!(error.contains("--confirm"), "{error}");
    assert!(!error.contains("connect to idf-remote"), "{error}");

    let wrong = command()
        .args([
            "erase-flash",
            "--port",
            "IDF_REMOTE_TEST_NONEXISTENT_PORT",
            "--confirm",
            "yes",
        ])
        .output()
        .unwrap();
    assert!(!wrong.status.success());
    let error = String::from_utf8_lossy(&wrong.stderr);
    assert!(error.contains("erase-all-flash"), "{error}");
    assert!(!error.contains("connect to idf-remote"), "{error}");
}

mod support;

fn elf_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("app.elf"), support::elf()).unwrap();
    // Isolate tests from the user's global espflash configuration.
    fs::write(dir.path().join("espflash.toml"), "").unwrap();
    dir
}

#[test]
fn elf_exports_a_relocatable_bundle_with_digests() {
    use sha2::{Digest, Sha256};
    let project = elf_project();
    let output = command()
        .current_dir(project.path())
        .args([
            "plan", "--elf", "app.elf", "--chip", "esp32s3", "--output", "bundle",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bundle = project.path().join("bundle");
    let plan: Value = serde_json::from_slice(&fs::read(bundle.join("plan.json")).unwrap()).unwrap();
    assert_eq!(plan["segments"].as_array().unwrap().len(), 3);
    let summary: Value =
        serde_json::from_slice(&fs::read(bundle.join("artifacts.json")).unwrap()).unwrap();
    for (segment, info) in plan["segments"]
        .as_array()
        .unwrap()
        .iter()
        .zip(summary["segments"].as_array().unwrap())
    {
        let file = std::path::Path::new(segment["file"].as_str().unwrap());
        assert!(file.is_relative());
        let data = fs::read(bundle.join(file)).unwrap();
        assert_eq!(info["offset"], segment["offset"]);
        assert_eq!(info["size"], data.len());
        assert_eq!(
            info["sha256"],
            Sha256::digest(&data)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
    }
    let relocated = tempfile::tempdir().unwrap();
    fs::rename(bundle, relocated.path().join("bundle")).unwrap();
    let checked = command()
        .current_dir(relocated.path())
        .args(["plan", "--plan", "bundle/plan.json"])
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
}

#[test]
fn elf_app_only_uses_the_selected_partition_and_config_precedence() {
    let project = elf_project();
    fs::write(project.path().join("parts.csv"), "nvs,data,nvs,0x9000,0x6000\nphy_init,data,phy,0xf000,0x1000\nfactory,app,factory,0x20000,0x100000\nother,app,ota_0,0x120000,0x100000\n").unwrap();
    fs::write(project.path().join("espflash.toml"), "[idf]\npartition_table = 'parts.csv'\ntarget_app_partition = 'factory'\n[flash]\nmode = 'dio'\nsize = '4MB'\nfrequency = '40MHz'\n").unwrap();
    let output = command()
        .current_dir(project.path())
        .args([
            "plan",
            "--app-only",
            "--elf",
            "app.elf",
            "--chip",
            "esp32s3",
            "--output",
            "bundle",
            "--target-app-partition",
            "other",
            "--flash-freq",
            "80MHz",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["segments"].as_array().unwrap().len(), 1);
    assert_eq!(plan["segments"][0]["offset"], 0x120000);
    assert_eq!(plan["flash_settings"]["frequency"], "80MHz");
    assert_eq!(plan["flash_settings"]["mode"], "dio");
}

#[test]
fn elf_rejects_conflicting_sources_and_requires_persistent_plan_output() {
    let project = elf_project();
    for extra in [
        vec!["--plan", "plan.json"],
        vec!["--build-dir", "build"],
        vec!["--image", "app"],
    ] {
        let output = command()
            .current_dir(project.path())
            .args(["plan", "--elf", "app.elf", "--chip", "esp32s3"])
            .args(extra)
            .output()
            .unwrap();
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(error.contains("cannot be used with"), "{error}");
    }
    let output = command()
        .current_dir(project.path())
        .args(["plan", "--elf", "app.elf", "--chip", "esp32s3"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--output"));
}

#[test]
fn elf_config_typos_and_truncated_bootloaders_fail_before_http() {
    let project = elf_project();
    fs::write(
        project.path().join("espflash.toml"),
        "[flash]\nfrequncy = '40MHz'\n",
    )
    .unwrap();
    let output = command()
        .current_dir(project.path())
        .args([
            "flash",
            "--chip",
            "esp32s3",
            "--monitor",
            "--elf",
            "app.elf",
        ])
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(error.contains("unknown field"), "{error}");
    fs::write(project.path().join("espflash.toml"), "").unwrap();
    fs::write(project.path().join("bad.bin"), [0xe9]).unwrap();
    let output = command()
        .current_dir(project.path())
        .args([
            "flash",
            "--chip",
            "esp32s3",
            "--bootloader",
            "bad.bin",
            "--elf",
            "app.elf",
        ])
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(error.contains("bootloader"), "{error}");
    assert!(
        !error.contains("panicked") && !error.contains("connect to idf-remote"),
        "{error}"
    );
}

#[test]
fn elf_export_preserves_existing_destination() {
    let project = elf_project();
    fs::create_dir(project.path().join("bundle")).unwrap();
    fs::write(project.path().join("bundle/plan.json"), b"keep me").unwrap();
    let output = command()
        .current_dir(project.path())
        .args([
            "plan", "--elf", "app.elf", "--chip", "esp32s3", "--output", "bundle",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        fs::read(project.path().join("bundle/plan.json")).unwrap(),
        b"keep me"
    );
}

#[test]
fn elf_ambiguous_bootloaders_require_explicit_selection() {
    let project = elf_project();
    for (chip, expected) in [
        ("esp32", "--xtal-freq"),
        ("esp32c2", "--xtal-freq"),
        ("esp32p4", "explicit bootloader"),
    ] {
        let output = command()
            .current_dir(project.path())
            .args(["flash", "--elf", "app.elf", "--chip", chip])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains("connect to idf-remote"), "{error}");
    }
}

#[test]
fn elf_invalid_files_and_partition_layouts_fail_without_export() {
    let project = elf_project();
    for (table, extra, expected) in [
        (
            "factory,app,factory,0x10000,0x100000\n",
            vec!["--target-app-partition", "missing"],
            "partition",
        ),
        (
            "nvs,data,nvs,0x9000,0x6000\nfactory,app,factory,0x10000,0x100000\n",
            vec!["--partition-table-offset", "0"],
            "overlap",
        ),
    ] {
        fs::write(project.path().join("parts.csv"), table).unwrap();
        let output = command()
            .current_dir(project.path())
            .args([
                "plan",
                "--elf",
                "app.elf",
                "--chip",
                "esp32s3",
                "--partition-table",
                "parts.csv",
                "--output",
                "bundle",
            ])
            .args(extra)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(expected), "{error}");
        assert!(!project.path().join("bundle").exists());
    }
    for bytes in [vec![], b"not ELF".to_vec(), support::elf()[..100].to_vec()] {
        fs::write(project.path().join("bad.elf"), bytes).unwrap();
        let output = command()
            .current_dir(project.path())
            .args(["flash", "--elf", "bad.elf", "--chip", "esp32s3"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("ELF"), "{error}");
        assert!(!error.contains("panicked"), "{error}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elf_flash_keeps_temporary_images_through_upload_and_cleans_on_error() {
    use axum::{Router, extract::Multipart, http::StatusCode, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let project = elf_project();
    let scratch = project.path().join("scratch");
    fs::create_dir(&scratch).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let app = Router::new().route("/v1/devices", axum::routing::get(|| async {
        axum::Json(json!({"devices": [{
            "id": "dev_fixture", "display_name": "Fixture", "transport": {"kind": "fake", "address": null},
            "status": {"availability": "available", "activity": "idle"}, "capabilities": ["flash"]
        }], "cursors": {}}))
    })).route(
        "/v1/flash",
        post(move |mut multipart: Multipart| async move {
            while let Some(field) = multipart.next_field().await.unwrap() {
                let name = field.name().unwrap().to_owned();
                let data = field.bytes().await.unwrap();
                if name != "metadata" {
                    assert!(!data.is_empty());
                    observed.fetch_add(1, Ordering::SeqCst);
                }
            }
            (StatusCode::BAD_REQUEST, "intentional upload rejection")
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let output = command()
        .current_dir(project.path())
        .env("TMPDIR", &scratch)
        .env("TMP", &scratch)
        .env("TEMP", &scratch)
        .args([
            "--url",
            &url,
            "--device",
            "dev_fixture",
            "flash",
            "--chip",
            "esp32s3",
            "--elf",
            "app.elf",
        ])
        .output()
        .unwrap();
    server.abort();
    assert!(!output.status.success());
    assert_eq!(
        count.load(Ordering::SeqCst),
        3,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_dir(scratch).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_flash_capture_replays_early_boot_without_duplicate_events() {
    use axum::{
        Json, Router,
        extract::Query,
        routing::{get, post},
    };
    use base64::Engine;
    let build = build_fixture();
    let raw = build.path().join("boot.raw");
    let operation = |status: &str| {
        json!({
            "id": "test-flash", "device_id": "dev_fixture", "request_key": "test",
            "status": status, "start_cursor": {"epoch": "fixture", "after": 0},
            "result": {}, "error": null
        })
    };
    let running = operation("running");
    let finished = operation("succeeded");
    let events = json!([
        {"seq": 1, "kind": "raw", "data": {"base64": base64::prelude::BASE64_STANDARD.encode(b"BOOT_ONCE\n")}},
        {"seq": 2, "kind": "log", "data": {"text": "BOOT_ONCE"}}
    ]);
    let app = Router::new()
        .route("/v1/devices", get(|| async { Json(json!({"devices": [{
            "id": "dev_fixture", "display_name": "Fixture", "transport": {"kind": "fake", "address": null},
            "status": {"availability": "available", "activity": "idle"}, "capabilities": ["flash"]
        }], "cursors": {}})) }))
        .route("/v1/flash", post(move || { let value = running.clone(); async { Json(value) } }))
        .route("/v1/operations/test-flash", get(move || { let value = finished.clone(); async { Json(value) } }))
        .route("/v1/events", get(move |Query(query): Query<std::collections::HashMap<String, String>>| {
            let events = events.clone();
            async move {
                Json(json!({"cursor": {"epoch": "fixture", "after": 2}, "device_status": {"availability": "available", "activity": "idle"},
                    "events": if query.get("after").is_some_and(|v| v == "0") { events } else { json!([]) }}))
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let output = command()
        .args(["--url", &url, "--json", "flash", "--build-dir"])
        .arg(build.path())
        .args(["--wait", "BOOT_ONCE", "--timeout", "1", "--raw-log"])
        .arg(&raw)
        .output()
        .unwrap();
    server.abort();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(raw).unwrap(), b"BOOT_ONCE\n");
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for seq in [1, 2] {
        assert_eq!(events.iter().filter(|event| event["seq"] == seq).count(), 1);
    }
}

#[test]
fn elf_requires_app_descriptor_before_export_or_http() {
    let project = elf_project();
    for (chip, riscv) in [("esp32s3", false), ("esp32c6", true)] {
        fs::write(
            project.path().join("app.elf"),
            support::elf_with_descriptor(None, riscv),
        )
        .unwrap();
        for action in ["plan", "flash"] {
            let output = command()
                .current_dir(project.path())
                .args([
                    "--url",
                    "http://127.0.0.1:1",
                    action,
                    "--elf",
                    "app.elf",
                    "--chip",
                    chip,
                    "--output",
                    "bundle",
                ])
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "{chip} {action} accepted missing descriptor"
            );
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(error.contains("app descriptor"), "{error}");
            assert!(!project.path().join("bundle").exists());
        }
    }
}

#[test]
fn elf_accepts_descriptor_sections_and_symbol() {
    for (chip, riscv) in [("esp32s3", false), ("esp32c6", true)] {
        let project = elf_project();
        for (index, name) in [".flash.appdesc", ".rodata_desc", ".rodata"]
            .into_iter()
            .enumerate()
        {
            fs::write(
                project.path().join("app.elf"),
                support::elf_with_descriptor(Some(name), riscv),
            )
            .unwrap();
            let output = command()
                .current_dir(project.path())
                .args([
                    "plan",
                    "--elf",
                    "app.elf",
                    "--chip",
                    chip,
                    "--output",
                    &format!("bundle{index}"),
                ])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{chip} {name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
fn elf_rejects_data_partition_targets_before_export_or_http() {
    let csv = "nvs,data,nvs,0x9000,0x17000\nfactory,app,factory,0x20000,0x100000\n";
    for table in [None, Some("parts.csv"), Some("parts.bin")] {
        for config in [false, true] {
            for app_only in [false, true] {
                let project = elf_project();
                fs::write(project.path().join("parts.csv"), csv).unwrap();
                let binary = esp_idf_part::PartitionTable::try_from(csv.as_bytes())
                    .unwrap()
                    .to_bin()
                    .unwrap();
                fs::write(project.path().join("parts.bin"), binary).unwrap();
                if config {
                    let setting = table
                        .map(|path| format!("partition_table = '{path}'\n"))
                        .unwrap_or_default();
                    fs::write(
                        project.path().join("espflash.toml"),
                        format!("[idf]\n{setting}target_app_partition = 'nvs'\n"),
                    )
                    .unwrap();
                }
                for action in ["plan", "flash"] {
                    let mut cmd = command();
                    cmd.current_dir(project.path()).args([
                        "--url",
                        "http://127.0.0.1:1",
                        action,
                        "--elf",
                        "app.elf",
                        "--chip",
                        "esp32s3",
                        "--output",
                        "bundle",
                    ]);
                    if !config {
                        cmd.args(["--target-app-partition", "nvs"]);
                        if let Some(table) = table {
                            cmd.args(["--partition-table", table]);
                        }
                    }
                    if app_only {
                        cmd.arg("--app-only");
                    }
                    let output = cmd.output().unwrap();
                    assert!(!output.status.success(), "accepted data partition");
                    let error = String::from_utf8_lossy(&output.stderr);
                    assert!(error.contains("not an app partition"), "{error}");
                    assert!(!project.path().join("bundle").exists());
                }
            }
        }
    }
}

#[test]
fn elf_default_partition_selection_skips_data_named_factory() {
    let csv = "factory,data,nvs,0x9000,0x17000\nmain,app,factory,0x20000,0x100000\n";
    for binary in [false, true] {
        for app_only in [false, true] {
            let project = elf_project();
            let data = if binary {
                esp_idf_part::PartitionTable::try_from(csv.as_bytes())
                    .unwrap()
                    .to_bin()
                    .unwrap()
            } else {
                csv.as_bytes().to_vec()
            };
            fs::write(project.path().join("parts"), data).unwrap();
            let mut cmd = command();
            cmd.current_dir(project.path()).args([
                "plan",
                "--elf",
                "app.elf",
                "--chip",
                "esp32s3",
                "--partition-table",
                "parts",
                "--output",
                "bundle",
            ]);
            if app_only {
                cmd.arg("--app-only");
            }
            let output = cmd.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                plan["segments"].as_array().unwrap().last().unwrap()["offset"],
                0x20000
            );
        }
    }
}
