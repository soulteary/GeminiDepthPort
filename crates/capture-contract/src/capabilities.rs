//! Additive capability declarations, never runtime readiness or active protection.
use serde_json::{Value, json};
pub fn build_info(component: &str, version: &str) -> Value {
    static DIGEST: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let digest = DIGEST.get_or_init(|| {
        #[cfg(target_os = "linux")]
        let file = std::fs::File::open("/proc/self/exe").ok();
        #[cfg(not(target_os = "linux"))]
        let file = std::env::current_exe()
            .ok()
            .and_then(|path| std::fs::File::open(path).ok());
        file.and_then(|mut file| {
            use std::io::Read;
            let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
            let mut buffer = [0u8; 65536];
            loop {
                let n = file.read(&mut buffer).ok()?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            Some(crate::lower_hex(hash.finish().as_ref()))
        })
    });
    json!({"version":1,"component":component,"package_version":version,
        "source_revision":option_env!("GDP_SOURCE_REVISION"),
        "source_dirty":option_env!("GDP_SOURCE_DIRTY").and_then(|v|v.parse::<bool>().ok()),
        "executable_sha256":digest,"executable_hash_source":if cfg!(target_os="linux"){"proc-self-exe"}else{"current-exe-path"},"identity_attested":false})
}
pub fn describe(component: &str, version: &str) -> Value {
    json!({"version":1,"kind":"capabilities","build":build_info(component,version),
        "protocols":{"snapshot":[1],"bound_pair_evidence":[1],"recording_context":[1],
            "quality_query":[1],"quality_batch":[1],"quality_window":[1],"offline_input":[1,2,3],
            "offline_report":[1,2],"offline_progress":[1],"offline_diagnostics":[1]},
        "capture":{"maximum_configured_cameras":3,"backends":["official","rust"],
            "strong_session_protection_backend":"rust","active_protection_verified":false},
        "offline":{"modes":["png","tar","both"],"operations":["original-mm","normalize-mm","preserve-counts","d2c"],
            "distortion_models":["none","plumb-bob","rational-polynomial"],
            "unsupported_distortion_models":["equidistant"],"d2c_algorithm_version":1,
            "queue_capacity":{"minimum":1,"maximum":16},"cooperative_cancel":true,"resume":false},
        "worker_ready":false,"hardware_verified":false})
}

/// Select supported protocol versions. This never authorizes a runtime session.
pub fn negotiate(component: &str, version: &str, request: &Value) -> crate::Result<Value> {
    let invalid = || {
        crate::ContractError::new(
            crate::ErrorClass::InvalidInput,
            "CAPABILITY_REQUEST_INVALID",
        )
    };
    let unsupported =
        || crate::ContractError::new(crate::ErrorClass::InvalidInput, "CAPABILITY_UNSUPPORTED");
    let object = request.as_object().ok_or_else(invalid)?;
    if object.len() != 1 || !object.contains_key("protocols") {
        return Err(invalid());
    }
    let protocols = request["protocols"]
        .as_object()
        .filter(|o| !o.is_empty() && o.len() <= 32)
        .ok_or_else(invalid)?;
    let mut response = describe(component, version);
    for (key, wanted) in protocols {
        if !wanted.is_u64() {
            return Err(invalid());
        }
        if !response["protocols"][key]
            .as_array()
            .is_some_and(|v| v.contains(wanted))
        {
            return Err(unsupported());
        }
    }
    response["negotiated"] = request.clone();
    Ok(response)
}
