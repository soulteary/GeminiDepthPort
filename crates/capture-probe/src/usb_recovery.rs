//! Explicit, fingerprinted pre-SDK USB transport recovery. No retry or camera-setting changes.
use anyhow::{Context, Result, ensure};
use ring::digest;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    path::{Component, PathBuf},
    process::{Command, Stdio},
};
pub(crate) struct Recovery {
    helper: PathBuf,
    sha256: String,
}
impl Recovery {
    pub(crate) fn parse(value: &Value) -> Result<Self> {
        let object = value.as_object().context("USB recovery object")?;
        ensure!(
            object.len() == 2 && object.contains_key("helper") && object.contains_key("sha256"),
            "USB recovery schema"
        );
        let helper = PathBuf::from(value["helper"].as_str().context("USB recovery helper")?);
        ensure!(
            helper.is_absolute()
                && !helper
                    .components()
                    .any(|p| matches!(p, Component::ParentDir)),
            "absolute USB recovery helper required"
        );
        let sha256 = value["sha256"]
            .as_str()
            .context("USB recovery digest")?
            .to_owned();
        capture_contract::ConfigurationId::parse(&sha256)?;
        Ok(Self { helper, sha256 })
    }
    pub(crate) fn verify(&self) -> Result<()> {
        let mut text = String::with_capacity(64);
        const HEX: &[u8] = b"0123456789abcdef";
        for b in digest::digest(&digest::SHA256, &fs::read(&self.helper)?).as_ref() {
            text.push(HEX[(b >> 4) as usize] as char);
            text.push(HEX[(b & 15) as usize] as char);
        }
        ensure!(text == self.sha256, "USB recovery artifact mismatch");
        Ok(())
    }
    pub(crate) fn execute(&self, serial: &str) -> Result<Value> {
        self.verify()?;
        let mut child = Command::new(&self.helper)
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        // Identity is private stdin, never process arguments or public output.
        let operation = (|| -> Result<Value> {
            child
                .stdin
                .take()
                .context("USB recovery stdin")?
                .write_all(serial.as_bytes())?;
            let mut bytes = Vec::new();
            child
                .stdout
                .take()
                .context("USB recovery stdout")?
                .take(4097)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 4096, "USB recovery response limit");
            let status = child.wait()?;
            if !status.success() {
                let code = capture_contract::parse_strict(&bytes)
                    .ok()
                    .and_then(|v| v["code"].as_str().map(str::to_owned))
                    .filter(|s| {
                        !s.is_empty()
                            && s.len() <= 64
                            && s.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
                    })
                    .unwrap_or_else(|| "USB_RECOVERY_FAILED".into());
                anyhow::bail!("USB stream recovery failed: {code}");
            }
            let value = capture_contract::parse_strict(&bytes)?;
            ensure!(
                value.as_object().is_some_and(|m| m.len() == 4)
                    && value["version"] == 1
                    && value["complete"] == true
                    && value["interfaces"]
                        .as_u64()
                        .is_some_and(|n| (1..=255).contains(&n))
                    && value["endpoints"]
                        .as_u64()
                        .is_some_and(|n| (1..=255).contains(&n)),
                "USB recovery response invalid"
            );
            Ok(
                json!({"version":1,"state":"complete","action":"clear-video-bulk-in-endpoints","interfaces":value["interfaces"],"endpoints":value["endpoints"]}),
            )
        })();
        if operation.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        operation
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_is_explicit_closed_and_fingerprinted() {
        assert!(Recovery::parse(&json!({"helper":"relative","sha256":"a".repeat(64)})).is_err());
        assert!(
            Recovery::parse(&json!({"helper":"/helper","sha256":"a".repeat(64),"retry":3}))
                .is_err()
        );
        assert!(Recovery::parse(&json!({"helper":"/helper","sha256":"bad"})).is_err());
        let path = std::env::current_exe().unwrap();
        let policy = Recovery::parse(&json!({"helper":path,"sha256":"a".repeat(64)})).unwrap();
        assert!(policy.verify().is_err());
    }
    struct Helper(std::path::PathBuf);
    impl Drop for Helper {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn helper(body: &str) -> (Helper, Recovery) {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "gdp-usb-helper-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("helper");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let mut sha = String::with_capacity(64);
        for byte in digest::digest(&digest::SHA256, &fs::read(&path).unwrap()).as_ref() {
            use std::fmt::Write as _;
            write!(&mut sha, "{byte:02x}").unwrap();
        }
        let policy = Recovery::parse(&json!({"helper":path,"sha256":sha})).unwrap();
        (Helper(root), policy)
    }
    #[test]
    fn recovery_helper_receives_private_stdin_and_returns_only_validated_counts() {
        let (_dir, policy) = helper(
            "[ \"$#\" = 2 ] && [ \"$1\" = --parent-pid ] || exit 2\ns=$(cat)\n[ \"$s\" = synthetic-private-serial ] || exit 3\nprintf '{\"version\":1,\"complete\":true,\"interfaces\":2,\"endpoints\":3}'",
        );
        let result = policy.execute("synthetic-private-serial").unwrap();
        assert_eq!(result["state"], "complete");
        assert_eq!(result["interfaces"], 2);
        assert_eq!(result["endpoints"], 3);
        assert!(!result.to_string().contains("serial"));
    }
    #[test]
    fn recovery_errors_never_echo_untrusted_helper_diagnostics() {
        for code in [
            "USB_DEVICE_MISSING",
            "private-serial-path",
            "",
            &"A".repeat(65),
        ] {
            let body = format!(
                "cat >/dev/null\nprintf '%s' '{}'\nexit 1",
                json!({"code":code})
            );
            let (_dir, policy) = helper(&body);
            let error = policy
                .execute("synthetic-private-serial")
                .unwrap_err()
                .to_string();
            assert_eq!(
                error,
                format!(
                    "USB stream recovery failed: {}",
                    if code == "USB_DEVICE_MISSING" {
                        code
                    } else {
                        "USB_RECOVERY_FAILED"
                    }
                )
            );
        }
    }
    #[test]
    fn recovery_rejects_oversized_malformed_or_unbounded_success_records() {
        for value in [
            json!({"version":1,"complete":false,"interfaces":2,"endpoints":2}),
            json!({"version":1,"complete":true,"interfaces":0,"endpoints":2}),
            json!({"version":1,"complete":true,"interfaces":2,"endpoints":256}),
            json!({"version":1,"complete":true,"interfaces":2,"endpoints":2,"extra":true}),
        ] {
            let (_dir, policy) = helper(&format!("cat >/dev/null\nprintf '%s' '{value}'"));
            assert!(policy.execute("private").is_err());
        }
        for body in [
            "cat >/dev/null\nprintf not-json",
            "cat >/dev/null\nhead -c 4097 /dev/zero",
        ] {
            let (_dir, policy) = helper(body);
            assert!(policy.execute("private").is_err());
        }
    }
}
