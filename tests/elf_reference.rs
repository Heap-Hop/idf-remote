#![cfg(feature = "desktop")]

mod support;

#[test]
#[ignore = "requires Python 3 and ESPFLASH pointing to espflash 4.5.0"]
fn synthetic_elf_matches_independent_espflash_cli() {
    let reference = std::env::var_os("ESPFLASH").expect("set ESPFLASH to espflash 4.5.0");
    for (chip, riscv) in [("esp32s3", false), ("esp32c6", true)] {
        let temp = tempfile::tempdir().unwrap();
        let elf = temp.path().join("synthetic.elf");
        let bytes = if riscv {
            support::elf_with_descriptor(Some(".flash.appdesc"), true)
        } else {
            support::elf()
        };
        std::fs::write(&elf, bytes).unwrap();
        let output = std::process::Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/elf_reference.py"
            ))
            .args(["--idfr", env!("CARGO_BIN_EXE_idfr"), "--espflash"])
            .arg(&reference)
            .args(["--chip", chip])
            .arg("--elf")
            .arg(elf)
            .arg("--output")
            .arg(temp.path().join("report"))
            .arg("--synthetic")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{chip}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
