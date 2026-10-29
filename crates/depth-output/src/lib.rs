//! Lossless u16 depth storage: millimetres or native counts with explicit scale.
//! The caller supplies the representation; encoding performs no unit conversion.
//! A completed job is committed as one directory; incomplete jobs retain `.partial`.
use capture_contract::{FrameContext, SnapshotId};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, SyncSender, sync_channel},
};

pub type Result<T> = std::result::Result<T, OutputError>;
#[derive(Debug)]
pub enum OutputError {
    Invalid(&'static str),
    Io(io::Error),
    Encode(png::EncodingError),
    Json(serde_json::Error),
    CommitUncertain { path: PathBuf, source: io::Error },
}
impl std::fmt::Display for OutputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for OutputError {}
impl From<io::Error> for OutputError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<png::EncodingError> for OutputError {
    fn from(e: png::EncodingError) -> Self {
        Self::Encode(e)
    }
}
impl From<serde_json::Error> for OutputError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Endian {
    Little,
    Big,
}
/// A borrowed view; trailing row padding is ignored. The final row must include its full stride.
pub struct DepthView<'a> {
    pub bytes: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub endian: Endian,
}
const MAX_PIXELS: usize = 16_777_216;
impl DepthView<'_> {
    pub fn pixels(&self) -> Result<Vec<u16>> {
        let n = (self.width as usize)
            .checked_mul(self.height as usize)
            .ok_or(OutputError::Invalid("DIMENSION_OVERFLOW"))?;
        let row = (self.width as usize)
            .checked_mul(2)
            .ok_or(OutputError::Invalid("DIMENSION_OVERFLOW"))?;
        if n == 0 || n > MAX_PIXELS || self.stride < row {
            return Err(OutputError::Invalid("DEPTH_LAYOUT"));
        }
        let len = self
            .stride
            .checked_mul(self.height as usize)
            .ok_or(OutputError::Invalid("DIMENSION_OVERFLOW"))?;
        if self.bytes.len() != len {
            return Err(OutputError::Invalid("DEPTH_LENGTH"));
        }
        let mut out = Vec::with_capacity(n);
        for row in self.bytes.chunks_exact(self.stride) {
            for p in row[..self.width as usize * 2].chunks_exact(2) {
                out.push(match self.endian {
                    Endian::Little => u16::from_le_bytes([p[0], p[1]]),
                    Endian::Big => u16::from_be_bytes([p[0], p[1]]),
                });
            }
        }
        Ok(out)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DepthKind {
    OriginalMillimetres,
    /// Exact integer samples, with an explicit millimetres-per-count scale.
    OriginalSdkCounts {
        scale_mm: f64,
    },
    D2cColorZMillimetres,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameRecord {
    pub ordinal: u64,
    pub context: FrameContext,
    pub kind: DepthKind,
    pub pair_sequence: u64,
    pub pair_evidence_sha256: String,
}
impl FrameRecord {
    fn validate(&self) -> Result<()> {
        self.context
            .validate()
            .map_err(|_| OutputError::Invalid("FRAME_CONTEXT"))?;
        if matches!(self.kind, DepthKind::OriginalSdkCounts { scale_mm } if !scale_mm.is_finite() || scale_mm <= 0.)
        {
            return Err(OutputError::Invalid("DEPTH_SCALE"));
        }
        if self.pair_sequence == 0
            || self.pair_evidence_sha256.len() != 64
            || !self
                .pair_evidence_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(OutputError::Invalid("PAIR_EVIDENCE"));
        }
        Ok(())
    }
}
#[derive(Debug)]
pub struct EncodedDepth {
    record: FrameRecord,
    width: u32,
    height: u32,
    valid_pixels: u64,
    png: Vec<u8>,
    pixel_sha256: String,
    png_sha256: String,
}
impl EncodedDepth {
    pub fn record(&self) -> &FrameRecord {
        &self.record
    }
    pub fn png_bytes(&self) -> &[u8] {
        &self.png
    }
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub fn valid_pixels(&self) -> u64 {
        self.valid_pixels
    }
    pub fn pixel_sha256(&self) -> &str {
        &self.pixel_sha256
    }
    pub fn png_sha256(&self) -> &str {
        &self.png_sha256
    }
}

fn digest(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .flat_map(|b| {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            [
                HEX[(b >> 4) as usize] as char,
                HEX[(b & 15) as usize] as char,
            ]
        })
        .collect()
}
pub fn encode(view: DepthView<'_>, record: FrameRecord) -> Result<EncodedDepth> {
    record.validate()?;
    let pixels = view.pixels()?;
    let mut canonical = Vec::with_capacity(pixels.len() * 2);
    let mut network = Vec::with_capacity(pixels.len() * 2);
    for p in &pixels {
        canonical.extend_from_slice(&p.to_le_bytes());
        network.extend_from_slice(&p.to_be_bytes());
    }
    let mut bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut bytes, view.width, view.height);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Sixteen);
        let mut writer = enc.write_header()?;
        writer.write_image_data(&network)?;
        writer.finish()?;
    }
    Ok(EncodedDepth {
        record,
        width: view.width,
        height: view.height,
        valid_pixels: pixels.iter().filter(|p| **p != 0).count() as u64,
        pixel_sha256: digest(&canonical),
        png_sha256: digest(&bytes),
        png: bytes,
    })
}
/// Sending blocks when the offline queue is full; no frame dropping policy exists here.
pub fn bounded_queue(
    capacity: usize,
) -> Result<(SyncSender<EncodedDepth>, Receiver<EncodedDepth>)> {
    if capacity == 0 {
        return Err(OutputError::Invalid("QUEUE_CAPACITY"));
    }
    Ok(sync_channel(capacity))
}
#[derive(Clone, Copy, Debug)]
pub enum Mode {
    Directory,
    Tar,
    Both,
}
#[derive(Serialize)]
struct Entry<'a> {
    version: u32,
    unit: &'static str,
    file: String,
    width: u32,
    height: u32,
    valid_pixels: u64,
    pixel_sha256: &'a str,
    png_sha256: &'a str,
    frame: &'a FrameRecord,
}
/// Appends an already-encoded PNG. Used by Job and fault-injection writers.
pub fn append_tar<W: Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    encoded: &EncodedDepth,
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(encoded.png.len() as u64);
    header.set_mode(0o600);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, name, encoded.png.as_slice())?;
    Ok(())
}
pub struct Job {
    stage: PathBuf,
    final_path: PathBuf,
    mode: Mode,
    snapshot: SnapshotId,
    manifest: File,
    tar: Option<tar::Builder<File>>,
    next: u64,
    failed: bool,
    first_context: Option<FrameContext>,
}
fn new_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
impl Job {
    /// Parent must exist. A unique name prevents overwrite; `.partial` is never reused.
    pub fn create(parent: &Path, name: &str, mode: Mode, snapshot: SnapshotId) -> Result<Self> {
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(OutputError::Invalid("JOB_NAME"));
        }
        let final_path = parent.join(name);
        if final_path.exists() {
            return Err(OutputError::Invalid("OUTPUT_EXISTS"));
        }
        let stage = parent.join(format!("{name}.partial"));
        fs::create_dir(&stage)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stage, fs::Permissions::from_mode(0o700))?;
        }
        let manifest = new_file(&stage.join("manifest.jsonl.partial"))?;
        if matches!(mode, Mode::Directory | Mode::Both) {
            fs::create_dir(stage.join("frames"))?;
        }
        let tar = if matches!(mode, Mode::Tar | Mode::Both) {
            Some(tar::Builder::new(new_file(
                &stage.join("depth.tar.partial"),
            )?))
        } else {
            None
        };
        Ok(Self {
            stage,
            final_path,
            mode,
            snapshot,
            manifest,
            tar,
            next: 0,
            failed: false,
            first_context: None,
        })
    }
    /// Private immutable provenance sidecars. Snapshot must be attached before
    /// frames; per-pair evidence can be attached immediately before its frame.
    /// Only strict JSON with unique keys is accepted, up to 4 MiB.
    pub fn stage_path(&self) -> &Path {
        &self.stage
    }
    pub fn attach_json(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        if self.failed {
            return Err(OutputError::Invalid("JOB_FAILED"));
        }
        let result = (|| {
            let recording_name = name
                .strip_prefix("recording-record-")
                .and_then(|s| s.strip_suffix(".json"))
                .is_some_and(|s| s.len() == 20 && s.bytes().all(|b| b.is_ascii_digit()));
            let diagnostic_name = name
                .strip_prefix("d2c-diagnostics-")
                .and_then(|s| s.strip_suffix(".json"))
                .is_some_and(|s| {
                    s.len() == 20
                        && s.bytes().all(|b| b.is_ascii_digit())
                        && s.parse::<u64>().ok() == Some(self.next)
                });
            let pair_name = name
                .strip_prefix("pair-evidence-")
                .and_then(|s| s.strip_suffix(".json"))
                .is_some_and(|s| s.len() == 20 && s.bytes().all(|b| b.is_ascii_digit()));
            if !(name == "build-info.json"
                || diagnostic_name
                || name == "snapshot.json"
                || name == "recording-context.json"
                || pair_name
                || recording_name)
                || ((name == "snapshot.json" || name == "build-info.json") && self.next != 0)
                || ((pair_name
                    || recording_name
                    || diagnostic_name
                    || name == "recording-context.json")
                    && !self.stage.join("snapshot.json").is_file())
            {
                return Err(OutputError::Invalid("SIDECAR_NAME_OR_ORDER"));
            }
            capture_contract::parse_strict(bytes)
                .map_err(|_| OutputError::Invalid("SIDECAR_JSON"))?;
            let mut file = new_file(&self.stage.join(name))?;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
            let _ = fs::write(
                self.stage.join("FAILED"),
                b"sidecar write failed; output incomplete\n",
            );
        }
        result
    }
    /// Immutable content-addressed original recording bytes; never interpreted as
    /// pixels or rewritten. Repeated references may reuse an identical blob.
    pub fn attach_source_blob(&mut self, sha256: &str, bytes: &[u8]) -> Result<()> {
        if self.failed {
            return Err(OutputError::Invalid("JOB_FAILED"));
        }
        let result = (|| {
            if bytes.len() > 64 * 1024 * 1024 || digest(bytes) != sha256 {
                return Err(OutputError::Invalid("SOURCE_BLOB_DIGEST_OR_LIMIT"));
            }
            let directory = self.stage.join("source-blobs");
            match fs::create_dir(&directory) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            let path = directory.join(sha256);
            if path.is_file() {
                return Ok(());
            }
            let mut file = new_file(&path)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
            let _ = fs::write(
                self.stage.join("FAILED"),
                b"source blob failed; output incomplete\n",
            );
        }
        result
    }
    pub fn append(&mut self, frame: &EncodedDepth) -> Result<()> {
        if self.failed {
            return Err(OutputError::Invalid("JOB_FAILED"));
        }
        let result = self.append_inner(frame);
        if result.is_err() {
            self.failed = true;
            let _ = fs::write(
                self.stage.join("FAILED"),
                b"frame append failed; output incomplete\n",
            );
        }
        result
    }
    fn append_inner(&mut self, frame: &EncodedDepth) -> Result<()> {
        frame.record.validate()?;
        if frame.record.ordinal != self.next || frame.record.context.snapshot_id != self.snapshot {
            return Err(OutputError::Invalid("JOB_FRAME_BINDING"));
        }
        if let Some(first) = &self.first_context {
            let current = &frame.record.context;
            if first.session_id != current.session_id
                || first.generation_id != current.generation_id
                || first.configuration_id != current.configuration_id
                || first.calibration_id != current.calibration_id
            {
                return Err(OutputError::Invalid("JOB_CONTEXT_CHANGED"));
            }
        } else {
            self.first_context = Some(frame.record.context.clone());
        }
        let name = format!("{:020}.png", self.next);
        if matches!(self.mode, Mode::Directory | Mode::Both) {
            let path = self.stage.join("frames").join(&name);
            let tmp = path.with_extension("png.partial");
            let mut f = new_file(&tmp)?;
            f.write_all(&frame.png)?;
            f.sync_all()?;
            fs::rename(tmp, path)?;
        }
        if let Some(tar) = self.tar.as_mut() {
            append_tar(tar, &name, frame)?;
        }
        let entry = Entry {
            version: 1,
            unit: if matches!(frame.record.kind, DepthKind::OriginalSdkCounts { .. }) {
                "sdk-counts"
            } else {
                "mm"
            },
            file: name,
            width: frame.width,
            height: frame.height,
            valid_pixels: frame.valid_pixels,
            pixel_sha256: &frame.pixel_sha256,
            png_sha256: &frame.png_sha256,
            frame: &frame.record,
        };
        serde_json::to_writer(&mut self.manifest, &entry)?;
        self.manifest.write_all(b"\n")?;
        self.next = self
            .next
            .checked_add(1)
            .ok_or(OutputError::Invalid("ORDINAL_OVERFLOW"))?;
        Ok(())
    }
    pub fn finish(mut self) -> Result<PathBuf> {
        if self.failed || self.next == 0 {
            return Err(OutputError::Invalid("JOB_INCOMPLETE"));
        }
        if let Some(mut tar) = self.tar.take() {
            tar.finish()?;
            tar.into_inner()?.sync_all()?;
            fs::rename(
                self.stage.join("depth.tar.partial"),
                self.stage.join("depth.tar"),
            )?;
        }
        self.manifest.sync_all()?;
        fs::rename(
            self.stage.join("manifest.jsonl.partial"),
            self.stage.join("manifest.jsonl"),
        )?;
        let mut marker = new_file(&self.stage.join("COMPLETE"))?;
        struct PreparedMarker(PathBuf);
        impl Drop for PreparedMarker {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }
        // A failed pre-publication commit must not leave a prepared success marker.
        let _prepared_marker = PreparedMarker(self.stage.join("COMPLETE"));
        writeln!(
            marker,
            "{{\"version\":1,\"frames\":{},\"snapshot_id\":\"{}\"}}",
            self.next, self.snapshot
        )?;
        marker.sync_all()?;
        File::open(&self.stage)?.sync_all()?;
        if matches!(self.mode, Mode::Directory | Mode::Both) {
            File::open(self.stage.join("frames"))?.sync_all()?;
        }
        if self.stage.join("source-blobs").is_dir() {
            File::open(self.stage.join("source-blobs"))?.sync_all()?;
        }
        // Platform no-replace rename closes the concurrent target-creation race.
        if self.final_path.exists() {
            return Err(OutputError::Invalid("OUTPUT_EXISTS"));
        }
        rename_no_replace(&self.stage, &self.final_path)?;
        if let Err(source) =
            File::open(self.final_path.parent().unwrap()).and_then(|f| f.sync_all())
        {
            return Err(OutputError::CommitUncertain {
                path: self.final_path.clone(),
                source,
            });
        }
        Ok(self.final_path.clone())
    }
}
/// Atomic no-replace publication for an already validated private directory.
/// Caller must sync staged contents and handle parent-directory sync separately.
pub fn rename_directory_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    rename_no_replace(from, to)
}

