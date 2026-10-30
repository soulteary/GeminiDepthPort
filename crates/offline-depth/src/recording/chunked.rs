//! V3 payload extents and durable-prefix checkpoints. A checkpoint never names
//! an open chunk: payload, index and directory entries are synced first.
use super::*;
use std::io::{Read, Seek, SeekFrom, Write};

pub(super) const INDEX_BUFFER_BYTES: usize = 64 * 1024;
pub(super) const CHUNK_PAIRS: u64 = 32;
pub(super) const CHUNK_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CHUNK: u64 = 128 * 1024 * 1024 + 65536;
#[derive(Default)]
pub(super) struct Chunked {
    pub active: Option<Chunk>,
    pub chunks: u64,
    pub index_bytes: u64,
    pub index_buffer: Vec<u8>,
    pipeline: bool,
    pending: Option<PendingCheckpoint>,
    checkpoint_bytes: u64,
    verified: Option<VerifiedPrefix>,
}
struct PendingCheckpoint {
    task: std::thread::JoinHandle<Result<VerifiedPrefix>>,
    checkpoint: Checkpoint,
    raw: Vec<u8>,
    payload_bytes: u64,
    pairs: u64,
}
impl Chunked {
    pub(super) fn pending_credits(&self) -> (u64, u64) {
        self.pending
            .as_ref()
            .map_or((0, 0), |p| (p.pairs, p.payload_bytes))
    }
}
impl Drop for Chunked {
    fn drop(&mut self) {
        // No background read can outlive the writer/directory lifecycle.
        if let Some(pending) = self.pending.take() {
            let _ = pending.task.join();
        }
    }
}
struct VerifiedPrefix {
    imu: ImuProgress,
    count: u64,
    index_bytes: u64,
    digest: ring::digest::Context,
    previous: Option<Progress>,
    position: Option<(u64, u64)>,
}
pub(super) struct Chunk {
    file: fs::File,
    path: String,
    pub bytes: u64,
    pub pairs: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Checkpoint {
    version: u32,
    binding: SnapshotBinding,
    snapshot_sha256: String,
    record_count: u64,
    index_bytes: u64,
    records_sha256: String,
    chunks: u64,
}
fn chunk_name(n: u64) -> String {
    format!("payload-{n:020}.bin")
}
fn chunk_number(path: &str) -> Result<u64> {
    let n = path
        .strip_prefix("payload-")
        .and_then(|s| s.strip_suffix(".bin"))
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or(OfflineError::Invalid("RECORDING_CHUNK_PATH"))?;
    require(n > 0 && path == chunk_name(n), "RECORDING_CHUNK_PATH")?;
    Ok(n)
}
pub(super) struct CachedChunk {
    path: String,
    file: fs::File,
    length: u64,
}
pub(super) fn read_range(
    root: &Path,
    file: &FileRef,
    range: &FileRange,
    max: usize,
    cache: &Mutex<Option<CachedChunk>>,
) -> Result<Vec<u8>> {
    with_range(root, file, range, max, cache, |input, length| {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length as usize)
            .map_err(|_| OfflineError::Invalid("ALLOCATION_FAILED"))?;
        bytes.resize(length as usize, 0);
        input.read_exact(&mut bytes)?;
        Ok(bytes)
    })
}
pub(super) fn with_range<T>(
    root: &Path,
    file: &FileRef,
    range: &FileRange,
    max: usize,
    cache: &Mutex<Option<CachedChunk>>,
    read: impl FnOnce(&mut fs::File, u64) -> Result<T>,
) -> Result<T> {
    chunk_number(&file.path)?;
    require(
        range.length > 0 && range.length <= max as u64,
        "RECORDING_RANGE_LIMIT",
    )?;
    let end = range
        .offset
        .checked_add(range.length)
        .ok_or(OfflineError::Invalid("RECORDING_RANGE_OVERFLOW"))?;
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if cache.as_ref().is_none_or(|c| c.path != file.path) {
        let path = member(root, &file.path)?;
        require(fs::metadata(&path)?.is_file(), "SOURCE_NOT_FILE")?;
        let input = fs::File::open(path)?;
        let meta = input.metadata()?;
        require(
            meta.is_file() && meta.len() <= MAX_CHUNK,
            "RECORDING_RANGE_BOUNDS",
        )?;
        *cache = Some(CachedChunk {
            path: file.path.clone(),
            file: input,
            length: meta.len(),
        });
    }
    let input = cache.as_mut().unwrap();
    require(end <= input.length, "RECORDING_RANGE_BOUNDS")?;
    input.file.seek(SeekFrom::Start(range.offset))?;
    read(&mut input.file, range.length)
}
impl RecordingDirectorySource {
    /// Read-only recovery of the last durable prefix. No source file is changed,
    /// no COMPLETE is created, and process() forbids exporting it as a full job.
    pub fn open_recovered(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        require(!root.join("COMPLETE").exists(), "RECOVERY_COMPLETED_INPUT")?;
        let checkpoint: Checkpoint =
            parse(&read_bounded(&member(&root, "checkpoint.json")?, 65536)?)?;
        require(
            checkpoint.version == 3
                && checkpoint.record_count > 0
                && checkpoint.chunks > 0
                && checkpoint.index_bytes >= checkpoint.record_count
                && digest_valid(&checkpoint.records_sha256),
            "RECOVERY_CHECKPOINT",
        )?;
        let snapshot = read_bounded(&member(&root, "snapshot.json")?, 4 * 1024 * 1024)?;
        require(
            sha256(&snapshot) == checkpoint.snapshot_sha256,
            "RECOVERY_SNAPSHOT_DIGEST",
        )?;
        require(
            fs::metadata(member(&root, "records.jsonl")?)?.len() >= checkpoint.index_bytes,
            "RECOVERY_INDEX_TRUNCATED",
        )?;
        let manifest = RecordingManifest {
            version: 3,
            source_kind: "recording-directory-v3".into(),
            snapshot: "snapshot.json".into(),
            binding: checkpoint.binding,
            records: "records.jsonl".into(),
            records_sha256: checkpoint.records_sha256,
            record_count: checkpoint.record_count,
        };
        let mut source = Self::open_manifest(root, manifest, checkpoint.index_bytes, true)?;
        source.recovered_chunks = Some(checkpoint.chunks);
        Ok(source)
    }
    pub(super) fn finish_chunk(&self) -> Result<()> {
        if let Some((number, end)) = self.chunk_position {
            require(
                fs::metadata(member(&self.root, &chunk_name(number))?)?.len() == end,
                "RECORDING_CHUNK_TRAILING_BYTES",
            )?;
        }
        Ok(())
    }
    pub(super) fn check_ranges(&mut self, record: &RecordingRecord) -> Result<()> {
        let refs = [&record.color.file, &record.depth.file, &record.evidence];
        if self.manifest.version == 2 {
            return require(
                refs.iter().all(|r| r.range.is_none()),
                "RECORDING_RANGE_VERSION",
            );
        }
        require(
            refs.iter().all(|r| r.range.is_some())
                && record.color.file.path == record.depth.file.path
                && record.color.file.path == record.evidence.path,
            "RECORDING_CHUNK_PAIR",
        )?;
        require(
            record.evidence.range.as_ref().unwrap().length <= 65536,
            "RECORDING_EVIDENCE_LIMIT",
        )?;
        for reference in refs {
            let number = chunk_number(&reference.path)?;
            let range = reference.range.as_ref().unwrap();
            let expected = match self.chunk_position {
                None => {
                    require(number == 1, "RECORDING_CHUNK_SEQUENCE")?;
                    0
                }
                Some((previous, end)) if previous == number => end,
                Some((previous, _)) => {
                    self.finish_chunk()?;
                    require(
                        previous.checked_add(1) == Some(number),
                        "RECORDING_CHUNK_SEQUENCE",
                    )?;
                    0
                }
            };
            require(
                range.offset == expected && range.length > 0,
                "RECORDING_RANGE_ORDER",
            )?;
            let end = range
                .offset
                .checked_add(range.length)
                .ok_or(OfflineError::Invalid("RECORDING_RANGE_OVERFLOW"))?;
            require(end <= MAX_CHUNK, "RECORDING_RANGE_LIMIT")?;
            self.chunk_position = Some((number, end));
        }
        Ok(())
    }
}
impl RawRecordingWriter {
    pub fn begin_chunked(
        parent: &Path,
        name: &str,
        snapshot: SnapshotEnvelope,
        max_bytes: u64,
    ) -> Result<Self> {
        Self::begin_chunked_with_diagnostics(
            parent,
            name,
            snapshot,
            max_bytes,
            RawRecordingDiagnostics::default(),
        )
    }
    pub fn begin_chunked_with_diagnostics(
        parent: &Path,
        name: &str,
        snapshot: SnapshotEnvelope,
        max_bytes: u64,
        diagnostics: RawRecordingDiagnostics,
    ) -> Result<Self> {
        let mut writer =
            Self::begin_with_diagnostics(parent, name, snapshot, max_bytes, diagnostics)?;
        writer.chunked = Some(Chunked::default());
        Ok(writer)
    }
    /// Opt in before the first append. Keeps global uncommitted payload at
    /// 32 pairs / 16 MiB (or one indivisible oversized pair).
    pub fn enable_checkpoint_pipeline(&mut self) -> Result<()> {
        require(
            self.count == 0 && !self.failed,
            "RECORDING_PIPELINE_START_ONLY",
        )?;
        let state = self
            .chunked
            .as_mut()
            .ok_or(OfflineError::Invalid("RECORDING_PIPELINE_REQUIRES_V3"))?;
        state.pipeline = true;
        Ok(())
    }
    pub(super) fn automatic_checkpoint(&mut self) -> Result<()> {
        if self.chunked.as_ref().is_some_and(|s| s.pipeline) {
            self.complete_pending()?;
            self.seal_checkpoint(true)
        } else {
            self.checkpoint()
        }
    }
    pub(super) fn chunk_limits(&self) -> (u64, u64) {
        if self.chunked.as_ref().is_some_and(|s| s.pipeline) {
            (CHUNK_PAIRS / 2, CHUNK_BYTES / 2)
        } else {
            (CHUNK_PAIRS, CHUNK_BYTES)
        }
    }
    pub(super) fn prepare_chunk(
        &mut self,
        color: usize,
        depth: usize,
        evidence: usize,
    ) -> Result<()> {
        if self.chunked.is_none() {
            return Ok(());
        }
        require(evidence <= 65536, "RECORDING_EVIDENCE_LIMIT")?;
        let length = color
            .checked_add(depth)
            .and_then(|n| n.checked_add(evidence))
            .ok_or(OfflineError::Invalid("RECORDING_RANGE_OVERFLOW"))? as u64;
        require(length <= MAX_CHUNK, "RECORDING_RANGE_LIMIT")?;
        let state = self.chunked.as_ref().unwrap();
        if state.pending.as_ref().is_some_and(|p| p.task.is_finished()) {
            self.complete_pending()?;
        }
        let state = self.chunked.as_ref().unwrap();
        let active_bytes = state.active.as_ref().map_or(0, |c| c.bytes);
        let active_pairs = state.active.as_ref().map_or(0, |c| c.pairs);
        if state.pending.as_ref().is_some_and(|p| {
            p.payload_bytes + active_bytes + length > CHUNK_BYTES
                || p.pairs + active_pairs + 1 > CHUNK_PAIRS
        }) {
            self.complete_pending()?;
        }
        if self
            .chunked
            .as_ref()
            .unwrap()
            .active
            .as_ref()
            .is_some_and(|c| c.bytes + length > self.chunk_limits().1)
        {
            self.automatic_checkpoint()?;
        }
        // Sealing above may have created a pending prefix; a large next pair
        // cannot bypass the global credit check.
        if self
            .chunked
            .as_ref()
            .unwrap()
            .pending
            .as_ref()
            .is_some_and(|p| p.payload_bytes + length > CHUNK_BYTES || p.pairs + 1 > CHUNK_PAIRS)
        {
            self.complete_pending()?;
        }
        let state = self.chunked.as_mut().unwrap();
        if state.active.is_none() {
            state.chunks += 1;
            let path = chunk_name(state.chunks);
            let file = self
                .diagnostics
                .measure("write", || new_private(&self.stage.join(&path)))?;
            state.active = Some(Chunk {
                file,
                path,
                bytes: 0,
                pairs: 0,
            });
            self.diagnostics
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .payload_files += 1;
        }
        Ok(())
    }
    pub(super) fn store_pair(&mut self, sequence: u64, parts: [&[u8]; 3]) -> Result<[FileRef; 3]> {
        if self.chunked.is_none() {
            let mut store = |role: &str, suffix: &str, bytes: &[u8]| -> Result<FileRef> {
                let path = format!("{role}-{sequence:020}.{suffix}");
                self.write_budgeted(&path, bytes)?;
                Ok(FileRef {
                    path,
                    sha256: sha256(bytes),
                    range: None,
                })
            };
            return Ok([
                store("color", "raw", parts[0])?,
                store("depth", "raw", parts[1])?,
                store("evidence", "json", parts[2])?,
            ]);
        }
        let total = parts.iter().map(|b| b.len()).sum();
        self.reserve_bytes(total)?;
        let chunk = self.chunked.as_mut().unwrap().active.as_mut().unwrap();
        let mut slices = parts.map(std::io::IoSlice::new);
        let mut calls = 0;
        let result = self.diagnostics.measure("write", || {
            write_all_vectored(&mut chunk.file, &mut slices, &mut calls)
        });
        self.diagnostics
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .payload_write_calls += calls;
        result?;
        let mut offset = chunk.bytes;
        let refs = parts.map(|bytes| {
            let reference = FileRef {
                path: chunk.path.clone(),
                sha256: sha256(bytes),
                range: Some(FileRange {
                    offset,
                    length: bytes.len() as u64,
                }),
            };
            offset += bytes.len() as u64;
            reference
        });
        chunk.bytes = offset;
        Ok(refs)
    }
    /// Seal a whole-pair prefix. Nothing can append to the sealed payload file.
    pub fn checkpoint(&mut self) -> Result<()> {
        require(!self.failed, "RECORDING_ALREADY_FAILED")?;
        let result = self.checkpoint_inner();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn checkpoint_inner(&mut self) -> Result<()> {
        self.complete_pending()?;
        self.seal_checkpoint(false)
    }
    fn seal_checkpoint(&mut self, asynchronous: bool) -> Result<()> {
        self.flush_index()?;
        let Some(state) = self.chunked.as_mut() else {
            return Ok(());
        };
        let Some(chunk) = state.active.take() else {
            return Ok(());
        };
        self.diagnostics
            .measure("fsync", || chunk.file.sync_all())?;
        self.diagnostics
            .measure("fsync", || self.records.sync_all())?;
        self.diagnostics
            .measure("fsync", || fs::File::open(&self.stage)?.sync_all())?;
        let checkpoint = Checkpoint {
            version: 3,
            binding: self.snapshot.content.binding.clone(),
            snapshot_sha256: sha256(&self.snapshot.to_bytes()?),
            record_count: self.count,
            index_bytes: state.index_bytes,
            records_sha256: lower_hex(self.digest.clone().finish().as_ref()),
            chunks: state.chunks,
        };
        let prefix = state.verified.take();
        let raw = serde_json::to_vec(&checkpoint)?;
        self.reserve_bytes(raw.len())?;
        if asynchronous {
            let stage = self.stage.clone();
            let diagnostics = self.diagnostics.clone();
            let work: Checkpoint = parse(&raw)?;
            let task = std::thread::Builder::new()
                .name("recording-checkpoint-verify".into())
                .spawn(move || verify_checkpoint(stage, &work, prefix, diagnostics))?;
            self.chunked.as_mut().unwrap().pending = Some(PendingCheckpoint {
                task,
                checkpoint,
                raw,
                payload_bytes: chunk.bytes,
                pairs: chunk.pairs,
            });
            Ok(())
        } else {
            let verified = verify_checkpoint(
                self.stage.clone(),
                &checkpoint,
                prefix,
                self.diagnostics.clone(),
            )?;
            self.commit_checkpoint(checkpoint, raw, verified)
        }
    }
    fn complete_pending(&mut self) -> Result<()> {
        let pending = self.chunked.as_mut().and_then(|s| s.pending.take());
        if let Some(pending) = pending {
            let verified = pending
                .task
                .join()
                .map_err(|_| OfflineError::Invalid("RECORDING_VERIFIER_PANIC"))??;
            self.commit_checkpoint(pending.checkpoint, pending.raw, verified)?;
        }
        Ok(())
    }
    fn commit_checkpoint(
        &mut self,
        checkpoint: Checkpoint,
        raw: Vec<u8>,
        verified: VerifiedPrefix,
    ) -> Result<()> {
        let previous = self.chunked.as_ref().unwrap().checkpoint_bytes;
        let temporary = self.stage.join(".checkpoint.tmp");
        write_private_observed(&temporary, &raw, &self.diagnostics)?;
        self.diagnostics.measure("rename", || {
            fs::rename(&temporary, self.stage.join("checkpoint.json"))
        })?;
        self.diagnostics
            .measure("fsync", || fs::File::open(&self.stage)?.sync_all())?;
        self.used_bytes -= previous;
        let state = self.chunked.as_mut().unwrap();
        state.checkpoint_bytes = raw.len() as u64;
        state.verified = Some(verified);
        let mut stats = self.diagnostics.0.lock().unwrap_or_else(|e| e.into_inner());
        stats.durable_pairs = checkpoint.record_count;
        stats.checkpoints += 1;
        Ok(())
    }
}

/// Count every underlying call, including short writes and EINTR retries.
pub(super) fn write_all_vectored(
    writer: &mut impl Write,
    mut buffers: &mut [std::io::IoSlice<'_>],
    calls: &mut u64,
) -> std::io::Result<()> {
    while buffers.iter().any(|b| !b.is_empty()) {
        *calls = calls.saturating_add(1);
        match writer.write_vectored(buffers) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => std::io::IoSlice::advance_slices(&mut buffers, n),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
pub(super) fn write_all_counted(
    writer: &mut impl Write,
    mut bytes: &[u8],
    calls: &mut u64,
) -> std::io::Result<()> {
    while !bytes.is_empty() {
        *calls = calls.saturating_add(1);
        match writer.write(bytes) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn verify_checkpoint(
    stage: PathBuf,
    checkpoint: &Checkpoint,
    prefix: Option<VerifiedPrefix>,
    diagnostics: RawRecordingDiagnostics,
) -> Result<VerifiedPrefix> {
    diagnostics.measure("verification", || -> Result<VerifiedPrefix> {
        let manifest = RecordingManifest {
            version: 3,
            source_kind: "recording-directory-v3".into(),
            snapshot: "snapshot.json".into(),
            binding: checkpoint.binding.clone(),
            records: "records.jsonl".into(),
            records_sha256: checkpoint.records_sha256.clone(),
            record_count: checkpoint.record_count,
        };
        let mut source = RecordingDirectorySource::open_manifest(
            fs::canonicalize(&stage)?,
            manifest,
            checkpoint.index_bytes,
            false,
        )?;
        if let Some(prefix) = &prefix {
            source
                .reader
                .get_mut()
                .get_mut()
                .seek(SeekFrom::Start(prefix.index_bytes))?;
            source
                .reader
                .get_mut()
                .set_limit(checkpoint.index_bytes - prefix.index_bytes);
            source.seen = prefix.count;
            source.digest = prefix.digest.clone();
            source.previous = prefix.previous.clone();
            source.imu_progress = prefix.imu.clone();
            source.chunk_position = prefix.position;
        }
        while source.validate_next()? {}
        diagnostics.verified(&source);
        Ok(VerifiedPrefix {
            imu: source.imu_progress,
            count: source.seen,
            index_bytes: checkpoint.index_bytes,
            digest: source.digest,
            previous: source.previous,
            position: source.chunk_position,
        })
    })
}

#[cfg(test)]
mod write_tests {
    use super::*;
    use std::{
        collections::VecDeque,
        io::{self, IoSlice},
    };
    struct Scripted {
        plan: VecDeque<io::Result<usize>>,
        bytes: Vec<u8>,
    }
    impl Scripted {
        fn apply(&mut self, buffers: &[IoSlice<'_>]) -> io::Result<usize> {
            let limit = self.plan.pop_front().unwrap_or(Ok(usize::MAX))?;
            let mut left = limit;
            let mut count = 0;
            for b in buffers {
                let n = b.len().min(left);
                self.bytes.extend_from_slice(&b[..n]);
                left -= n;
                count += n;
            }
            Ok(count)
        }
    }
    impl Write for Scripted {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.apply(&[IoSlice::new(bytes)])
        }
        fn write_vectored(&mut self, buffers: &[IoSlice<'_>]) -> io::Result<usize> {
            self.apply(buffers)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn run(writer: &mut Scripted, vectored: bool, calls: &mut u64) -> io::Result<()> {
        if vectored {
            write_all_vectored(
                writer,
                &mut [
                    IoSlice::new(b"abc"),
                    IoSlice::new(b""),
                    IoSlice::new(b"defgh"),
                    IoSlice::new(b"ij"),
                ],
                calls,
            )
        } else {
            write_all_counted(writer, b"abcdefghij", calls)
        }
    }
    #[test]
    fn scalar_and_vectored_writes_retry_interrupts_and_preserve_cross_slice_short_writes() {
        for vectored in [false, true] {
            let mut writer = Scripted {
                plan: vec![
                    Err(io::ErrorKind::Interrupted.into()),
                    Ok(2),
                    Ok(4),
                    Ok(usize::MAX),
                ]
                .into(),
                bytes: Vec::new(),
            };
            let mut calls = 0;
            run(&mut writer, vectored, &mut calls).unwrap();
            assert_eq!(writer.bytes, b"abcdefghij");
            assert_eq!(calls, 4);
        }
    }
    #[test]
    fn zero_and_partial_then_failed_writes_are_errors_and_empty_inputs_make_no_call() {
        for vectored in [false, true] {
            for zero in [false, true] {
                let plan = if zero {
                    vec![Ok(0)]
                } else {
                    vec![Ok(2), Err(io::Error::other("injected write failure"))]
                };
                let mut writer = Scripted {
                    plan: plan.into(),
                    bytes: Vec::new(),
                };
                let mut calls = 0;
                let error = run(&mut writer, vectored, &mut calls).unwrap_err();
                if zero {
                    assert_eq!(error.kind(), io::ErrorKind::WriteZero);
                    assert!(writer.bytes.is_empty());
                    assert_eq!(calls, 1);
                } else {
                    assert_eq!(writer.bytes, b"ab");
                    assert_eq!(calls, 2);
                }
            }
        }
        let mut writer = Scripted {
            plan: VecDeque::new(),
            bytes: Vec::new(),
        };
        let mut calls = 0;
        write_all_counted(&mut writer, b"", &mut calls).unwrap();
        write_all_vectored(&mut writer, &mut [IoSlice::new(b"")], &mut calls).unwrap();
        assert_eq!(calls, 0);
    }
}
