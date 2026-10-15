//! Command-line interface: argument definitions and dispatch.

#[cfg(target_os = "linux")]
mod attach;
mod format;
mod list;
mod status;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use uuid::Uuid;

#[cfg(not(target_os = "linux"))]
mod attach {
    use anyhow::{bail, Result};
    pub fn attach(_: super::AttachArgs) -> Result<()> {
        bail!("attach is only supported on Linux")
    }
    pub fn detach(_: super::DetachArgs) -> Result<()> {
        bail!("detach is only supported on Linux")
    }
    pub fn daemon(_: super::DaemonArgs) -> Result<()> {
        bail!("daemon is only supported on Linux")
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "wlvl",
    version,
    about = "Wear-leveling block device for SD cards and USB drives"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Write a new wlvl volume onto a device (destroys existing data).
    Format(FormatArgs),
    /// List block devices holding wlvl volumes.
    List(ListArgs),
    /// Expose a volume as /dev/ublkbN.
    Attach(AttachArgs),
    /// Remove the /dev/ublkbN device of a volume.
    Detach(DetachArgs),
    /// Show wear and health of a volume.
    Status(StatusArgs),
    /// Serve every configured volume until SIGTERM (for systemd).
    Daemon(DaemonArgs),
}

#[derive(Args, Debug)]
pub struct DaemonArgs {
    /// TOML configuration file.
    #[arg(long, value_name = "PATH", default_value = "/etc/wlvl/config.toml")]
    pub config: PathBuf,
    /// Additional volume UUIDs to serve with default settings.
    #[arg(long = "uuid", value_name = "UUID")]
    pub uuids: Vec<Uuid>,
    /// Tolerate a missing config file (serve only --uuid volumes).
    #[arg(long)]
    pub ignore_missing_config: bool,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Volume UUID.
    #[arg(long, value_name = "UUID")]
    pub uuid: Option<Uuid>,
    /// Device path instead of a UUID.
    #[arg(long, value_name = "PATH")]
    pub device: Option<PathBuf>,
    /// Emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,
    /// Skip smartctl / mmc sysfs queries.
    #[arg(long)]
    pub no_hardware: bool,
    /// Expected program/erase cycles of the flash (for the health estimate).
    #[arg(long, value_name = "N", default_value_t = 3000)]
    pub expected_pe_cycles: u64,
    /// Control socket of the daemon (default: /run/wlvl/control.sock, then per-volume socket).
    #[arg(long, value_name = "PATH")]
    pub control_socket: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct AttachArgs {
    /// Volume UUID (found by scanning block devices).
    #[arg(long, value_name = "UUID", required_unless_present = "device")]
    pub uuid: Option<Uuid>,
    /// Device path instead of (or in addition to) the UUID.
    #[arg(long, value_name = "PATH")]
    pub device: Option<PathBuf>,
    /// ublk device id; -1 picks the next free one.
    #[arg(long, value_name = "N", default_value_t = -1, allow_hyphen_values = true)]
    pub dev_id: i32,
    /// Number of hardware queues.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub queues: u16,
    /// Queue depth (max in-flight requests per queue).
    #[arg(long, value_name = "N", default_value_t = 64)]
    pub depth: u16,
    /// Max bytes per request.
    #[arg(long, value_name = "SIZE", default_value = "512K", value_parser = parse_size_u32)]
    pub io_buf_bytes: u32,
    /// Seconds between timer-driven metadata commits.
    #[arg(long, value_name = "SECS", default_value_t = 30)]
    pub flush_interval_secs: u64,
    /// Seconds between wear-leveling policy checks.
    #[arg(long, value_name = "SECS", default_value_t = 60)]
    pub check_interval_secs: u64,
    /// Minimum seconds between two chunk migrations.
    #[arg(long, value_name = "SECS", default_value_t = 600)]
    pub rotate_interval_secs: u64,
    /// Migrate only when max-min chunk write counts exceed this.
    #[arg(long, value_name = "N", default_value_t = 50)]
    pub wear_delta_threshold: u64,
    /// Serve I/O only; never migrate chunks.
    #[arg(long)]
    pub no_wear_leveling: bool,
    /// Read back one in every N written segments to catch silent corruption (0 = off).
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub verify_every: u32,
    /// Control socket path (default: /run/wlvl/<uuid>.sock).
    #[arg(long, value_name = "PATH")]
    pub control_socket: Option<PathBuf>,
    /// Expected program/erase cycles of the flash (for the health estimate).
    #[arg(long, value_name = "N", default_value_t = 3000)]
    pub expected_pe_cycles: u64,
    /// Stay in the foreground instead of daemonizing.
    #[arg(long)]
    pub foreground: bool,
}

#[derive(Args, Debug)]
pub struct DetachArgs {
    /// Volume UUID.
    #[arg(long, value_name = "UUID", required_unless_present = "dev_id")]
    pub uuid: Option<Uuid>,
    /// ublk device id (the N in /dev/ublkbN).
    #[arg(long, value_name = "N")]
    pub dev_id: Option<u32>,
}

#[derive(Args, Debug)]
pub struct FormatArgs {
    /// Block device (or file) to format.
    #[arg(long, value_name = "PATH")]
    pub device: PathBuf,
    /// Chunk size, e.g. 4M, 8M, 1G. Power of two.
    #[arg(long, value_name = "SIZE", default_value = "4M", value_parser = parse_size)]
    pub chunk_size: u64,
    /// Percentage of the pool reserved as spare chunks.
    #[arg(long, value_name = "PCT", default_value_t = crate::ondisk::layout::DEFAULT_SPARE_PCT)]
    pub spare_pct: u8,
    /// Number of metadata ring slots.
    #[arg(long, value_name = "N", default_value_t = crate::ondisk::layout::DEFAULT_META_SLOTS)]
    pub meta_slots: u32,
    /// Use this UUID instead of generating one.
    #[arg(long, value_name = "UUID")]
    pub uuid: Option<Uuid>,
    /// Overwrite an existing wlvl volume.
    #[arg(long)]
    pub force: bool,
    /// Confirm the destructive operation.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Format(a) => format::run(a),
        Command::List(a) => list::run(a),
        Command::Attach(a) => attach::attach(a),
        Command::Detach(a) => attach::detach(a),
        Command::Status(a) => status::run(a),
        Command::Daemon(a) => attach::daemon(a),
    }
}

fn parse_size_u32(s: &str) -> std::result::Result<u32, String> {
    u32::try_from(parse_size(s)?).map_err(|_| format!("size '{s}' does not fit in 32 bits"))
}

/// Parse sizes like `4096`, `64K`, `4M`, `1G` (binary units).
pub fn parse_size(s: &str) -> std::result::Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => {
            let m = match c.to_ascii_uppercase() {
                'K' => 1u64 << 10,
                'M' => 1u64 << 20,
                'G' => 1u64 << 30,
                _ => return Err(format!("unknown size suffix '{c}'")),
            };
            (&s[..s.len() - 1], m)
        }
        _ => (s, 1),
    };
    let n: u64 = num.parse().map_err(|_| format!("invalid size '{s}'"))?;
    n.checked_mul(mult)
        .ok_or_else(|| format!("size '{s}' overflows"))
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("64K").unwrap(), 65536);
        assert_eq!(parse_size("4M").unwrap(), 4 << 20);
        assert_eq!(parse_size("1g").unwrap(), 1 << 30);
        assert!(parse_size("4X").is_err());
        assert!(parse_size("").is_err());
    }
}
