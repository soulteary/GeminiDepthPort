//! V3 payload extents and durable-prefix checkpoints. A checkpoint never names
//! an open chunk: payload, index and directory entries are synced first.
use super::*;
use std::io::{Read, Seek, SeekFrom, Write};

pub(super) const CHUNK_PAIRS: u64 = 32;
pub(super) const CHUNK_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CHUNK: u64 = 128 * 1024 * 1024 + 65536;
#[derive(Default)]
pub(super) struct Chunked {
    pub active: Option<Chunk>,
    pub chunks: u64,
    pub index_bytes: u64,
    checkpoint_bytes: u64,
    verified: Option<VerifiedPrefix>,
}
struct VerifiedPrefix {
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
        if self
            .chunked
            .as_ref()
            .unwrap()
            .active
            .as_ref()
            .is_some_and(|c| c.bytes + length > CHUNK_BYTES)
        {
            self.checkpoint()?;
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
    pub(super) fn store_blob(&mut self, path: &str, bytes: &[u8]) -> Result<FileRef> {
        if self.chunked.is_none() {
            self.write_budgeted(path, bytes)?;
            return Ok(FileRef {
                path: path.into(),
                sha256: sha256(bytes),
                range: None,
            });
        }
        self.reserve_bytes(bytes.len())?;
        let chunk = self.chunked.as_mut().unwrap().active.as_mut().unwrap();
        let range = FileRange {
            offset: chunk.bytes,
            length: bytes.len() as u64,
        };
        self.diagnostics
            .measure("write", || chunk.file.write_all(bytes))?;
        chunk.bytes += bytes.len() as u64;
        Ok(FileRef {
            path: chunk.path.clone(),
            sha256: sha256(bytes),
            range: Some(range),
        })
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
        // Reuse the full source validator, but scan only the newly sealed chunk.
        // Cached progress and the index digest preserve cross-chunk checks in O(n).
        let verified = self
            .diagnostics
            .measure("verification", || -> Result<VerifiedPrefix> {
                let manifest = RecordingManifest {
                    version: 3,
                    source_kind: "recording-directory-v3".into(),
                    snapshot: "snapshot.json".into(),
                    binding: self.snapshot.content.binding.clone(),
                    records: "records.jsonl".into(),
                    records_sha256: checkpoint.records_sha256.clone(),
                    record_count: self.count,
                };
                let mut source = RecordingDirectorySource::open_manifest(
                    fs::canonicalize(&self.stage)?,
                    manifest,
                    checkpoint.index_bytes,
                    false,
                )?;
                if let Some(prefix) = &state.verified {
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
                    source.chunk_position = prefix.position;
                }
                while source.validate_next()? {}
                self.diagnostics.verified(&source);
                Ok(VerifiedPrefix {
                    count: source.seen,
                    index_bytes: checkpoint.index_bytes,
                    digest: source.digest,
                    previous: source.previous,
                    position: source.chunk_position,
                })
            })?;
        let raw = serde_json::to_vec(&checkpoint)?;
        let previous = state.checkpoint_bytes;
        self.reserve_bytes(raw.len())?;
        let temporary = self.stage.join(".checkpoint.tmp");
        write_private_observed(&temporary, &raw, &self.diagnostics)?;
        self.diagnostics.measure("rename", || {
            fs::rename(&temporary, self.stage.join("checkpoint.json"))
        })?;
        self.diagnostics
            .measure("fsync", || fs::File::open(&self.stage)?.sync_all())?;
        self.used_bytes -= previous;
        self.chunked.as_mut().unwrap().checkpoint_bytes = raw.len() as u64;
        self.chunked.as_mut().unwrap().verified = Some(verified);
        let mut stats = self.diagnostics.0.lock().unwrap_or_else(|e| e.into_inner());
        stats.durable_pairs = self.count;
        stats.checkpoints += 1;
        Ok(())
    }
}
