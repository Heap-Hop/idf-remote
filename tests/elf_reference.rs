#![cfg(feature = "desktop")]

mod support;

#[test]
#[ignore = "requires Python 3 and ESPFLASH pointing to espflash 4.5.0"]
fn synthetic_elf_matches_independent_espflash_cli() {
    let reference = std::env::var_os("ESPFLASH").expect("set ESPFLASH to espflash 4.5.0");
    let temp = tempfile::tempdir().unwrap();
    let elf = temp.path().join("synthetic.elf");
    std::fs::write(&elf, support::elf()).unwrap();
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/elf_reference.py"
        ))
        .args(["--idfr", env!("CARGO_BIN_EXE_idfr"), "--espflash"])
        .arg(reference)
        .arg("--elf")
        .arg(elf)
        .arg("--output")
        .arg(temp.path().join("report"))
        .arg("--synthetic")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
