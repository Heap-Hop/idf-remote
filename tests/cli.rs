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
