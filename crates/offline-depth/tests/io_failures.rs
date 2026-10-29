#![cfg(target_os = "linux")]
mod support;
use std::{fs, path::PathBuf, process::Command};
#[test]
fn real_output_io_failures_never_report_success_and_keep_source_bytes_unchanged() {
    let source = tempfile::tempdir().unwrap();
    support::fixture(source.path(), false, 0.);
    let evidence = fs::read(source.path().join("pair.json")).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let lib = temp.path().join("io-faults.so");
    let c = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/support/io_faults.c");
    let out = Command::new("cc")
        .args([
            "-shared", "-fPIC", "-std=c11", "-Wall", "-Wextra", "-Werror",
        ])
        .arg(c)
        .args(["-ldl", "-o"])
        .arg(&lib)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for (i, (op, suffix)) in [
        ("write", "/COMPLETE"),
        ("open", "/depth.tar.partial"),
        ("pthread_create", "raw-writer"),
        ("fsync", "/frames"),
        ("fsync", "/output"),
        ("write", "/pair-evidence-00000000000000000001.json"),
    ]
    .into_iter()
    .enumerate()
    {
        let parent = temp.path().join(format!("case-{i}/output"));
        fs::create_dir_all(&parent).unwrap();
        let marker = parent.join("injected");
        let out = Command::new(env!("CARGO_BIN_EXE_offline-depth"))
            .arg("--input")
            .arg(source.path())
            .arg("--output")
            .arg(&parent)
            .args(["--job", "failure", "--mode", "both"])
            .env("LD_PRELOAD", &lib)
            .env("GDP_IO_FAULT_MARKER", &marker)
            .env("GDP_IO_FAIL_OP", op)
            .env("GDP_IO_FAIL_SUFFIX", suffix)
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "{op} {suffix}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert_eq!(fs::read_to_string(&marker).unwrap(), op);
        assert!(!String::from_utf8_lossy(&out.stdout).contains("\"complete\":true"));
        assert_eq!(fs::read(source.path().join("pair.json")).unwrap(), evidence);
        if suffix != "/output" {
            assert!(!parent.join("failure/COMPLETE").exists());
        }
    }
}
