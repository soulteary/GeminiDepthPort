//! Versioned recording boundary. Observed associations never become SDK framesets.
use super::*;
mod chunked;
use capture_contract::imu::{ImuProgress, ImuSample, ValidatedImuConfig};
use capture_contract::timing::StageStats;
use capture_contract::{DepthRepresentation, QuantizationPolicy};
use capture_observer::{DecodedCameraInfo, MetadataProvenance, ObservedAssociation};
use chunked::Chunked;
use quality_core::{ClockMapper, SourceClock, SourceStamp, map_source_time};
use std::{
    io::{BufRead, BufReader},
    sync::{Arc, Mutex},
    time::Instant,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRange {
    pub offset: u64,
    pub length: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<FileRange>,
    pub path: String,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredImage {
    pub file: FileRef,
    pub topic: Option<String>,
    pub record_timestamp_ns: Option<u64>,
    pub header_timestamp_ns: Option<u64>,
    pub frame_id: String,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub encoding: String,
    pub endian: StorageEndian,
    pub camera_info: Option<FileRef>,
    #[serde(default)]
    pub metadata: Option<FileRef>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum TaggedEvidence {
    SdkBoundPair {
        pair: BoundPairEvidence,
        #[serde(default)]
        source_json: Option<FileRef>,
    },
    ObservedAssociation {
        association: ObservedAssociation,
        header_mapping: ClockMapper,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingRecord {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imu: Vec<ImuSample>,
    pub sequence: u64,
    pub evidence: FileRef,
    pub color: StoredImage,
    pub depth: StoredImage,
    pub storage_representation: DepthRepresentation,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingManifest {
    pub version: u32,
    pub source_kind: String,
    pub snapshot: String,
    pub binding: SnapshotBinding,
    pub records: String,
    pub records_sha256: String,
    pub record_count: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotAssociation {
    pub version: u32,
    pub native_snapshot_id: capture_contract::SnapshotId,
    pub ros_snapshot_id: capture_contract::SnapshotId,
}
/// Reconstructed independently from each verified writer/reader snapshot.
/// Checkpoints retain sample progress, never a binding from an untrusted source.
struct RecordingImuBinding {
    config: ValidatedImuConfig,
    global: bool,
}
impl RecordingImuBinding {
    fn from_snapshot(snapshot: &SnapshotEnvelope) -> Result<Option<Self>> {
        let configuration = snapshot
            .content
            .configuration
            .get("capture")
            .unwrap_or(&snapshot.content.configuration);
        configuration
            .get("imu")
            .map(|value| {
                let config = ValidatedImuConfig::new(serde_json::from_value(value.clone())?)?;
                let global = if config.config().enabled {
                    let acquisition: capture_contract::AcquisitionPolicy =
                        serde_json::from_value(configuration["acquisition"].clone())?;
                    acquisition.resolve()?.message_timestamp
                        == capture_contract::MessageTimestamp::Global
                } else {
                    false
                };
                Ok(Self { config, global })
            })
            .transpose()
    }
}
fn validate_imu(
    samples: &[ImuSample],
    binding: Option<&RecordingImuBinding>,
    progress: &mut ImuProgress,
) -> Result<()> {
    require(samples.len() <= 128, "IMU_BATCH_LIMIT")?;
    if !samples.is_empty() {
        let binding = binding.ok_or(OfflineError::Invalid("IMU_CONFIG_REQUIRED"))?;
        for sample in samples {
            require(
                !binding.global || sample.global_timestamp_us.is_some(),
                "IMU_GLOBAL_TIMESTAMP_REQUIRED",
            )?;
            progress.accept_validated(sample, &binding.config)?;
        }
    }
    Ok(())
}
fn validate_snapshot_association(
    native: &SnapshotEnvelope,
    ros: &SnapshotEnvelope,
    link: &SnapshotAssociation,
) -> Result<()> {
    native.verify(&native.content.binding)?;
    ros.verify(&native.content.binding)?;
    let mut expected = ros.content.clone();
    expected.depth = native.content.depth.clone();
    require(
        link.version == 1
            && link.native_snapshot_id == native.snapshot_id
            && link.ros_snapshot_id == ros.snapshot_id
            && matches!(native.content.depth, DepthRepresentation::SdkCounts { .. })
            && matches!(ros.content.depth, DepthRepresentation::Millimetres { .. })
            && expected == native.content,
        "NATIVE_ROS_SNAPSHOT_BINDING",
    )
}
fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 15) as usize] as char);
    }
    result
}
#[derive(Clone)]
struct Progress {
    sequence: u64,
    color: u64,
    depth: u64,
    color_mono: u64,
    depth_mono: u64,
    kind: &'static str,
    color_sequence: Option<u64>,
    depth_sequence: Option<u64>,
}
const VALIDATION_BUFFER_BYTES: usize = 64 * 1024;
#[derive(Default)]
struct ValidationScratch {
    buffer: Vec<u8>,
    image_bytes: u64,
}
/// Hash exactly the declared extent using a fixed, reusable scratch buffer.
fn hash_extent(input: &mut impl Read, length: u64, buffer: &mut Vec<u8>) -> Result<String> {
    if buffer.is_empty() {
        buffer
            .try_reserve_exact(VALIDATION_BUFFER_BYTES)
            .map_err(|_| OfflineError::Invalid("ALLOCATION_FAILED"))?;
        buffer.resize(VALIDATION_BUFFER_BYTES, 0);
    }
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut remaining = length;
    while remaining > 0 {
        let n = remaining.min(buffer.len() as u64) as usize;
        input.read_exact(&mut buffer[..n])?;
        hash.update(&buffer[..n]);
        remaining -= n as u64;
    }
    Ok(lower_hex(hash.finish().as_ref()))
}
pub struct RecordingDirectorySource {
    root: PathBuf,
    manifest: RecordingManifest,
    snapshot: SnapshotEnvelope,
    reader: BufReader<std::io::Take<fs::File>>,
    line: Vec<u8>,
    validation: Mutex<ValidationScratch>,
    recovered: bool,
    recovered_chunks: Option<u64>,
    chunk_position: Option<(u64, u64)>,
    chunk_reader: Mutex<Option<chunked::CachedChunk>>,
    digest: ring::digest::Context,
    seen: u64,
    imu_binding: Option<RecordingImuBinding>,
    imu_progress: ImuProgress,
    imu_validated: u64,
    previous: Option<Progress>,
    finished: bool,
}
pub struct RecordedFrame {
    pub record: RecordingRecord,
    pub evidence: TaggedEvidence,
    pub evidence_json: Vec<u8>,
    pub depth: Vec<u8>,
}
impl RecordingDirectorySource {
    pub fn open(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        let manifest: RecordingManifest = parse(&read_bounded(
            &member(&root, "manifest.json")?,
            4 * 1024 * 1024,
        )?)?;
        require(
            matches!(
                (manifest.version, manifest.source_kind.as_str()),
                (2, "recording-directory-v2") | (3, "recording-directory-v3")
            ),
            "RECORDING_VERSION",
        )?;
        require(
            manifest.record_count > 0 && digest_valid(&manifest.records_sha256),
            "RECORDING_INDEX",
        )?;
        Self::open_manifest(root, manifest, u64::MAX, false)
    }
    fn open_manifest(
        root: PathBuf,
        manifest: RecordingManifest,
        index_limit: u64,
        recovered: bool,
    ) -> Result<Self> {
        let snapshot = SnapshotEnvelope::parse(
            &read_bounded(&member(&root, &manifest.snapshot)?, 4 * 1024 * 1024)?,
            &manifest.binding,
        )?;
        if root.join("snapshot-association.json").exists()
            || root.join("ros-snapshot.json").exists()
        {
            let link: SnapshotAssociation = parse(&read_bounded(
                &member(&root, "snapshot-association.json")?,
                65536,
            )?)?;
            let ros = SnapshotEnvelope::parse(
                &read_bounded(&member(&root, "ros-snapshot.json")?, 4 * 1024 * 1024)?,
                &manifest.binding,
            )?;
            validate_snapshot_association(&snapshot, &ros, &link)?;
        }
        let index_path = member(&root, &manifest.records)?;
        require(fs::metadata(&index_path)?.is_file(), "SOURCE_NOT_FILE")?;
        let file = fs::File::open(index_path)?;
        require(file.metadata()?.is_file(), "SOURCE_NOT_FILE")?;
        let imu_binding = RecordingImuBinding::from_snapshot(&snapshot)?;
        Ok(Self {
            root,
            manifest,
            snapshot,
            reader: BufReader::new(file.take(index_limit)),
            line: Vec::new(),
            validation: Mutex::default(),
            recovered,
            recovered_chunks: None,
            chunk_position: None,
            chunk_reader: Mutex::new(None),
            digest: ring::digest::Context::new(&ring::digest::SHA256),
            seen: 0,
            imu_binding,
            imu_progress: ImuProgress::default(),
            imu_validated: 0,
            previous: None,
            finished: false,
        })
    }
    pub fn snapshot(&self) -> &SnapshotEnvelope {
        &self.snapshot
    }
    fn has_complete_marker(&self) -> Result<bool> {
        if !self.root.join("COMPLETE").try_exists()? {
            return Ok(false);
        }
        let bytes = read_bounded(&member(&self.root, "COMPLETE")?, 256)?;
        require(
            bytes == b"recording-directory-v3; no business validity decision\n",
            "RECORDING_COMPLETE_MARKER",
        )?;
        Ok(true)
    }

    fn file(&self, file: &FileRef, max: usize) -> Result<Vec<u8>> {
        require(digest_valid(&file.sha256), "RECORDING_DIGEST_FORMAT")?;
        let bytes = if let Some(range) = &file.range {
            require(self.manifest.version == 3, "RECORDING_RANGE_VERSION")?;
            chunked::read_range(&self.root, file, range, max, &self.chunk_reader)?
        } else {
            read_bounded(&member(&self.root, &file.path)?, max)?
        };
        require(sha256(&bytes) == file.sha256, "RECORDING_DIGEST")?;
        Ok(bytes)
    }
    fn verify_image_file(
        &self,
        file: &FileRef,
        max: usize,
        scratch: &mut ValidationScratch,
    ) -> Result<u64> {
        require(digest_valid(&file.sha256), "RECORDING_DIGEST_FORMAT")?;
        let (length, hash) = if let Some(range) = &file.range {
            require(self.manifest.version == 3, "RECORDING_RANGE_VERSION")?;
            let hash = chunked::with_range(
                &self.root,
                file,
                range,
                max,
                &self.chunk_reader,
                |input, length| hash_extent(input, length, &mut scratch.buffer),
            )?;
            (range.length, hash)
        } else {
            let path = member(&self.root, &file.path)?;
            require(fs::metadata(&path)?.is_file(), "SOURCE_NOT_FILE")?;
            let mut input = fs::File::open(path)?;
            let metadata = input.metadata()?;
            require(metadata.is_file(), "SOURCE_NOT_FILE")?;
            require(metadata.len() <= max as u64, "SOURCE_SIZE_LIMIT")?;
            let length = metadata.len();
            let hash = hash_extent(&mut input, length, &mut scratch.buffer)?;
            let mut extra = [0u8; 1];
            require(input.read(&mut extra)? == 0, "SOURCE_CHANGED_DURING_READ")?;
            (length, hash)
        };
        require(hash == file.sha256, "RECORDING_DIGEST")?;
        scratch.image_bytes = scratch.image_bytes.saturating_add(length);
        Ok(length)
    }
    fn image(
        &self,
        image: &StoredImage,
        stream: &str,
        observed: bool,
        materialize: bool,
    ) -> Result<Vec<u8>> {
        let model = self
            .snapshot
            .content
            .calibration
            .models
            .get(stream)
            .ok_or(OfflineError::Invalid("CALIBRATION_STREAM_MISSING"))?;
        require(
            image.width == model.width
                && image.height == model.height
                && image.frame_id == model.frame_id,
            "RECORDING_IMAGE_BINDING",
        )?;
        let bpp = match (stream, image.encoding.as_str()) {
            ("depth", "depth16" | "16UC1") => 2usize,
            ("color", "yuyv") => 2,
            ("color", "rgb8" | "bgr8") => 3,
            _ => return Err(OfflineError::Invalid("RECORDING_ENCODING")),
        };
        require(
            image.width > 0
                && image.height > 0
                && image.stride >= image.width as usize * bpp
                && (image.encoding != "yuyv" || image.width % 2 == 0),
            "RECORDING_LAYOUT",
        )?;
        let length = image
            .stride
            .checked_mul(image.height as usize)
            .ok_or(OfflineError::Invalid("RECORDING_LAYOUT"))?;
        require(length <= 64 * 1024 * 1024, "RECORDING_PAYLOAD_LIMIT")?;
        let bytes = if materialize {
            let bytes = self.file(&image.file, 64 * 1024 * 1024)?;
            require(bytes.len() == length, "RECORDING_PAYLOAD_LENGTH")?;
            bytes
        } else {
            let mut scratch = self.validation.lock().unwrap_or_else(|e| e.into_inner());
            let read = self.verify_image_file(&image.file, 64 * 1024 * 1024, &mut scratch)?;
            require(read == length as u64, "RECORDING_PAYLOAD_LENGTH")?;
            Vec::new()
        };
        if observed {
            require(
                image
                    .topic
                    .as_ref()
                    .is_some_and(|s| s.starts_with('/') && s.len() <= 1024)
                    && image.record_timestamp_ns.is_some_and(|v| v > 0),
                "OBSERVED_WIRE_CONTEXT",
            )?;
            let info: DecodedCameraInfo = parse(
                &self.file(
                    image
                        .camera_info
                        .as_ref()
                        .ok_or(OfflineError::Invalid("OBSERVED_INFO_REQUIRED"))?,
                    65536,
                )?,
            )?;
            let distortion = match model.distortion_model {
                capture_contract::DistortionModel::None => "",
                capture_contract::DistortionModel::PlumbBob => "plumb_bob",
                capture_contract::DistortionModel::RationalPolynomial => "rational_polynomial",
                capture_contract::DistortionModel::Equidistant => "equidistant",
            };
            require(
                info.frame_id == image.frame_id
                    && info.width == image.width
                    && info.height == image.height
                    && info.k == model.k
                    && info.d == model.distortion_coefficients
                    && (info.distortion_model == distortion
                        || distortion.is_empty() && info.distortion_model == "none")
                    && info.r == [1., 0., 0., 0., 1., 0., 0., 0., 1.]
                    && info.binning.iter().all(|v| *v <= 1)
                    && info.p.iter().all(|v| v.is_finite())
                    && info.roi == [0; 4]
                    && !info.roi_do_rectify
                    && Some(
                        info.stamp
                            .to_ns()
                            .map_err(|_| OfflineError::Invalid("OBSERVED_INFO_STAMP"))?,
                    ) == image.header_timestamp_ns,
                "OBSERVED_INFO_BINDING",
            )?;
        }
        Ok(bytes)
    }
    pub fn next_frame(&mut self) -> Result<Option<RecordedFrame>> {
        self.read_next(true)
    }
    /// Validate one record with bounded image scratch storage, without returning
    /// image buffers. EOF still verifies the complete index and chunk lengths.
    pub fn validate_next(&mut self) -> Result<bool> {
        let before = self.seen;
        self.read_next(false)?;
        Ok(self.seen > before)
    }
    fn read_next(&mut self, materialize: bool) -> Result<Option<RecordedFrame>> {
        if self.finished {
            return Ok(None);
        }
        self.line.clear();
        let n = self
            .reader
            .by_ref()
            .take(65537)
            .read_until(b'\n', &mut self.line)?;
        require(n <= 65536, "RECORDING_INDEX_LINE_LIMIT")?;
        if n == 0 {
            require(
                self.imu_binding
                    .as_ref()
                    .is_none_or(|b| !b.config.config().enabled)
                    || self.imu_progress.has_both(),
                "IMU_STREAMS_MISSING",
            )?;
            self.finish_chunk()?;
            if let Some(chunks) = self.recovered_chunks {
                require(
                    self.chunk_position.map(|c| c.0) == Some(chunks),
                    "RECOVERY_CHUNK_COUNT",
                )?;
            }
            let actual = lower_hex(self.digest.clone().finish().as_ref());
            require(
                self.seen == self.manifest.record_count && actual == self.manifest.records_sha256,
                "RECORDING_INDEX_INCOMPLETE",
            )?;
            self.finished = true;
            return Ok(None);
        }
        self.digest.update(&self.line);
        require(
            self.seen < self.manifest.record_count,
            "RECORDING_INDEX_COUNT",
        )?;
        let record: RecordingRecord = parse(&self.line)?;
        validate_imu(
            &record.imu,
            self.imu_binding.as_ref(),
            &mut self.imu_progress,
        )?;
        self.imu_validated += record.imu.len() as u64;
        self.check_ranges(&record)?;
        record.storage_representation.validate()?;
        let evidence_json = self.file(&record.evidence, 4 * 1024 * 1024)?;
        let evidence: TaggedEvidence = parse(&evidence_json)?;
        if self.manifest.version == 3 {
            require(
                matches!(
                    &evidence,
                    TaggedEvidence::SdkBoundPair {
                        source_json: None,
                        ..
                    }
                ) && record.color.camera_info.is_none()
                    && record.depth.camera_info.is_none()
                    && record.color.metadata.is_none()
                    && record.depth.metadata.is_none(),
                "RECORDING_V3_NATIVE_ONLY",
            )?;
        }
        let (color, depth, color_time, depth_time, kind) = match &evidence {
            TaggedEvidence::SdkBoundPair { pair, source_json } => {
                if let Some(file) = source_json {
                    let original: BoundPairEvidence = parse(&self.file(file, 4 * 1024 * 1024)?)?;
                    require(
                        serde_json::to_value(&original)? == serde_json::to_value(pair)?,
                        "SDK_ORIGINAL_JSON_BINDING",
                    )?;
                }
                require(
                    pair.delta_us <= self.snapshot.content.quality_policy.max_pair_delta_us,
                    "PAIR_SKEW",
                )?;
                pair.validate()
                    .map_err(|_| OfflineError::Invalid("PAIR_EVIDENCE_INVALID"))?;
                require(
                    pair.pair_sequence == record.sequence,
                    "PAIR_SEQUENCE_BINDING",
                )?;
                for (stored, e) in [(&record.color, &pair.color), (&record.depth, &pair.depth)] {
                    require(
                        stored.width == e.width
                            && stored.height == e.height
                            && stored.stride == e.stride
                            && stored.encoding == e.encoding,
                        "SDK_STORED_LAYOUT_BINDING",
                    )?;
                    if stored.topic.is_some()
                        || stored.header_timestamp_ns.is_some()
                        || stored.record_timestamp_ns.is_some()
                    {
                        require(
                            stored
                                .topic
                                .as_ref()
                                .is_some_and(|v| v.starts_with('/') && v.len() <= 1024)
                                && stored.record_timestamp_ns.is_some_and(|v| v > 0)
                                && stored.header_timestamp_ns
                                    == e.context
                                        .system_timestamp_us
                                        .and_then(|v| v.checked_mul(1000)),
                            "SDK_WIRE_HEADER_BINDING",
                        )?;
                    }
                }
                let scale = representation_scale(&record.storage_representation);
                require(
                    pair.depth.output_depth_scale_mm.map(f64::from) == Some(scale),
                    "SDK_STORED_SCALE_BINDING",
                )?;
                if record.storage_representation != self.snapshot.content.depth {
                    require(
                        matches!(
                            record.storage_representation,
                            DepthRepresentation::SdkCounts { .. }
                        ) && !pair.depth.normalized
                            && pair.depth.source_depth_scale_mm == pair.depth.output_depth_scale_mm
                            && self
                                .snapshot
                                .content
                                .configuration
                                .get("source_depth_scale_mm")
                                .and_then(|v| v.as_f64())
                                == Some(scale),
                        "SDK_RAW_SOURCE_BINDING",
                    )?;
                }
                (
                    pair.color.context.clone(),
                    pair.depth.context.clone(),
                    pair.color_stamp_us,
                    pair.depth_stamp_us,
                    "sdk-bound-pair",
                )
            }
            TaggedEvidence::ObservedAssociation {
                association: a,
                header_mapping,
            } => {
                require(
                    a.version == 1
                        && a.association_sequence == record.sequence
                        && a.same_sdk_frameset.is_none()
                        && !a.hardware_sync_verified
                        && a.delta_ns == a.color.mapped_unix_ns.abs_diff(a.depth.mapped_unix_ns)
                        && a.delta_ns
                            <= self
                                .snapshot
                                .content
                                .quality_policy
                                .max_pair_delta_us
                                .checked_mul(1000)
                                .ok_or(OfflineError::Invalid("PAIR_SKEW"))?,
                    "OBSERVED_ASSOCIATION_INVALID",
                )?;
                require(matches!(record.storage_representation,DepthRepresentation::Millimetres{quantization:QuantizationPolicy::NearestHalfUpPreserveZeroClampPositiveRejectOverflow}) && record.storage_representation==self.snapshot.content.depth,"OBSERVED_COUNTS_UNRECOVERABLE")?;
                for (stored, frame) in [(&record.color, &a.color), (&record.depth, &a.depth)] {
                    require(
                        frame.observation_sequence > 0
                            && stored.header_timestamp_ns == Some(frame.observed_header_ns)
                            && frame.observed_header_ns > 0
                            && frame.mapped_unix_ns > 0,
                        "OBSERVED_HEADER_BINDING",
                    )?;
                    require(
                        matches!(
                            header_mapping,
                            ClockMapper::DeclaredObservedUnixHeader
                                | ClockMapper::MeasuredObservedHeaderOffset { .. }
                        ) && map_source_time(
                            SourceStamp {
                                clock: SourceClock::ObservedHeaderNanoseconds,
                                value: frame.observed_header_ns,
                            },
                            *header_mapping,
                        )
                        .ok()
                            == Some(frame.mapped_unix_ns),
                        "OBSERVED_TIME_MAPPING",
                    )?;
                    if frame.metadata_provenance == MetadataProvenance::Unobserved {
                        require(
                            frame.context.sequence.is_none()
                                && frame.context.device_timestamp_us.is_none()
                                && frame.context.system_timestamp_us.is_none()
                                && frame.reported_source_session.is_none(),
                            "OBSERVED_UNKNOWN_SDK_BINDING",
                        )?;
                    } else {
                        let raw = self.file(
                            stored
                                .metadata
                                .as_ref()
                                .ok_or(OfflineError::Invalid("OBSERVED_METADATA_REQUIRED"))?,
                            4096,
                        )?;
                        let value = capture_contract::parse_strict(&raw)?;
                        require(
                            value["version"] == 1
                                && value["frame_id"].as_str() == Some(stored.frame_id.as_str())
                                && value["stamp_sec"]
                                    .as_u64()
                                    .and_then(|v| v.checked_mul(1_000_000_000))
                                    .and_then(|v| {
                                        value["stamp_nanosec"]
                                            .as_u64()
                                            .and_then(|n| v.checked_add(n))
                                    })
                                    == Some(frame.observed_header_ns)
                                && value.get("sequence").and_then(|v| v.as_u64())
                                    == frame.context.sequence
                                && value.get("device_timestamp_us").and_then(|v| v.as_u64())
                                    == frame.context.device_timestamp_us
                                && value.get("system_timestamp_us").and_then(|v| v.as_u64())
                                    == frame.context.system_timestamp_us
                                && value.get("session_id").and_then(|v| v.as_str())
                                    == frame.reported_source_session.as_ref().map(|v| v.as_str()),
                            "OBSERVED_METADATA_BINDING",
                        )?;
                    }
                }
                (
                    a.color.context.clone(),
                    a.depth.context.clone(),
                    a.color.mapped_unix_ns,
                    a.depth.mapped_unix_ns,
                    "observed-association",
                )
            }
        };
        require(
            context_binding(&color, &self.snapshot)
                && context_binding(&depth, &self.snapshot)
                && color.stream_id == "color"
                && depth.stream_id == "depth",
            "PAIR_SNAPSHOT_BINDING",
        )?;
        color.validate()?;
        depth.validate()?;
        require(record.sequence > 0, "PAIR_SEQUENCE_BINDING")?;
        if let Some(last) = &self.previous {
            require(
                record.sequence > last.sequence
                    && kind == last.kind
                    && (kind != "sdk-bound-pair"
                        || color.sequence > last.color_sequence
                            && depth.sequence > last.depth_sequence)
                    && color_time > last.color
                    && depth_time > last.depth
                    && color.received_mono_offset_ns >= last.color_mono
                    && depth.received_mono_offset_ns >= last.depth_mono,
                "SOURCE_PROGRESSION",
            )?;
        }
        let observed = matches!(evidence, TaggedEvidence::ObservedAssociation { .. });
        let _color = self.image(&record.color, "color", observed, materialize)?;
        let depth_bytes = self.image(&record.depth, "depth", observed, materialize)?;
        self.previous = Some(Progress {
            sequence: record.sequence,
            color: color_time,
            depth: depth_time,
            color_mono: color.received_mono_offset_ns,
            depth_mono: depth.received_mono_offset_ns,
            kind,
            color_sequence: color.sequence,
            depth_sequence: depth.sequence,
        });
        self.seen += 1;
        if materialize {
            Ok(Some(RecordedFrame {
                record,
                evidence,
                evidence_json,
                depth: depth_bytes,
            }))
        } else {
            Ok(None)
        }
    }
}
/// Blocking, one-record-at-a-time conversion. EOF verifies the complete index
/// digest before Job::finish; any failure leaves only the existing partial job.
pub fn process(
    source: &mut RecordingDirectorySource,
    output: Option<(&Path, &str, depth_output::Mode)>,
    operation: Operation,
) -> Result<RunReport> {
    if output.is_some() {
        require(
            !source.recovered
                && (source.manifest.version == 2 || source.has_complete_marker()?)
                && !source
                    .root
                    .file_name()
                    .is_some_and(|v| v.to_string_lossy().ends_with(".partial"))
                && !source.root.join("INTERRUPTED.json").exists()
                && !source.root.join("FAILED.json").exists(),
            "INCOMPLETE_RECORDING_INPUT",
        )?;
    }
    let snapshot = source.snapshot.clone();
    let mut g = if matches!(operation, Operation::DepthToColor) {
        Some(geometry(&snapshot)?)
    } else {
        None
    };
    let mut job = output
        .map(|(p, n, m)| depth_output::Job::create(p, n, m, snapshot.snapshot_id.clone()))
        .transpose()?;
    if let Some(job) = job.as_mut() {
        job_control::track(job.stage_path());
        job.attach_json("build-info.json", &serde_json::to_vec(&build_info())?)?;
        job.attach_json("snapshot.json", &snapshot.to_bytes()?)?;
    }
    if let Some(job) = job.as_mut() {
        if source.root.join("snapshot-association.json").is_file() {
            let link = read_bounded(&member(&source.root, "snapshot-association.json")?, 65536)?;
            let ros = read_bounded(&member(&source.root, "ros-snapshot.json")?, 4 * 1024 * 1024)?;
            let link_digest = sha256(&link);
            let ros_digest = sha256(&ros);
            job.attach_source_blob(&link_digest, &link)?;
            job.attach_source_blob(&ros_digest, &ros)?;
            job.attach_json("recording-context.json",&serde_json::to_vec(&serde_json::json!({"version":1,"snapshot_association_sha256":link_digest,"ros_snapshot_sha256":ros_digest}))?)?;
        }
    }

    let mut report = RunReport {
        version: 2,
        complete: false,
        validated_pairs: 0,
        written_frames: 0,
        d2c: matches!(operation, Operation::DepthToColor),
        normalized_frames: 0,
    };
    while let Some(frame) = source.next_frame()? {
        let context = match &frame.evidence {
            TaggedEvidence::SdkBoundPair { pair, .. } => &pair.depth.context,
            TaggedEvidence::ObservedAssociation { association, .. } => &association.depth.context,
        };
        job_control::checkpoint()?;
        let (encoded, normalized, stats) = convert_material(
            &frame.depth,
            frame.record.depth.endian,
            context,
            frame.record.depth.width,
            frame.record.depth.height,
            frame.record.depth.stride,
            frame.record.sequence,
            &frame.record.evidence.sha256,
            &frame.record.storage_representation,
            operation,
            &mut g,
            report.validated_pairs,
        )?;
        if let Some(job) = job.as_mut() {
            let mut files = vec![
                &frame.record.evidence,
                &frame.record.color.file,
                &frame.record.depth.file,
            ];
            for image in [&frame.record.color, &frame.record.depth] {
                if let Some(file) = &image.camera_info {
                    files.push(file);
                }
                if let Some(file) = &image.metadata {
                    files.push(file);
                }
            }
            if let TaggedEvidence::SdkBoundPair {
                source_json: Some(file),
                ..
            } = &frame.evidence
            {
                files.push(file);
            }
            for file in files {
                let bytes = source.file(file, 64 * 1024 * 1024)?;
                job.attach_source_blob(&file.sha256, &bytes)?;
            }
            job.attach_json(
                &format!("recording-record-{:020}.json", frame.record.sequence),
                &serde_json::to_vec(&frame.record)?,
            )?;
        }
        if let Some(job) = job.as_mut() {
            job.attach_json(
                &format!("pair-evidence-{:020}.json", frame.record.sequence),
                &frame.evidence_json,
            )?;
            if let Some(stats) = &stats {
                job.attach_json(&format!("d2c-diagnostics-{:020}.json",report.validated_pairs),
                &serde_json::to_vec(&serde_json::json!({"version":1,"algorithm_version":d2c_core::RAY_ALGORITHM_VERSION,
                    "pair_sequence":frame.record.sequence,"source_context":context,"counters":stats}))?)?;
            }
            job.append(&encoded)?;
        }
        report.validated_pairs += 1;
        report.normalized_frames += u64::from(normalized);
        job_control::advance(&report, stats.as_ref())?;
    }
    require(report.validated_pairs > 0, "SOURCE_EMPTY")?;
    if let Some(job) = job {
        job_control::checkpoint()?;
        job.finish()?;
        report.written_frames = report.validated_pairs;
    }
    report.complete =
        !source.recovered && (source.manifest.version == 2 || source.has_complete_marker()?);
    Ok(report)
}

/// Runtime-only IO diagnostics, excluded from recording files and content IDs.
#[derive(Clone, Default)]
pub struct RawRecordingDiagnostics(Arc<Mutex<RawIoStats>>);
#[derive(Default)]
struct RawIoStats {
    write: StageStats,
    fsync: StageStats,
    rename: StageStats,
    verification: StageStats,
    append: StageStats,
    finish: StageStats,
    uncommitted_pairs_peak: u64,
    uncommitted_payload_bytes_peak: u64,
    validation_imu_samples: u64,
    payload_write_calls: u64,
    index_write_calls: u64,
    index_flushes: u64,
    validation_image_bytes: u64,
    validation_buffer_peak_bytes: usize,
    durable_pairs: u64,
    checkpoints: u64,
    payload_files: u64,
    failures: std::collections::BTreeMap<&'static str, u64>,
}
impl RawRecordingDiagnostics {
    fn measure<T, E>(
        &self,
        stage: &'static str,
        run: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        let start = Instant::now();
        let result = run();
        let elapsed = start.elapsed();
        let mut stats = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match stage {
            "write" => &mut stats.write,
            "fsync" => &mut stats.fsync,
            "rename" => &mut stats.rename,
            "verification" => &mut stats.verification,
            "append" => &mut stats.append,
            _ => &mut stats.finish,
        }
        .observe(elapsed);
        if result.is_err() {
            *stats.failures.entry(stage).or_default() += 1;
        }
        result
    }
    fn verified(&self, source: &RecordingDirectorySource) {
        let scratch = source.validation.lock().unwrap_or_else(|e| e.into_inner());
        let mut stats = self.0.lock().unwrap_or_else(|e| e.into_inner());
        stats.validation_imu_samples += source.imu_validated;
        stats.validation_image_bytes = stats
            .validation_image_bytes
            .saturating_add(scratch.image_bytes);
        stats.validation_buffer_peak_bytes = stats
            .validation_buffer_peak_bytes
            .max(scratch.buffer.capacity());
    }
    pub fn report(&self) -> serde_json::Value {
        let stats = self.0.lock().unwrap_or_else(|e| e.into_inner());
        serde_json::json!({"clock":"monotonic-duration","scope":"native-writer-only; write-success-is-not-durability",
            "write":stats.write.report(),"fsync":stats.fsync.report(),"rename":stats.rename.report(),
            "verification":stats.verification.report(),"append":stats.append.report(),"finish_total":stats.finish.report(),
            "failed_calls":stats.failures,"durable_pairs":stats.durable_pairs,"checkpoints":stats.checkpoints,"payload_files":stats.payload_files,"validation_image_bytes":stats.validation_image_bytes,
            "validation_buffer_peak_bytes":stats.validation_buffer_peak_bytes,
            "uncommitted_pairs_peak":stats.uncommitted_pairs_peak,"uncommitted_payload_bytes_peak":stats.uncommitted_payload_bytes_peak,
            "validation_imu_samples":stats.validation_imu_samples,
            "payload_write_calls":stats.payload_write_calls,"index_write_calls":stats.index_write_calls,
            "index_flushes":stats.index_flushes})
    }
}

/// Optional native SDK sink. No SDK handles cross this API; caller owns both
/// copied pixel buffers. Bounded acquisition queues remain the caller's policy.
pub struct RawRecordingWriter {
    chunked: Option<Chunked>,
    stage: PathBuf,
    final_path: PathBuf,
    snapshot: SnapshotEnvelope,
    records: fs::File,
    imu_binding: Option<RecordingImuBinding>,
    imu_progress: ImuProgress,
    evidence_buffer: Vec<u8>,
    record_buffer: Vec<u8>,
    digest: ring::digest::Context,
    count: u64,
    failed: bool,
    completed: bool,
    max_bytes: u64,
    used_bytes: u64,
    diagnostics: RawRecordingDiagnostics,
}
impl RawRecordingWriter {
    pub fn begin(parent: &Path, name: &str, snapshot: SnapshotEnvelope) -> Result<Self> {
        Self::begin_with_limit(parent, name, snapshot, u64::MAX)
    }
    pub fn begin_with_limit(
        parent: &Path,
        name: &str,
        snapshot: SnapshotEnvelope,
        max_bytes: u64,
    ) -> Result<Self> {
        Self::begin_with_diagnostics(
            parent,
            name,
            snapshot,
            max_bytes,
            RawRecordingDiagnostics::default(),
        )
    }
    pub fn begin_with_diagnostics(
        parent: &Path,
        name: &str,
        snapshot: SnapshotEnvelope,
        max_bytes: u64,
        diagnostics: RawRecordingDiagnostics,
    ) -> Result<Self> {
        snapshot.verify(&snapshot.content.binding)?;
        let imu_binding = RecordingImuBinding::from_snapshot(&snapshot)?;
        require(
            !name.is_empty()
                && name.len() <= 128
                && name
                    .bytes()
                    .all(|v| v.is_ascii_alphanumeric() || v == b'-' || v == b'_'),
            "RECORDING_NAME",
        )?;
        let final_path = parent.join(name);
        require(!final_path.exists(), "OUTPUT_EXISTS")?;
        let stage = parent.join(format!("{name}.partial"));
        fs::create_dir(&stage)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&stage, fs::Permissions::from_mode(0o700))?;
        }
        let snapshot_bytes = snapshot.to_bytes()?;
        require(
            max_bytes > 0 && snapshot_bytes.len() as u64 <= max_bytes,
            "RECORDING_BYTE_LIMIT",
        )?;
        write_private_observed(&stage.join("snapshot.json"), &snapshot_bytes, &diagnostics)?;
        let records = new_private(&stage.join("records.jsonl"))?;
        Ok(Self {
            chunked: None,
            stage,
            final_path,
            snapshot,
            records,
            imu_binding,
            imu_progress: ImuProgress::default(),
            evidence_buffer: Vec::new(),
            record_buffer: Vec::new(),
            digest: ring::digest::Context::new(&ring::digest::SHA256),
            count: 0,
            failed: false,
            completed: false,
            max_bytes,
            used_bytes: snapshot_bytes.len() as u64,
            diagnostics,
        })
    }
    pub fn diagnostics(&self) -> RawRecordingDiagnostics {
        self.diagnostics.clone()
    }
    /// Persist the original ROS/mm snapshot link before any native pair. It is
    /// public provenance, not a claim that mm can recover original SDK counts.
    pub fn attach_ros_snapshot(&mut self, ros: &SnapshotEnvelope) -> Result<()> {
        require(!self.failed, "RECORDING_ALREADY_FAILED")?;
        let result = (|| -> Result<()> {
            require(self.count == 0, "SNAPSHOT_LINK_AFTER_PIXELS")?;
            let link = SnapshotAssociation {
                version: 1,
                native_snapshot_id: self.snapshot.snapshot_id.clone(),
                ros_snapshot_id: ros.snapshot_id.clone(),
            };
            validate_snapshot_association(&self.snapshot, ros, &link)?;
            self.write_budgeted("ros-snapshot.json", &ros.to_bytes()?)?;
            self.write_budgeted("snapshot-association.json", &serde_json::to_vec(&link)?)?;
            Ok(())
        })();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    pub fn append(
        &mut self,
        pair: &BoundPairEvidence,
        color: &[u8],
        depth: &[u8],
        endian: StorageEndian,
    ) -> Result<()> {
        self.append_with_imu(pair, color, depth, endian, &[])
    }
    pub fn append_with_imu(
        &mut self,
        pair: &BoundPairEvidence,
        color: &[u8],
        depth: &[u8],
        endian: StorageEndian,
        imu: &[ImuSample],
    ) -> Result<()> {
        require(!self.failed, "RECORDING_ALREADY_FAILED")?;
        let diagnostics = self.diagnostics.clone();
        let result = diagnostics.measure("append", || {
            self.append_inner(pair, color, depth, endian, imu)
        });
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn append_inner(
        &mut self,
        pair: &BoundPairEvidence,
        color: &[u8],
        depth: &[u8],
        endian: StorageEndian,
        imu: &[ImuSample],
    ) -> Result<()> {
        validate_imu(imu, self.imu_binding.as_ref(), &mut self.imu_progress)?;
        pair.validate()
            .map_err(|_| OfflineError::Invalid("PAIR_EVIDENCE_INVALID"))?;
        require(
            !pair.depth.normalized
                && pair.depth.source_depth_scale_mm == pair.depth.output_depth_scale_mm,
            "NATIVE_COUNTS_REQUIRED",
        )?;
        require(
            context_binding(&pair.color.context, &self.snapshot)
                && context_binding(&pair.depth.context, &self.snapshot),
            "PAIR_SNAPSHOT_BINDING",
        )?;
        let sequence = pair.pair_sequence;
        let evidence = TaggedEvidence::SdkBoundPair {
            pair: pair.clone(),
            source_json: None,
        };
        let mut raw_evidence = std::mem::take(&mut self.evidence_buffer);
        raw_evidence.clear();
        serde_json::to_writer(&mut raw_evidence, &evidence)?;
        for (frame, pixels) in [(&pair.color, color), (&pair.depth, depth)] {
            let length = frame
                .stride
                .checked_mul(frame.height as usize)
                .ok_or(OfflineError::Invalid("RECORDING_LAYOUT"))?;
            require(
                length <= 64 * 1024 * 1024 && pixels.len() == length,
                "RECORDING_PAYLOAD_LENGTH",
            )?;
        }
        self.prepare_chunk(color.len(), depth.len(), raw_evidence.len())?;
        let [color_file, depth_file, evidence] =
            self.store_pair(sequence, [color, depth, &raw_evidence])?;
        let store =
            |role: &str, frame: &frame_core::FrameEvidence, file: FileRef| -> Result<StoredImage> {
                let model = self
                    .snapshot
                    .content
                    .calibration
                    .models
                    .get(role)
                    .ok_or(OfflineError::Invalid("CALIBRATION_STREAM_MISSING"))?;
                Ok(StoredImage {
                    file,
                    topic: None,
                    record_timestamp_ns: None,
                    header_timestamp_ns: None,
                    frame_id: model.frame_id.clone(),
                    width: frame.width,
                    height: frame.height,
                    stride: frame.stride,
                    encoding: frame.encoding.clone(),
                    endian,
                    camera_info: None,
                    metadata: None,
                })
            };
        let color = store("color", &pair.color, color_file)?;
        let depth = store("depth", &pair.depth, depth_file)?;
        let record = RecordingRecord {
            imu: imu.to_vec(),
            sequence,
            evidence,
            color,
            depth,
            storage_representation: DepthRepresentation::SdkCounts {
                scale_mm: f64::from(
                    pair.depth
                        .source_depth_scale_mm
                        .ok_or(OfflineError::Invalid("DEPTH_SCALE"))?,
                ),
            },
        };
        let mut raw = std::mem::take(&mut self.record_buffer);
        raw.clear();
        serde_json::to_writer(&mut raw, &record)?;
        raw.push(b'\n');
        require(raw.len() <= 65536, "RECORDING_INDEX_LINE_LIMIT")?;
        self.reserve_bytes(raw.len())?;
        self.append_index(&raw)?;
        self.digest.update(&raw);
        let index_bytes = raw.len() as u64;
        self.record_buffer = raw;
        self.evidence_buffer = raw_evidence;
        self.count += 1;
        let (pair_limit, byte_limit) = self.chunk_limits();
        if let Some(chunked) = &mut self.chunked {
            chunked.index_bytes += index_bytes;
            let active = chunked.active.as_mut().unwrap();
            active.pairs += 1;
            let pending = chunked.pending_credits();
            let mut stats = self.diagnostics.0.lock().unwrap_or_else(|e| e.into_inner());
            stats.uncommitted_pairs_peak = stats
                .uncommitted_pairs_peak
                .max(pending.0 + chunked.active.as_ref().unwrap().pairs);
            stats.uncommitted_payload_bytes_peak = stats
                .uncommitted_payload_bytes_peak
                .max(pending.1 + chunked.active.as_ref().unwrap().bytes);
            drop(stats);
            let active = chunked.active.as_ref().unwrap();
            if active.pairs >= pair_limit || active.bytes >= byte_limit {
                self.automatic_checkpoint()?;
            }
        }
        Ok(())
    }
    fn append_index(&mut self, raw: &[u8]) -> Result<()> {
        if self.chunked.is_none() {
            use std::io::Write;
            self.diagnostics
                .measure("write", || self.records.write_all(raw))?;
            return Ok(());
        }
        if self.chunked.as_ref().unwrap().index_buffer.len() + raw.len()
            > chunked::INDEX_BUFFER_BYTES
        {
            self.flush_index()?;
        }
        let buffer = &mut self.chunked.as_mut().unwrap().index_buffer;
        if buffer.capacity() == 0 {
            buffer
                .try_reserve_exact(chunked::INDEX_BUFFER_BYTES)
                .map_err(|_| OfflineError::Invalid("ALLOCATION_FAILED"))?;
        }
        buffer.extend_from_slice(raw);
        Ok(())
    }
    fn flush_index(&mut self) -> Result<()> {
        let Some(state) = &mut self.chunked else {
            return Ok(());
        };
        if state.index_buffer.is_empty() {
            return Ok(());
        }
        let mut calls = 0;
        let result = self.diagnostics.measure("write", || {
            chunked::write_all_counted(&mut self.records, &state.index_buffer, &mut calls)
        });
        let mut stats = self.diagnostics.0.lock().unwrap_or_else(|e| e.into_inner());
        stats.index_write_calls = stats.index_write_calls.saturating_add(calls);
        result?;
        stats.index_flushes = stats.index_flushes.saturating_add(1);
        state.index_buffer.clear();
        Ok(())
    }
    fn reserve_bytes(&mut self, len: usize) -> Result<()> {
        let total = self
            .used_bytes
            .checked_add(len as u64)
            .ok_or(OfflineError::Invalid("RECORDING_BYTE_LIMIT"))?;
        require(total <= self.max_bytes, "RECORDING_BYTE_LIMIT")?;
        // Reserve before IO: short writes must not permit later writes past budget.
        self.used_bytes = total;
        Ok(())
    }
    fn write_budgeted(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        self.reserve_bytes(bytes.len())?;
        write_private_observed(&self.stage.join(name), bytes, &self.diagnostics)?;
        Ok(())
    }
    pub fn finish(self) -> Result<PathBuf> {
        self.finish_guarded(|| Ok(()))
    }
    /// Acquire an application RAII commit guard only after full disk validation.
    /// The guard stays held through no-replace publication, COMPLETE and fsync.
    pub fn finish_guarded<G>(mut self, acquire: impl FnOnce() -> Result<G>) -> Result<PathBuf> {
        let diagnostics = self.diagnostics.clone();
        let result = diagnostics.measure("finish", || self.finish_inner(acquire));
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn finish_inner<G>(&mut self, acquire: impl FnOnce() -> Result<G>) -> Result<PathBuf> {
        require(!self.failed && self.count > 0, "RECORDING_INCOMPLETE")?;
        if self.chunked.is_some() {
            // The checkpoint already synced this exact index prefix.
            self.checkpoint()?;
        } else {
            self.diagnostics
                .measure("fsync", || self.records.sync_all())?;
        }
        let version = if self.chunked.is_some() { 3 } else { 2 };
        let records_sha256 = lower_hex(self.digest.clone().finish().as_ref());
        let manifest = RecordingManifest {
            version,
            source_kind: format!("recording-directory-v{version}"),
            snapshot: "snapshot.json".into(),
            binding: self.snapshot.content.binding.clone(),
            records: "records.jsonl".into(),
            records_sha256,
            record_count: self.count,
        };
        self.write_budgeted("manifest.json", &serde_json::to_vec(&manifest)?)?;
        self.diagnostics.measure("verification", || -> Result<()> {
            let mut verify = RecordingDirectorySource::open(&self.stage)?;
            while verify.validate_next()? {}
            self.diagnostics.verified(&verify);
            Ok(())
        })?;
        let _commit_guard = acquire()?;
        self.diagnostics
            .measure("fsync", || fs::File::open(&self.stage)?.sync_all())?;
        // No prepared COMPLETE marker survives a failed no-replace commit.
        self.diagnostics.measure("rename", || {
            depth_output::rename_directory_no_replace(&self.stage, &self.final_path)
        })?;
        self.completed = true;
        let final_path = self.final_path.clone();
        let completed = (|| -> Result<()> {
            let marker = format!("recording-directory-v{version}; no business validity decision\n");
            self.reserve_bytes(marker.len())?;
            write_private_observed(
                &final_path.join("COMPLETE"),
                marker.as_bytes(),
                &self.diagnostics,
            )?;
            self.diagnostics
                .measure("fsync", || fs::File::open(&final_path)?.sync_all())?;
            let parent = final_path
                .parent()
                .ok_or(OfflineError::Invalid("RECORDING_PARENT"))?;
            self.diagnostics
                .measure("fsync", || fs::File::open(parent)?.sync_all())?;
            Ok(())
        })();
        if let Err(error) = completed {
            // Keep publication failure visibly partial where possible. Never
            // overwrite a concurrently-created stage when rolling back.
            let _ = fs::remove_file(final_path.join("COMPLETE"));
            if self
                .diagnostics
                .measure("rename", || {
                    depth_output::rename_directory_no_replace(&final_path, &self.stage)
                })
                .is_ok()
            {
                self.completed = false;
            }
            return Err(error);
        }
        self.diagnostics
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .durable_pairs = self.count;
        Ok(final_path)
    }
}
impl Drop for RawRecordingWriter {
    fn drop(&mut self) {
        if !self.completed {
            if !self.failed && self.chunked.is_some() {
                let _ = self.checkpoint();
            }
            let _ = self.write_budgeted(
                "INTERRUPTED.json",
                br#"{"version":1,"complete":false,"business_validity":null}"#,
            );
        }
    }
}
fn new_private(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
fn write_private_observed(
    path: &Path,
    bytes: &[u8],
    diagnostics: &RawRecordingDiagnostics,
) -> std::io::Result<()> {
    use std::io::Write;
    let file = diagnostics.measure("write", || -> std::io::Result<fs::File> {
        let mut file = new_private(path)?;
        file.write_all(bytes)?;
        Ok(file)
    })?;
    diagnostics.measure("fsync", || file.sync_all())
}

#[cfg(test)]
mod streaming_hash_tests {
    use super::*;
    #[test]
    fn streaming_hash_matches_whole_extent_hash_at_buffer_boundaries() {
        let data: Vec<u8> = (0..131089).map(|n| (n % 251) as u8).collect();
        let mut buffer = Vec::new();
        for length in [0, 1, 65535, 65536, 65537, 131089] {
            let mut input = std::io::Cursor::new(&data);
            assert_eq!(
                hash_extent(&mut input, length as u64, &mut buffer).unwrap(),
                sha256(&data[..length])
            );
            assert_eq!(input.position(), length as u64);
            assert!(buffer.capacity() <= VALIDATION_BUFFER_BYTES);
        }
        let mut truncated = std::io::Cursor::new(&data[..100]);
        assert!(hash_extent(&mut truncated, 101, &mut buffer).is_err());
    }
}
