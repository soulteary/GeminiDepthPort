//! Read-only resource preflight. No sysfs/sysctl/module writes exist here.
use crate::util::*;
use anyhow::Result;
use serde_json::{Value, json};
use std::{fs, path::Path};
pub fn status_at(path: &Path, required_mb: Option<u64>) -> Value {
    match fs::read_to_string(path)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(current) => {
            json!({"version":1,"kind":"usbfs-resource-status","available":true,"current_mb":current,"unlimited":current==0,"required_mb":required_mb,"insufficient":required_mb.is_some_and(|minimum|current!=0 && current<minimum),"capacity_certified":false,"host_written":false,"code":if required_mb.is_none(){"USBFS_BUDGET_UNDECLARED"}else{"USBFS_BUDGET_CHECKED"}})
        }
        None => {
            json!({"version":1,"kind":"usbfs-resource-status","available":false,"required_mb":required_mb,"capacity_certified":false,"host_written":false,"code":"USBFS_UNAVAILABLE"})
        }
    }
}
pub fn status(d: &Value) -> Value {
    status_at(
        Path::new("/sys/module/usbcore/parameters/usbfs_memory_mb"),
        d["usbfs_minimum_mb"].as_u64(),
    )
}
pub fn preflight(d: &Value) -> Result<()> {
    check_status(d, &status(d))
}
fn check_status(d: &Value, status: &Value) -> Result<()> {
    if d.get("usbfs_minimum_mb").is_some() {
        require(
            status["available"] == true,
            "USBFS_UNAVAILABLE",
            "Cannot read configured USBFS resource bound",
        )?;
        require(
            status["insufficient"] == false,
            "USBFS_RESOURCE_INSUFFICIENT",
            "USBFS is below the declared minimum; use usbfs-status and explicit administrator usbfs_control.py, never automatic host modification",
        )?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_are_declared_not_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("parameter");
        fs::write(&file, "16\n").unwrap();
        assert_eq!(status_at(&file, Some(128))["insufficient"], true);
        assert_eq!(status_at(&file, None)["code"], "USBFS_BUDGET_UNDECLARED");
        fs::write(&file, "0\n").unwrap();
        assert_eq!(status_at(&file, Some(128))["insufficient"], false);
        assert!(
            !status_at(&file, Some(128))["capacity_certified"]
                .as_bool()
                .unwrap()
        );
    }
    #[test]
    fn configured_usbfs_preflight_refuses_unavailable_or_insufficient_resources_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("parameter");
        let d = json!({"usbfs_minimum_mb":128});
        assert!(
            check_status(&d, &status_at(&file, Some(128)))
                .unwrap_err()
                .to_string()
                .starts_with("USBFS_UNAVAILABLE")
        );
        fs::write(&file, b"16\n").unwrap();
        assert!(
            check_status(&d, &status_at(&file, Some(128)))
                .unwrap_err()
                .to_string()
                .starts_with("USBFS_RESOURCE_INSUFFICIENT")
        );
        fs::write(&file, b"0\n").unwrap();
        assert!(check_status(&d, &status_at(&file, Some(128))).is_ok());
        assert_eq!(fs::read(&file).unwrap(), b"0\n");
    }
}
