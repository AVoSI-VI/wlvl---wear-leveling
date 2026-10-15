//! Best-effort SMART data via `smartctl --json` (smartmontools >= 7.0).
//!
//! USB-SATA/NVMe bridges often answer through SAT pass-through; plain USB
//! sticks and SD readers usually do not. Absence is reported, not fatal.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartAttr {
    pub id: u32,
    pub name: String,
    pub value: Option<u32>,
    pub worst: Option<u32>,
    pub thresh: Option<u32>,
    pub raw: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartReport {
    pub available: bool,
    pub source: String,
    pub device_type_tried: Vec<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub smart_passed: Option<bool>,
    pub temperature_c: Option<i64>,
    pub power_on_hours: Option<u64>,
    pub percentage_used: Option<u64>,
    pub attributes: Vec<SmartAttr>,
    pub error: Option<String>,
}

/// Query `dev`, trying SAT pass-through first, then auto-detection.
pub fn query(dev: &Path) -> SmartReport {
    let mut report = SmartReport {
        source: "smartctl".into(),
        ..Default::default()
    };
    for dtype in [Some("sat"), None] {
        let label = dtype.unwrap_or("auto").to_string();
        report.device_type_tried.push(label);
        match run(dev, dtype) {
            Ok(json) => {
                if let Some(r) = parse(&json, report.device_type_tried.clone()) {
                    return r;
                }
                report.error = Some("smartctl produced no SMART data".into());
            }
            Err(e) => {
                report.error = Some(e);
                if report
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains("not found"))
                {
                    break;
                }
            }
        }
    }
    report
}

fn run(dev: &Path, dtype: Option<&str>) -> Result<serde_json::Value, String> {
    let mut cmd = Command::new("smartctl");
    cmd.arg("--json").arg("-a");
    if let Some(t) = dtype {
        cmd.arg("-d").arg(t);
    }
    cmd.arg(dev);
    let out = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "smartctl not found on PATH".to_string()
        } else {
            format!("running smartctl: {e}")
        }
    })?;
    // smartctl encodes informational conditions in its exit code; the JSON is still valid.
    if out.stdout.is_empty() {
        return Err(format!("smartctl exited with {} and no output", out.status));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("parsing smartctl JSON: {e}"))
}

/// Interpret smartctl's JSON document. Public so it can be tested without smartctl.
pub fn parse(json: &serde_json::Value, device_type_tried: Vec<String>) -> Option<SmartReport> {
    let messages: Vec<String> = json["smartctl"]["messages"]
        .as_array()
        .map(|m| {
            m.iter()
                .filter_map(|x| x["string"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let open_failed = json["smartctl"]["exit_status"]
        .as_u64()
        .is_some_and(|s| s & 0b11 != 0);
    let has_data = json.get("ata_smart_attributes").is_some()
        || json.get("nvme_smart_health_information_log").is_some()
        || json.get("smart_status").is_some();
    if open_failed || !has_data {
        return None;
    }
    let mut r = SmartReport {
        available: true,
        source: "smartctl".into(),
        device_type_tried,
        model: json["model_name"].as_str().map(str::to_string),
        serial: json["serial_number"].as_str().map(str::to_string),
        smart_passed: json["smart_status"]["passed"].as_bool(),
        temperature_c: json["temperature"]["current"].as_i64(),
        power_on_hours: json["power_on_time"]["hours"].as_u64(),
        percentage_used: json["nvme_smart_health_information_log"]["percentage_used"].as_u64(),
        attributes: Vec::new(),
        error: if messages.is_empty() {
            None
        } else {
            Some(messages.join("; "))
        },
    };
    if let Some(table) = json["ata_smart_attributes"]["table"].as_array() {
        for a in table {
            r.attributes.push(SmartAttr {
                id: a["id"].as_u64().unwrap_or(0) as u32,
                name: a["name"].as_str().unwrap_or("").to_string(),
                value: a["value"].as_u64().map(|v| v as u32),
                worst: a["worst"].as_u64().map(|v| v as u32),
                thresh: a["thresh"].as_u64().map(|v| v as u32),
                raw: a["raw"]["string"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| a["raw"]["value"].to_string()),
            });
        }
    }
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ata_document() {
        let doc = serde_json::json!({
            "smartctl": {"exit_status": 0, "messages": []},
            "model_name": "Example SSD",
            "serial_number": "ABC123",
            "smart_status": {"passed": true},
            "temperature": {"current": 41},
            "power_on_time": {"hours": 1234},
            "ata_smart_attributes": {"table": [
                {"id": 5, "name": "Reallocated_Sector_Ct", "value": 100, "worst": 100, "thresh": 10, "raw": {"value": 0, "string": "0"}},
                {"id": 177, "name": "Wear_Leveling_Count", "value": 97, "worst": 97, "thresh": 0, "raw": {"value": 42, "string": "42"}}
            ]}
        });
        let r = parse(&doc, vec!["sat".into()]).unwrap();
        assert!(r.available);
        assert_eq!(r.model.as_deref(), Some("Example SSD"));
        assert_eq!(r.smart_passed, Some(true));
        assert_eq!(r.temperature_c, Some(41));
        assert_eq!(r.attributes.len(), 2);
        assert_eq!(r.attributes[1].raw, "42");
        assert!(r.error.is_none());
    }

    #[test]
    fn open_failure_is_unavailable() {
        let doc = serde_json::json!({"smartctl": {"exit_status": 2, "messages": [{"string": "Unable to open device", "severity": "error"}]}});
        assert!(parse(&doc, vec![]).is_none());
        let doc =
            serde_json::json!({"smartctl": {"exit_status": 0}, "device": {"name": "/dev/sdb"}});
        assert!(
            parse(&doc, vec![]).is_none(),
            "no SMART sections means unavailable"
        );
    }
}
