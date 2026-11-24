//! Bounded read-only queries. No group-atomicity or business-validity claim.
use crate::{control::Controller, managed_quality, util::require};
use anyhow::Result;
use quality_core::EventCursor;
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_lowercase() || (i > 0 && (b.is_ascii_digit() || b == b'_')))
}
fn keys(value: &Value, allowed: &[&str]) -> Result<()> {
    require(
        value
            .as_object()
            .is_some_and(|o| o.keys().all(|k| allowed.contains(&k.as_str()))),
        "QUALITY_REQUEST_SCHEMA",
        "Unknown request field",
    )
}
fn bound(value: &Value, expected: &Value) -> Result<()> {
    keys(
        expected,
        &["generation_id", "instance_id", "session_id", "snapshot_id"],
    )?;
    let valid_hex = |key: &str, len: usize| {
        expected[key].as_str().is_some_and(|v| {
            v.len() == len
                && v.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    };
    require(
        expected.as_object().is_some_and(|o| o.len() == 4)
            && valid_hex("generation_id", 32)
            && valid_hex("instance_id", 64)
            && valid_hex("session_id", 32)
            && (expected["snapshot_id"].is_null() || valid_hex("snapshot_id", 64))
            && ["generation_id", "instance_id", "session_id", "snapshot_id"]
                .iter()
                .all(|k| value[k] == expected[k]),
        "QUALITY_BINDING_CHANGED",
        "Expected capture binding differs",
    )
}
pub fn execute(ctl: &Controller, kind: &str, request: &Value) -> Result<Value> {
    execute_with(kind, request, |camera, kind, cursor, limit| {
        managed_quality::query(ctl, camera, kind, cursor, limit)
    })
}
pub fn execute_with(
    mut_kind: &str,
    request: &Value,
    mut query: impl FnMut(&str, &str, Option<EventCursor>, usize) -> Result<Value>,
) -> Result<Value> {
    let started = std::time::Instant::now();
    require(
        request["version"] == 1,
        "QUALITY_REQUEST_VERSION",
        "Unsupported request",
    )?;
    match mut_kind {
        "quality-batch" => {
            keys(request, &["version", "cameras"])?;
            let cameras = request["cameras"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("QUALITY_REQUEST_SCHEMA"))?;
            require(
                !cameras.is_empty() && cameras.len() <= 3,
                "QUALITY_CAMERA_LIMIT",
                "Expected one to three cameras",
            )?;
            let mut seen = BTreeSet::new();
            let mut output = vec![];
            for item in cameras {
                keys(item, &["camera", "expected", "cursor", "limit"])?;
                let camera = item["camera"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("QUALITY_REQUEST_SCHEMA"))?;
                require(
                    valid_name(camera) && seen.insert(camera),
                    "QUALITY_CAMERA_DUPLICATE_OR_INVALID",
                    "Invalid camera group",
                )?;
                let limit = item
                    .get("limit")
                    .map(|v| v.as_u64().unwrap_or(0))
                    .unwrap_or(50);
                require(
                    (1..=1024).contains(&limit),
                    "QUALITY_LIMIT",
                    "Invalid page size",
                )?;
                let cursor = item
                    .get("cursor")
                    .map(|v| serde_json::from_value(v.clone()))
                    .transpose()?;
                let before = query(camera, "current", None, limit as usize)?;
                bound(&before, &item["expected"])?;
                let events = query(camera, "events", cursor, limit as usize)?;
                bound(&events, &item["expected"])?;
                let after = query(camera, "current", None, limit as usize)?;
                bound(&after, &item["expected"])?;
                output.push(json!({"camera":camera,"before":before,"events":events,"after":after}));
            }
            Ok(
                json!({"version":1,"kind":"quality-batch","cameras":output,"atomic_group_snapshot":false,
                "elapsed_ns":started.elapsed().as_nanos().min(u64::MAX as u128) as u64,"business_validity":null}),
            )
        }
        "quality-window" => {
            keys(
                request,
                &[
                    "version",
                    "camera",
                    "expected",
                    "from_cursor",
                    "to_sequence",
                    "limit",
                ],
            )?;
            let camera = request["camera"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("QUALITY_REQUEST_SCHEMA"))?;
            require(
                valid_name(camera),
                "QUALITY_CAMERA_INVALID",
                "Invalid camera",
            )?;
            let cursor: EventCursor = serde_json::from_value(request["from_cursor"].clone())?;
            let after = request["from_cursor"]["after_sequence"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("QUALITY_CURSOR_INVALID"))?;
            let end = request["to_sequence"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("QUALITY_WINDOW_INVALID"))?;
            let limit = request
                .get("limit")
                .map(|v| v.as_u64().unwrap_or(0))
                .unwrap_or(1024);
            require(
                end >= after && (1..=1024).contains(&limit),
                "QUALITY_WINDOW_INVALID",
                "Invalid window or limit",
            )?;
            require(
                request["from_cursor"]["origin_session_id"] == request["expected"]["session_id"],
                "QUALITY_CURSOR_ORIGIN",
                "Cursor differs",
            )?;
            let envelope = query(camera, "events", Some(cursor), limit as usize)?;
            bound(&envelope, &request["expected"])?;
            let mut source_metadata = envelope.clone();
            source_metadata.as_object_mut().unwrap().remove("result");
            let mut page = envelope["result"]["page"].clone();
            let latest = page["latest_sequence"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("QUALITY_PAGE_INVALID"))?;
            require(
                end <= latest,
                "QUALITY_WINDOW_NOT_AVAILABLE",
                "Requested upper boundary not observed",
            )?;
            let events = page["events"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("QUALITY_PAGE_INVALID"))?;
            let selected = events
                .iter()
                .filter(|e| {
                    e["sequence"]
                        .as_u64()
                        .is_some_and(|s| s > after && s <= end)
                })
                .cloned()
                .collect::<Vec<_>>();
            if page["gap"].is_null() {
                let mut previous = after;
                for event in &selected {
                    let sequence = event["sequence"]
                        .as_u64()
                        .ok_or_else(|| anyhow::anyhow!("QUALITY_PAGE_INVALID"))?;
                    require(
                        previous.checked_add(1) == Some(sequence),
                        "QUALITY_WINDOW_DISCONTINUITY",
                        "Event window is not contiguous",
                    )?;
                    previous = sequence;
                }
            }
            let reached = selected
                .last()
                .and_then(|e| e["sequence"].as_u64())
                .unwrap_or(after);
            let complete = reached == end && page["gap"].is_null();
            // A requested window wholly before retained history cannot advance.
            // Preserve the actual cursor/gap, but terminate this incomplete window.
            let evicted = selected.is_empty() && reached < end && !page["gap"].is_null();
            require(
                !selected.is_empty() || reached == end || evicted,
                "QUALITY_WINDOW_DISCONTINUITY",
                "No retained event can advance the requested window",
            )?;
            page["events"] = json!(selected);
            page["next_cursor"] = json!({"origin_session_id":request["expected"]["session_id"],"after_sequence":reached});
            page["has_more"] = json!(reached < end && !evicted);
            Ok(
                json!({"version":1,"kind":"quality-window","binding":request["expected"],"camera":camera,
                "from_exclusive":after,"to_inclusive":end,"page":page,"window_complete":complete,
                "business_validity":null,"live_query":true,"source":source_metadata,
                "terminal_reason":if evicted {Some("WINDOW_EVICTED")} else {None}}),
            )
        }
        _ => anyhow::bail!("QUALITY_QUERY_KIND"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> Value {
        json!({"generation_id":"b".repeat(32),"instance_id":"c".repeat(64),"session_id":"a".repeat(32),"snapshot_id":null})
    }

    fn window_request() -> Value {
        json!({"version":1,"camera":"front","expected":binding(),"from_cursor":{"origin_session_id":"a".repeat(32),"after_sequence":0},"to_sequence":1})
    }

    #[test]
    fn malformed_batch_schema_and_cursor_do_not_query_the_provider() {
        let err = execute_with(
            "quality-batch",
            &json!({"version":1,"cameras":null}),
            |_, _, _, _| panic!("invalid schema must not query a provider"),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "QUALITY_REQUEST_SCHEMA");
        let request = json!({"version":1,"cameras":[{"camera":"front","expected":binding(),"cursor":{"origin_session_id":"invalid","after_sequence":0}}]});
        assert!(
            execute_with("quality-batch", &request, |_, _, _, _| {
                panic!("invalid cursor must not query a provider")
            })
            .is_err()
        );
    }

    #[test]
    fn batch_forwards_a_valid_cursor_only_to_the_events_query() {
        let request = json!({"version":1,"cameras":[{"camera":"front","expected":binding(),"cursor":{"origin_session_id":"a".repeat(32),"after_sequence":7}}]});
        let mut calls = vec![];
        let result = execute_with("quality-batch", &request, |camera, kind, cursor, limit| {
            assert_eq!(camera, "front");
            assert_eq!(limit, 50);
            if kind == "events" {
                let cursor = cursor.unwrap();
                assert_eq!(cursor.after_sequence, 7);
                assert_eq!(cursor.origin_session_id.as_str(), "a".repeat(32));
            } else {
                assert!(cursor.is_none());
            }
            calls.push(kind.to_owned());
            Ok(binding())
        })
        .unwrap();
        assert_eq!(calls, ["current", "events", "current"]);
        assert_eq!(result["atomic_group_snapshot"], false);
    }

    #[test]
    fn non_numeric_window_boundary_is_rejected_before_querying() {
        let mut request = window_request();
        request["to_sequence"] = json!("one");
        let err = execute_with("quality-window", &request, |_, _, _, _| {
            panic!("invalid window must not query a provider")
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "QUALITY_WINDOW_INVALID");
    }

    #[test]
    fn window_rejects_malformed_provider_page_fields() {
        for page in [
            json!({"latest_sequence":"one","events":[]}),
            json!({"latest_sequence":1,"events":null}),
        ] {
            let mut envelope = binding();
            envelope["result"] = json!({"page":page});
            let err = execute_with("quality-window", &window_request(), |_, _, _, _| {
                Ok(envelope.clone())
            })
            .unwrap_err();
            assert_eq!(err.to_string(), "QUALITY_PAGE_INVALID");
        }
    }
}
