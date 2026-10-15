//! TOML configuration for the daemon, merged with command-line UUIDs.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn d_flush_secs() -> u64 {
    30
}
fn d_socket() -> PathBuf {
    PathBuf::from(crate::control::DEFAULT_SOCKET)
}
fn d_rotate_secs() -> u64 {
    600
}
fn d_check_secs() -> u64 {
    60
}
fn d_wear_delta() -> u64 {
    50
}
fn d_pe_cycles() -> u64 {
    3000
}
fn d_retire() -> u16 {
    3
}
fn d_true() -> bool {
    true
}
fn d_queues() -> u16 {
    1
}
fn d_depth() -> u16 {
    64
}
fn d_io_buf() -> u32 {
    512 * 1024
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Global {
    #[serde(default = "d_flush_secs")]
    pub metadata_flush_interval_secs: u64,
    #[serde(default = "d_socket")]
    pub control_socket: PathBuf,
}

impl Default for Global {
    fn default() -> Self {
        Global {
            metadata_flush_interval_secs: d_flush_secs(),
            control_socket: d_socket(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    /// Volume UUID written by `wlvl format`.
    #[serde(default)]
    pub uuid: Option<Uuid>,
    /// Alternative to `uuid`: a stable device path such as `/dev/disk/by-id/...`.
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "d_rotate_secs")]
    pub rotate_interval_secs: u64,
    #[serde(default = "d_check_secs")]
    pub check_interval_secs: u64,
    #[serde(default = "d_wear_delta")]
    pub wear_delta_threshold: u64,
    /// Read back one in every N writes to detect silent corruption (0 = off).
    #[serde(default)]
    pub verify_every: u32,
    #[serde(default = "d_pe_cycles")]
    pub expected_pe_cycles: u64,
    #[serde(default = "d_retire")]
    pub retire_threshold: u16,
    #[serde(default = "d_true")]
    pub hardware_health: bool,
    #[serde(default = "d_true")]
    pub wear_leveling: bool,
    #[serde(default = "d_queues")]
    pub queues: u16,
    #[serde(default = "d_depth")]
    pub depth: u16,
    #[serde(default = "d_io_buf")]
    pub io_buf_bytes: u32,
}

impl DeviceConfig {
    pub fn for_uuid(uuid: Uuid) -> DeviceConfig {
        DeviceConfig {
            uuid: Some(uuid),
            path: None,
            name: None,
            rotate_interval_secs: d_rotate_secs(),
            check_interval_secs: d_check_secs(),
            wear_delta_threshold: d_wear_delta(),
            verify_every: 0,
            expected_pe_cycles: d_pe_cycles(),
            retire_threshold: d_retire(),
            hardware_health: true,
            wear_leveling: true,
            queues: d_queues(),
            depth: d_depth(),
            io_buf_bytes: d_io_buf(),
        }
    }

    pub fn label(&self) -> String {
        match (&self.name, &self.uuid, &self.path) {
            (Some(n), _, _) => n.clone(),
            (None, Some(u), _) => u.to_string(),
            (None, None, Some(p)) => p.display().to_string(),
            (None, None, None) => "<unnamed>".into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub global: Global,
    #[serde(default, rename = "device")]
    pub devices: Vec<DeviceConfig>,
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, String> {
        let c: Config = toml::from_str(text).map_err(|e| format!("invalid config: {e}"))?;
        c.validate()?;
        Ok(c)
    }

    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        Config::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn validate(&self) -> Result<(), String> {
        for (i, d) in self.devices.iter().enumerate() {
            if d.uuid.is_none() && d.path.is_none() {
                return Err(format!(
                    "device #{} ({}) needs a uuid or a path",
                    i + 1,
                    d.label()
                ));
            }
            if d.queues == 0 || d.depth == 0 || d.io_buf_bytes < 4096 {
                return Err(format!("device {} has invalid queue settings", d.label()));
            }
        }
        let mut seen = std::collections::HashSet::new();
        for d in &self.devices {
            if let Some(u) = d.uuid {
                if !seen.insert(u) {
                    return Err(format!("uuid {u} listed twice"));
                }
            }
        }
        Ok(())
    }

    /// Add CLI-supplied UUIDs that the file does not already mention.
    pub fn merge_cli_uuids(&mut self, uuids: &[Uuid]) {
        for &u in uuids {
            if !self.devices.iter().any(|d| d.uuid == Some(u)) {
                self.devices.push(DeviceConfig::for_uuid(u));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[global]
metadata_flush_interval_secs = 15
control_socket = "/run/wlvl/test.sock"

[[device]]
uuid = "6ba7b810-9dad-11d1-80b4-00c04fd430c8"
name = "sdcard"
rotate_interval_secs = 120
wear_delta_threshold = 20
verify_every = 50
expected_pe_cycles = 1000
hardware_health = false

[[device]]
path = "/dev/disk/by-id/usb-Generic_Flash_Disk-0:0"
"#;

    #[test]
    fn parses_and_defaults() {
        let c = Config::parse(SAMPLE).unwrap();
        assert_eq!(c.global.metadata_flush_interval_secs, 15);
        assert_eq!(
            c.global.control_socket,
            PathBuf::from("/run/wlvl/test.sock")
        );
        assert_eq!(c.devices.len(), 2);
        let d = &c.devices[0];
        assert_eq!(d.label(), "sdcard");
        assert_eq!(
            (
                d.rotate_interval_secs,
                d.check_interval_secs,
                d.wear_delta_threshold
            ),
            (120, 60, 20)
        );
        assert_eq!(
            (d.verify_every, d.expected_pe_cycles, d.hardware_health),
            (50, 1000, false)
        );
        assert!(d.wear_leveling && c.devices[1].hardware_health);
        assert_eq!(c.devices[1].queues, 1);
    }

    #[test]
    fn shipped_example_parses() {
        let c = Config::parse(include_str!("../contrib/config.example.toml")).unwrap();
        assert_eq!(c.devices.len(), 1);
        assert_eq!(c.devices[0].label(), "wlvl-sdcard");
        assert_eq!(c.global, Global::default());
        assert_eq!(
            c.devices[0],
            DeviceConfig {
                name: Some("wlvl-sdcard".into()),
                ..DeviceConfig::for_uuid(c.devices[0].uuid.unwrap())
            }
        );
    }

    #[test]
    fn empty_config_is_valid() {
        let c = Config::parse("").unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn rejects_bad_configs() {
        assert!(Config::parse("[[device]]\nname = \"x\"\n")
            .unwrap_err()
            .contains("needs a uuid or a path"));
        assert!(Config::parse("[global]\nbogus = 1\n").is_err());
        let dup = "[[device]]\nuuid = \"6ba7b810-9dad-11d1-80b4-00c04fd430c8\"\n[[device]]\nuuid = \"6ba7b810-9dad-11d1-80b4-00c04fd430c8\"\n";
        assert!(Config::parse(dup).unwrap_err().contains("twice"));
        assert!(Config::parse(
            "[[device]]\nuuid = \"6ba7b810-9dad-11d1-80b4-00c04fd430c8\"\ndepth = 0\n"
        )
        .is_err());
    }

    #[test]
    fn merge_adds_missing_uuids_only() {
        let mut c = Config::parse(SAMPLE).unwrap();
        let existing = c.devices[0].uuid.unwrap();
        let new = Uuid::new_v4();
        c.merge_cli_uuids(&[existing, new]);
        assert_eq!(c.devices.len(), 3);
        assert_eq!(c.devices[2].uuid, Some(new));
        assert_eq!(
            c.devices[0].rotate_interval_secs, 120,
            "file settings win over CLI defaults"
        );
    }
}
