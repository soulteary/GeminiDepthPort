//! Each failure is scoped to a child process and proven by a private marker.
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
pub fn compile(directory: &Path) -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while !root.join("tests/support/io_faults.c").is_file() {
        root = root.parent().unwrap().into();
    }
    let lib = directory.join("io-faults.so");
    let out = Command::new("cc")
        .args([
            "-shared", "-fPIC", "-std=c11", "-Wall", "-Wextra", "-Werror",
        ])
        .arg(root.join("tests/support/io_faults.c"))
        .args(["-ldl", "-o"])
        .arg(&lib)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    lib
}
pub fn inject(command: &mut Command, library: &Path, operation: &str, suffix: &str, marker: &Path) {
    command
        .env("LD_PRELOAD", library)
        .env("GDP_IO_FAIL_OP", operation)
        .env("GDP_IO_FAIL_SUFFIX", suffix)
        .env("GDP_IO_FAULT_MARKER", marker);
}
pub fn assert_injected(marker: &Path, operation: &str) {
    assert_eq!(fs::read_to_string(marker).unwrap(), operation);
}