#[cfg(unix)]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let from = std::ffi::CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path NUL"))?;
    let to = std::ffi::CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path NUL"))?;
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace rename requires Linux or macOS",
    ));
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
#[cfg(not(unix))]
fn rename_no_replace(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace rename requires Linux or macOS",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record() -> FrameRecord {
        FrameRecord {
            ordinal: 0,
            context: FrameContext {
                version: 1,
                session_id: capture_contract::SessionId::parse("a".repeat(32)).unwrap(),
                generation_id: capture_contract::GenerationId::parse("b".repeat(32)).unwrap(),
                configuration_id: capture_contract::ConfigurationId::parse("c".repeat(64)).unwrap(),
                calibration_id: capture_contract::CalibrationId::parse("d".repeat(64)).unwrap(),
                snapshot_id: SnapshotId::parse("e".repeat(64)).unwrap(),
                stream_id: "depth".into(),
                sequence: Some(42),
                device_timestamp_us: Some(10),
                system_timestamp_us: Some(20),
                global_timestamp_us: None,
                received_mono_offset_ns: 30,
            },
            kind: DepthKind::OriginalMillimetres,
            pair_sequence: 1,
            pair_evidence_sha256: "f".repeat(64),
        }
    }
    fn encoded() -> EncodedDepth {
        encode(
            DepthView {
                bytes: &[0, 0, 255, 255, 1, 0],
                width: 3,
                height: 1,
                stride: 6,
                endian: Endian::Little,
            },
            record(),
        )
        .unwrap()
    }
    #[test]
    fn roundtrip_numeric_extremes_endian_and_padding() {
        for (endian, bytes) in [
            (Endian::Little, vec![0, 0, 255, 255, 1, 0, 9, 9]),
            (Endian::Big, vec![0, 0, 255, 255, 0, 1, 9, 9]),
        ] {
            let e = encode(
                DepthView {
                    bytes: &bytes,
                    width: 3,
                    height: 1,
                    stride: 8,
                    endian,
                },
                record(),
            )
            .unwrap();
            let mut reader = png::Decoder::new(e.png.as_slice()).read_info().unwrap();
            let mut out = vec![0; reader.output_buffer_size()];
            let info = reader.next_frame(&mut out).unwrap();
            assert_eq!(info.bit_depth, png::BitDepth::Sixteen);
            assert_eq!(info.color_type, png::ColorType::Grayscale);
            assert_eq!(&out[..info.buffer_size()], &[0, 0, 255, 255, 0, 1]);
            assert_eq!(e.valid_pixels, 2);
        }
        assert!(
            encode(
                DepthView {
                    bytes: &[0],
                    width: 1,
                    height: 1,
                    stride: 2,
                    endian: Endian::Little
                },
                record()
            )
            .is_err()
        );
        assert!(
            encode(
                DepthView {
                    bytes: &[],
                    width: 0,
                    height: 1,
                    stride: 0,
                    endian: Endian::Little
                },
                record()
            )
            .is_err()
        );
    }
    #[test]
    fn directory_and_tar_share_identical_png_and_complete_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let e = encoded();
        let mut job = Job::create(
            tmp.path(),
            "test",
            Mode::Both,
            e.record.context.snapshot_id.clone(),
        )
        .unwrap();
        assert!(!tmp.path().join("test").exists());
        job.append(&e).unwrap();
        let path = job.finish().unwrap();
        assert!(path.join("COMPLETE").exists());
        assert!(!tmp.path().join("test.partial").exists());
        let file = fs::read(path.join("frames/00000000000000000000.png")).unwrap();
        let mut tar = tar::Archive::new(File::open(path.join("depth.tar")).unwrap());
        let mut entries = tar.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
        assert_eq!(file, bytes);
        assert_eq!(bytes, e.png);
        assert!(entries.next().is_none());
        assert!(
            Job::create(
                tmp.path(),
                "test",
                Mode::Both,
                e.record.context.snapshot_id.clone()
            )
            .is_err()
        );
    }
    #[test]
    fn binding_failure_poison_job_and_retains_partial_without_success_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let mut e = encoded();
        let mut job = Job::create(
            tmp.path(),
            "bad",
            Mode::Tar,
            e.record.context.snapshot_id.clone(),
        )
        .unwrap();
        e.record.ordinal = 1;
        assert!(job.append(&e).is_err());
        e.record.ordinal = 0;
        assert!(job.append(&e).is_err());
        assert!(job.finish().is_err());
        assert!(!tmp.path().join("bad").exists());
        assert!(tmp.path().join("bad.partial/FAILED").exists());
        assert!(!tmp.path().join("bad.partial/manifest.jsonl").exists());
    }
    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::StorageFull, "disk full"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn archive_write_error_propagates() {
        assert!(
            append_tar(
                &mut tar::Builder::new(FailingWriter),
                "frame.png",
                &encoded()
            )
            .is_err()
        );
    }
    #[test]
    fn full_offline_queue_blocks_and_retains_order() {
        let (tx, rx) = bounded_queue(1).unwrap();
        tx.send(encoded()).unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            tx.send(encoded()).unwrap();
            done_tx.send(()).unwrap();
        });
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(25))
                .is_err()
        );
        assert_eq!(rx.recv().unwrap().record.ordinal, 0);
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert_eq!(rx.recv().unwrap().record.ordinal, 0);
        thread.join().unwrap();
    }
    #[test]
    fn strict_private_sidecars_are_immutable_and_failures_poison() {
        let tmp = tempfile::tempdir().unwrap();
        let e = encoded();
        let mut job = Job::create(
            tmp.path(),
            "sidecars",
            Mode::Directory,
            e.record.context.snapshot_id.clone(),
        )
        .unwrap();
        job.attach_json("snapshot.json", br#"{"version":1}"#)
            .unwrap();
        job.attach_json("pair-evidence-00000000000000000001.json", br#"{"pair":1}"#)
            .unwrap();
        job.append(&e).unwrap();
        job.attach_json("pair-evidence-00000000000000000002.json", br#"{"pair":2}"#)
            .unwrap();
        let path = job.finish().unwrap();
        assert_eq!(
            fs::read(path.join("snapshot.json")).unwrap(),
            br#"{"version":1}"#
        );
        for (i, name, bytes) in [
            (0, "../escape.json", br#"{}"#.as_slice()),
            (1, "snapshot.json", br#"{"x":1,"x":2}"#),
            (2, "pair-evidence-1.json", br#"{}"#),
        ] {
            let mut job = Job::create(
                tmp.path(),
                &format!("bad{i}"),
                Mode::Directory,
                e.record.context.snapshot_id.clone(),
            )
            .unwrap();
            assert!(job.attach_json(name, bytes).is_err());
            assert!(job.append(&e).is_err());
            assert!(job.finish().is_err());
        }
        let mut job = Job::create(
            tmp.path(),
            "duplicate",
            Mode::Directory,
            e.record.context.snapshot_id.clone(),
        )
        .unwrap();
        job.attach_json("snapshot.json", b"{}").unwrap();
        assert!(job.attach_json("snapshot.json", b"{}").is_err());
        assert!(job.finish().is_err());
        let mut job = Job::create(
            tmp.path(),
            "late",
            Mode::Directory,
            e.record.context.snapshot_id.clone(),
        )
        .unwrap();
        job.append(&e).unwrap();
        assert!(job.attach_json("snapshot.json", b"{}").is_err());
        assert!(job.finish().is_err());
    }
    #[test]
    fn racing_final_target_cleans_prepared_success_marker() {
        let temp = tempfile::tempdir().unwrap();
        let frame = encoded();
        let mut job = Job::create(
            temp.path(),
            "race",
            Mode::Directory,
            frame.record.context.snapshot_id.clone(),
        )
        .unwrap();
        job.append(&frame).unwrap();
        fs::create_dir(temp.path().join("race")).unwrap();
        assert!(job.finish().is_err());
        assert!(!temp.path().join("race.partial/COMPLETE").exists());
        assert_eq!(fs::read_dir(temp.path().join("race")).unwrap().count(), 0);
    }
    #[test]
    fn source_blob_reuse_and_digest_failure_poison_the_whole_output_transaction() {
        let tmp = tempfile::tempdir().unwrap();
        let frame = encoded();
        let mut job = Job::create(
            tmp.path(),
            "blob",
            Mode::Directory,
            frame.record.context.snapshot_id.clone(),
        )
        .unwrap();
        let bytes = b"original native source";
        let sha = digest(bytes);
        job.attach_source_blob(&sha, bytes).unwrap();
        job.attach_source_blob(&sha, bytes).unwrap();
        assert_eq!(
            fs::read(job.stage.join("source-blobs").join(&sha)).unwrap(),
            bytes
        );
        assert!(job.attach_source_blob(&sha, b"changed").is_err());
        assert!(job.attach_source_blob(&sha, bytes).is_err());
        assert!(job.attach_json("extra.json", b"{}").is_err());
        assert!(job.finish().is_err());
        assert!(tmp.path().join("blob.partial/FAILED").is_file());
        assert!(!tmp.path().join("blob").exists());
    }
    #[test]
    fn jobs_reject_mid_recording_context_drift_even_when_snapshot_id_is_unchanged() {
        for key in [
            "session_id",
            "generation_id",
            "configuration_id",
            "calibration_id",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let first = encoded();
            let mut second = encoded();
            second.record.ordinal = 1;
            let mut context = serde_json::to_value(&second.record.context).unwrap();
            context[key] = serde_json::json!("0".repeat(
                if key == "session_id" || key == "generation_id" {
                    32
                } else {
                    64
                }
            ));
            second.record.context = serde_json::from_value(context).unwrap();
            let mut job = Job::create(
                tmp.path(),
                "drift",
                Mode::Both,
                first.record.context.snapshot_id.clone(),
            )
            .unwrap();
            job.append(&first).unwrap();
            assert!(matches!(
                job.append(&second),
                Err(OutputError::Invalid("JOB_CONTEXT_CHANGED"))
            ));
            assert!(job.finish().is_err());
            assert!(!tmp.path().join("drift").exists());
        }
    }
    #[test]
    fn depth_records_reject_invalid_scale_pair_binding_and_unbounded_queues() {
        let view = || DepthView {
            bytes: &[1, 0],
            width: 1,
            height: 1,
            stride: 2,
            endian: Endian::Little,
        };
        for scale in [0., -1., f64::NAN, f64::INFINITY] {
            let mut r = record();
            r.kind = DepthKind::OriginalSdkCounts { scale_mm: scale };
            assert!(matches!(
                encode(view(), r),
                Err(OutputError::Invalid("DEPTH_SCALE"))
            ));
        }
        for (pair, digest) in [
            (0, "a".repeat(64)),
            (1, "wrong".into()),
            (1, "G".repeat(64)),
        ] {
            let mut r = record();
            r.pair_sequence = pair;
            r.pair_evidence_sha256 = digest;
            assert!(matches!(
                encode(view(), r),
                Err(OutputError::Invalid("PAIR_EVIDENCE"))
            ));
        }
        assert!(matches!(
            bounded_queue(0),
            Err(OutputError::Invalid("QUEUE_CAPACITY"))
        ));
    }
    #[test]
    fn encoded_public_metadata_matches_the_numeric_png_and_committed_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let frame = encoded();
        assert_eq!(frame.dimensions(), (3, 1));
        assert_eq!(frame.valid_pixels(), 2);
        let mut job = Job::create(
            tmp.path(),
            "metadata",
            Mode::Both,
            frame.record().context.snapshot_id.clone(),
        )
        .unwrap();
        job.append(&frame).unwrap();
        let path = job.finish().unwrap();
        let row: serde_json::Value =
            serde_json::from_slice(&fs::read(path.join("manifest.jsonl")).unwrap()).unwrap();
        assert_eq!(row["width"], frame.dimensions().0);
        assert_eq!(row["height"], frame.dimensions().1);
        assert_eq!(row["valid_pixels"], frame.valid_pixels());
        let bytes = fs::read(path.join("frames/00000000000000000000.png")).unwrap();
        assert_eq!(bytes, frame.png_bytes());
        assert_eq!(row["png_sha256"], frame.png_sha256());
        assert_eq!(row["pixel_sha256"], frame.pixel_sha256());
        let mut decoder = png::Decoder::new(bytes.as_slice()).read_info().unwrap();
        let mut pixels = vec![0; decoder.output_buffer_size()];
        decoder.next_frame(&mut pixels).unwrap();
        assert_eq!(pixels, [0, 0, 255, 255, 0, 1]);
    }
    #[test]
    fn filesystem_obstacles_poison_output_jobs_without_a_complete_marker() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            Job::create(
                tmp.path(),
                "../escape",
                Mode::Directory,
                record().context.snapshot_id
            )
            .err()
            .unwrap()
            .to_string()
            .contains("JOB_NAME")
        );
        let snapshot = record().context.snapshot_id;
        for mode in 0..3 {
            let name = format!("obstacle{mode}");
            let mut job =
                Job::create(tmp.path(), &name, Mode::Directory, snapshot.clone()).unwrap();
            if mode == 0 {
                fs::write(job.stage_path().join("source-blobs"), b"obstacle").unwrap();
                let error = job
                    .attach_source_blob(&digest(b"data"), b"data")
                    .unwrap_err();
                assert!(error.to_string().contains("Io"));
            } else if mode == 1 {
                fs::remove_dir(job.stage_path().join("frames")).unwrap();
                fs::write(job.stage_path().join("frames"), b"obstacle").unwrap();
                assert!(job.append(&encoded()).is_err());
            } else {
                job.append(&encoded()).unwrap();
                fs::create_dir(tmp.path().join(&name)).unwrap();
                assert!(
                    job.finish()
                        .unwrap_err()
                        .to_string()
                        .contains("OUTPUT_EXISTS")
                );
                continue;
            }
            assert!(
                job.attach_json("build-info.json", b"{}")
                    .unwrap_err()
                    .to_string()
                    .contains("JOB_FAILED")
            );
            assert!(job.finish().is_err());
            assert!(!tmp.path().join(name).join("COMPLETE").exists());
        }
    }
    #[test]
    fn external_encoding_json_and_io_failures_retain_actionable_diagnostics() {
        fn parse(bytes: &[u8]) -> Result<serde_json::Value> {
            Ok(serde_json::from_slice(bytes)?)
        }
        let error = parse(b"{invalid").unwrap_err();
        assert!(error.to_string().contains("line: 1"));
        let mut bytes = Vec::new();
        let encoder = png::Encoder::new(&mut bytes, 0, 1);
        let error = encoder
            .write_header()
            .map_err(OutputError::from)
            .err()
            .unwrap();
        assert!(error.to_string().contains("Encode"));
        let tmp = tempfile::tempdir().unwrap();
        let mut job = Job::create(
            tmp.path(),
            "missing-manifest",
            Mode::Directory,
            record().context.snapshot_id,
        )
        .unwrap();
        job.append(&encoded()).unwrap();
        fs::remove_file(job.stage_path().join("manifest.jsonl.partial")).unwrap();
        assert!(job.finish().unwrap_err().to_string().contains("Io"));
        let mut job = Job::create(
            tmp.path(),
            "missing-tar",
            Mode::Tar,
            record().context.snapshot_id,
        )
        .unwrap();
        job.append(&encoded()).unwrap();
        fs::remove_file(job.stage_path().join("depth.tar.partial")).unwrap();
        assert!(job.finish().is_err());
    }
}
