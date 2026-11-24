//! Durable recording-gate messages. Existence (including malformed content)
//! blocks automatic worker recovery; only authenticated management removes it.
use crate::{
    ConfigurationId, ContractError, ErrorClass, Result, SessionId, SnapshotId, VERSION, check,
    parse_strict, snapshot::SnapshotBinding,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GatePhase {
    Preparing,
    Active,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateRequest {
    pub version: u32,
    pub phase: GatePhase,
    pub request_id: SessionId,
    pub binding: SnapshotBinding,
    pub snapshot_id: SnapshotId,
    /// Hash of the immutable capture.json bytes, distinct from configuration content ID.
    pub runtime_config_sha256: ConfigurationId,
    pub token_sha256: ConfigurationId,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateAck {
    pub version: u32,
    pub request_id: SessionId,
    pub request_sha256: ConfigurationId,
    pub worker_pid: u32,
    pub supervisor_pid: u32,
    pub session_id: SessionId,
}
impl GateRequest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let request: Self = serde_json::from_value(parse_strict(bytes)?)
            .map_err(|_| ContractError::new(ErrorClass::Protection, "RECORDING_GATE_SCHEMA"))?;
        check(
            request.version == VERSION,
            ErrorClass::UnsupportedVersion,
            "RECORDING_GATE_VERSION",
        )?;
        Ok(request)
    }
    pub fn digest(&self) -> Result<ConfigurationId> {
        check(
            self.version == VERSION,
            ErrorClass::UnsupportedVersion,
            "RECORDING_GATE_VERSION",
        )?;
        let mut raw = b"geminidepthport:recording-gate:v1\0".to_vec();
        raw.extend(crate::snapshot::encode(self)?);
        ConfigurationId::parse(crate::snapshot::raw_sha(&raw))
    }
    pub fn acknowledge(
        &self,
        status: &Value,
        worker_pid: u32,
        supervisor_pid: u32,
        config_sha: &str,
    ) -> Result<GateAck> {
        check(
            self.version == VERSION
                && worker_pid > 0
                && supervisor_pid > 0
                && status["version"] == 1
                && status["kind"] == "capture-service-status"
                && status["ready"] == true
                && status["state"] == "ready"
                && status["pid"].as_u64() == Some(u64::from(worker_pid))
                && status["session_id"].as_str() == Some(self.binding.session_id.as_str())
                && status["instance_id"].as_str() == Some(self.binding.instance_id.as_str())
                && status["snapshot_id"].as_str() == Some(self.snapshot_id.as_str())
                && status["generation"].as_str() == Some(self.binding.generation_id.as_str())
                && status["config_sha256"].as_str() == Some(self.runtime_config_sha256.as_str())
                && config_sha == self.runtime_config_sha256.as_str(),
            ErrorClass::Protection,
            "RECORDING_GATE_SESSION_NOT_READY",
        )?;
        Ok(GateAck {
            version: VERSION,
            request_id: self.request_id.clone(),
            request_sha256: self.digest()?,
            worker_pid,
            supervisor_pid,
            session_id: self.binding.session_id.clone(),
        })
    }
    pub fn verify_ack(&self, ack: &GateAck, worker_pid: u32) -> Result<()> {
        check(
            ack.version == VERSION
                && ack.request_id == self.request_id
                && ack.request_sha256 == self.digest()?
                && ack.worker_pid == worker_pid
                && ack.supervisor_pid > 0
                && ack.session_id == self.binding.session_id,
            ErrorClass::Protection,
            "RECORDING_GATE_ACK_MISMATCH",
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_errors_remain_distinct_from_malformed_json() {
        for raw in [b"{}".as_slice(), b"[]", br#"{"version":1,"phase":false}"#] {
            let err = GateRequest::parse(raw).unwrap_err();
            assert_eq!(err.class, ErrorClass::Protection);
            assert_eq!(err.code, "RECORDING_GATE_SCHEMA");
        }
        let err = GateRequest::parse(br#"{"version":1,"version":1}"#).unwrap_err();
        assert_eq!(err.class, ErrorClass::InvalidInput);
        assert_eq!(err.code, "JSON_INVALID_OR_DUPLICATE");
    }

    #[test]
    fn ack_is_bound_to_exact_phase_session_and_config() {
        let s = SessionId::parse("a".repeat(32)).unwrap();
        let id = ConfigurationId::parse("b".repeat(64)).unwrap();
        let mut request = GateRequest {
            version: 1,
            phase: GatePhase::Preparing,
            request_id: s.clone(),
            binding: SnapshotBinding {
                instance_id: crate::InstanceId::parse("c".repeat(64)).unwrap(),
                session_id: s.clone(),
                generation_id: crate::GenerationId::parse("d".repeat(32)).unwrap(),
                configuration_id: id.clone(),
                calibration_id: crate::CalibrationId::parse("e".repeat(64)).unwrap(),
            },
            snapshot_id: SnapshotId::parse("f".repeat(64)).unwrap(),
            runtime_config_sha256: id.clone(),
            token_sha256: id.clone(),
        };
        let mut status = serde_json::json!({"version":1,"kind":"capture-service-status","ready":true,"state":"ready","pid":42,"session_id":s,"instance_id":request.binding.instance_id,"generation":request.binding.generation_id,"config_sha256":id,"snapshot_id":request.snapshot_id});
        let ack = request.acknowledge(&status, 42, 1, id.as_str()).unwrap();
        request.verify_ack(&ack, 42).unwrap();
        // A live session may have multiple different representation snapshots;
        // only the worker's current capture snapshot may authorize this gate.
        let original_snapshot = request.snapshot_id.clone();
        request.snapshot_id = SnapshotId::parse("0".repeat(64)).unwrap();
        assert!(request.acknowledge(&status, 42, 1, id.as_str()).is_err());
        request.snapshot_id = original_snapshot;
        status.as_object_mut().unwrap().remove("snapshot_id");
        assert!(request.acknowledge(&status, 42, 1, id.as_str()).is_err());
        status["snapshot_id"] = serde_json::json!(request.snapshot_id);
        request.phase = GatePhase::Active;
        assert!(request.verify_ack(&ack, 42).is_err());
        request.phase = GatePhase::Preparing;
        status["session_id"] = serde_json::json!("0".repeat(32));
        assert!(request.acknowledge(&status, 42, 1, id.as_str()).is_err());
        assert!(GateRequest::parse(b"{\"version\":1,\"version\":1}").is_err());
    }
}
