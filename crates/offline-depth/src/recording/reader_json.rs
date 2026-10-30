//! Reject duplicate keys without building an intermediate JSON value tree.
//! Typed deserialization remains responsible for schema and unknown-field checks.
use super::*;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use std::{borrow::Cow, collections::BTreeSet, fmt};
struct Key<'a>(Cow<'a, str>);
impl<'de> Deserialize<'de> for Key<'de> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct K;
        impl<'de> Visitor<'de> for K {
            type Value = Key<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON key")
            }
            fn visit_borrowed_str<E: de::Error>(
                self,
                s: &'de str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(Key(Cow::Borrowed(s)))
            }
            fn visit_str<E: de::Error>(self, s: &str) -> std::result::Result<Self::Value, E> {
                Ok(Key(Cow::Owned(s.into())))
            }
        }
        d.deserialize_str(K)
    }
}
struct Unique;
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Unique;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON with unique keys and finite numbers")
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Unique, E> {
                if v.is_finite() {
                    Ok(Unique)
                } else {
                    Err(E::custom("nonfinite"))
                }
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Unique, A::Error> {
                while a.next_element::<Unique>()?.is_some() {}
                Ok(Unique)
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut keys = BTreeSet::new();
                while let Some(k) = a.next_key::<Key<'de>>()? {
                    if !keys.insert(k.0) {
                        return Err(de::Error::custom("duplicate key"));
                    }
                    a.next_value::<Unique>()?;
                }
                Ok(Unique)
            }
        }
        d.deserialize_any(V)
    }
}
pub(super) fn parse<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    use capture_contract::{ContractError, ErrorClass};
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(ContractError::new(ErrorClass::InvalidInput, "JSON_SIZE").into());
    }
    serde_json::from_slice::<Unique>(bytes)
        .map_err(|_| ContractError::new(ErrorClass::InvalidInput, "JSON_INVALID_OR_DUPLICATE"))?;
    Ok(serde_json::from_slice(bytes)?)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_semantics_match_value_parser_for_nested_and_escaped_inputs() {
        for raw in [
            br#"{"a":{"x":1,"x":2}}"#.as_slice(),
            br#"{"a":1,"\u0061":2}"#,
            br#"[1,{"q":true,"q":false}]"#,
            br#"{"a":NaN}"#,
            br#"{"a":1e999}"#,
            br#"{"a":1} trailing"#,
            br#"{"a":"\uD800"}"#,
        ] {
            assert!(capture_contract::parse_strict(raw).is_err());
            assert!(parse::<serde_json::Value>(raw).is_err());
        }
        for raw in [br#"{"a":null,"b":[true,false,-1,18446744073709551615,-0.0,0.1],"nested":{"escaped\"key":"value"}}"#.as_slice(),br#"{"a":1,"A":2,"\u0062":3}"#,br#"[]"#,br#""hello""#] {
            assert_eq!(parse::<serde_json::Value>(raw).unwrap(),capture_contract::parse_strict(raw).unwrap());
        }
        assert!(parse::<serde_json::Value>(&vec![b' '; 4 * 1024 * 1024 + 1]).is_err());
        let deep = format!("{}0{}", "[".repeat(140), "]".repeat(140));
        assert!(parse::<serde_json::Value>(deep.as_bytes()).is_err());
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Schema {
            _number: u64,
        }
        assert!(parse::<Schema>(br#"{"_number":1,"other":2}"#).is_err());
        assert!(parse::<Schema>(br#"{"_number":1.0}"#).is_err());
    }
    #[test]
    fn recording_numeric_bits_match_legacy_value_conversion() {
        // f32 direct JSON rounding can differ from Value -> f32 rounding.
        // Test the actual tagged evidence schema, not a surrogate f32 struct.
        let context = serde_json::json!({"version":1,"session_id":"a".repeat(32),"generation_id":"b".repeat(32),"configuration_id":"c".repeat(64),"calibration_id":"d".repeat(64),"snapshot_id":"e".repeat(64),"stream_id":"depth","sequence":1,"device_timestamp_us":1,"system_timestamp_us":2,"received_mono_offset_ns":0});
        let frame = serde_json::json!({"context":context,"encoding":"depth16","width":2,"height":1,"stride":4,"source_depth_scale_mm":"SCALE","output_depth_scale_mm":"SCALE","normalized":false});
        let value=serde_json::json!({"kind":"sdk-bound-pair","pair":{"version":1,"pair_sequence":1,"color":frame,"depth":frame,"color_stamp_us":1,"depth_stamp_us":1,"clock_source":"device_us","delta_us":0,"association":"sdk_frameset","hardware_sync_verified":false}}).to_string();
        for number in [
            "1.0000000596046448",
            "0.10000000521540642",
            "3.4028235e38",
            "1e-45",
            "-0",
            "0.10000000149011612",
            "1.00000005960464477539062500000000001",
        ] {
            let raw = value.replace("\"SCALE\"", number);
            let old: TaggedEvidence = super::super::parse(raw.as_bytes()).unwrap();
            let new: TaggedEvidence = parse(raw.as_bytes()).unwrap();
            let bits = |e: TaggedEvidence| match e {
                TaggedEvidence::SdkBoundPair { pair, .. } => [
                    pair.color.source_depth_scale_mm.map(f32::to_bits),
                    pair.depth.source_depth_scale_mm.map(f32::to_bits),
                    pair.depth.output_depth_scale_mm.map(f32::to_bits),
                ],
                _ => panic!("SDK fixture"),
            };
            assert_eq!(bits(old), bits(new), "{number}");
        }
        let raw = br#"[-0, -0.0, 0.1, 18446744073709551615, 1.7976931348623157e308, 5e-324]"#;
        let old: Vec<f64> = super::super::parse(raw).unwrap();
        let new: Vec<f64> = parse(raw).unwrap();
        assert_eq!(
            old.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            new.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
    }
}
