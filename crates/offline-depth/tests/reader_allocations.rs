//! Common driver compiled against both source revisions. Only the implementation
//! of the recording writer/reader changes; fixtures and RSS measurement do not.
#![cfg(target_os = "linux")]
mod support;
use capture_contract::{ConfigurationId, ContentKind, content_digest, snapshot::SnapshotEnvelope};
use frame_core::BoundPairEvidence;
use offline_depth::{StorageEndian, recording::*};
use serde_json::json;
use std::{fs, path::PathBuf};
fn fixture(width: u32, height: u32) -> (SnapshotEnvelope, BoundPairEvidence, Vec<u8>) {
    let root = tempfile::tempdir().unwrap();
    support::fixture(root.path(), true, 0.);
    let snapshot: SnapshotEnvelope =
        serde_json::from_slice(&fs::read(root.path().join("snapshot.json")).unwrap()).unwrap();
    let mut pair: BoundPairEvidence =
        serde_json::from_slice(&fs::read(root.path().join("pair.json")).unwrap()).unwrap();
    let mut content = snapshot.content;
    for m in content.calibration.models.values_mut() {
        m.width = width;
        m.height = height;
    }
    content.binding.calibration_id = content.calibration.id().unwrap();
    content.configuration =
        json!({"verification_benchmark":true,"source_depth_scale_mm":f64::from(0.1f32)});
    content.binding.configuration_id = ConfigurationId::parse(
        content_digest(
            ContentKind::Configuration,
            &serde_json::to_vec(&content.configuration).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    let snapshot = content.seal().unwrap();
    for frame in [&mut pair.color, &mut pair.depth] {
        frame.width = width;
        frame.height = height;
        frame.stride = width as usize * 2;
        frame.context.snapshot_id = snapshot.snapshot_id.clone();
        frame.context.calibration_id = snapshot.content.binding.calibration_id.clone();
        frame.context.configuration_id = snapshot.content.binding.configuration_id.clone();
    }
    let mut pixels = vec![0; width as usize * height as usize * 2];
    for p in pixels.chunks_exact_mut(2) {
        p[0] = 10;
    }
    (snapshot, pair, pixels)
}

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
struct Counter;
static ACTIVE: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicU64 = AtomicU64::new(0);
static REALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
fn count(bytes: usize, realloc: bool) {
    if ACTIVE.load(Ordering::Relaxed) {
        CALLS.fetch_add(1, Ordering::Relaxed);
        REALLOCS.fetch_add(u64::from(realloc), Ordering::Relaxed);
        BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        count(l.size(), false);
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        count(l.size(), false);
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        count(n, true);
        unsafe { System.realloc(p, l, n) }
    }
}
#[global_allocator]
static ALLOCATOR: Counter = Counter;
#[test]
#[ignore = "explicit isolated reader allocation count; not a CPU benchmark"]
fn reader_allocation_child() {
    let root = PathBuf::from(std::env::var_os("GDP_VERIFY_ROOT").unwrap());
    let (snapshot, mut pair, pixels) = fixture(640, 400);
    let mut writer =
        RawRecordingWriter::begin_chunked(&root, "capture", snapshot, 256 * 1024 * 1024).unwrap();
    writer.enable_checkpoint_pipeline().unwrap();
    for n in 1..=128 {
        pair.pair_sequence = n;
        for f in [&mut pair.color, &mut pair.depth] {
            f.context.sequence = Some(n);
            f.context.device_timestamp_us = Some(n * 33333);
            f.context.system_timestamp_us = Some(1_000_000 + n * 33333);
            f.context.received_mono_offset_ns = n * 33333000;
        }
        pair.color_stamp_us = n * 33333;
        pair.depth_stamp_us = n * 33333;
        writer
            .append(&pair, &pixels, &pixels, StorageEndian::Little)
            .unwrap();
    }
    let path = writer.finish().unwrap();
    let mut source = RecordingDirectorySource::open(&path).unwrap();
    CALLS.store(0, Ordering::Relaxed);
    REALLOCS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    ACTIVE.store(true, Ordering::Relaxed);
    let mut records = 0;
    while source.validate_next().unwrap() {
        records += 1;
    }
    ACTIVE.store(false, Ordering::Relaxed);
    assert_eq!(records, 128);
    let value = json!({"records":records,"allocation_calls_including_realloc":CALLS.load(Ordering::Relaxed),"reallocations":REALLOCS.load(Ordering::Relaxed),"requested_bytes_cumulative":BYTES.load(Ordering::Relaxed),"scope":"validate_next through EOF only; excludes open/drop; requested bytes not live memory or RSS; separate allocator instrumentation not used for CPU A/B"});
    fs::write(root.join("allocations.json"), value.to_string()).unwrap();
}

#[test]
fn actual_payload_seeks_are_reduced_without_skipping_hash_reads() {
    let root = tempfile::tempdir().unwrap();
    let mut repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while !repo.join("tests/support/reader_seek_counts.c").is_file() {
        repo = repo.parent().unwrap().into();
    }
    let library = root.path().join("reader-seeks.so");
    let output = std::process::Command::new("cc")
        .args([
            "-shared", "-fPIC", "-std=c11", "-Wall", "-Wextra", "-Werror",
        ])
        .arg(repo.join("tests/support/reader_seek_counts.c"))
        .args(["-ldl", "-o"])
        .arg(&library)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let counts = root.path().join("seeks.json");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "reader_allocation_child", "--ignored"])
        .env("GDP_VERIFY_ROOT", root.path())
        .env("LD_PRELOAD", library)
        .env(
            "GDP_READER_SEEK_ARMED",
            root.path().join("capture/COMPLETE"),
        )
        .env("GDP_READER_SEEK_COUNTS", &counts)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&fs::read(counts).unwrap()).unwrap();
    assert_eq!(value["payload_seek_calls"], 256);
}
