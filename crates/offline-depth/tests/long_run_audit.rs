#[allow(dead_code)]
#[path = "../examples/audit_imu_long_run.rs"]
mod auditor;
mod support;
use auditor::audit;
use offline_depth::{StorageEndian, recording::RawRecordingWriter};
use std::fs;
#[test]
fn audit_reads_complete_and_recovered_data_twice_and_rejects_corruption() {
    let source = tempfile::tempdir().unwrap();
    support::fixture(source.path(), true, 0.);
    let snapshot: capture_contract::snapshot::SnapshotEnvelope =
        serde_json::from_slice(&fs::read(source.path().join("snapshot.json")).unwrap()).unwrap();
    let mut pair: frame_core::BoundPairEvidence =
        serde_json::from_slice(&fs::read(source.path().join("pair.json")).unwrap()).unwrap();
    let pixels = fs::read(source.path().join("depth.raw16")).unwrap();
    for global in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut writer =
            RawRecordingWriter::begin_chunked(root.path(), "capture", snapshot.clone(), 1_000_000)
                .unwrap();
        pair.color.context.global_timestamp_us = global.then_some(1_700_000_000_000_000);
        pair.depth.context.global_timestamp_us = pair.color.context.global_timestamp_us;
        writer
            .append(&pair, &[1, 2, 3, 4], &pixels, StorageEndian::Little)
            .unwrap();
        writer.checkpoint().unwrap();
        let partial = root.path().join("capture.partial");
        let mut output = Vec::new();
        assert_eq!(audit(&partial, true, &mut output).is_ok(), global);
        assert!(audit(&partial, false, &mut Vec::new()).is_err());
        let path = writer.finish().unwrap();
        if !global {
            assert!(audit(&path, false, &mut Vec::new()).is_err());
            continue;
        }
        output.clear();
        audit(&path, false, &mut output).unwrap();
        let rows: Vec<serde_json::Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|v| serde_json::from_str(v).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["color"]["stamp_ns"], 1_700_000_000_000_000_000u64);
        assert_eq!(rows[1]["independent_audit_passes"], 2);
        assert_eq!(rows[1]["recovered"], false);
        assert!(audit(&path, true, &mut Vec::new()).is_err());
        let payload = path.join("payload-00000000000000000001.bin");
        let mut bytes = fs::read(&payload).unwrap();
        bytes[0] ^= 1;
        fs::write(payload, bytes).unwrap();
        assert!(audit(&path, false, &mut Vec::new()).is_err());
    }
}
