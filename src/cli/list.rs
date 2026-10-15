//! `wlvl list`

use super::ListArgs;
use crate::backing;
use anyhow::{Context, Result};

pub fn run(a: ListArgs) -> Result<()> {
    let entries = backing::scan().context("scanning block devices")?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    if entries.is_empty() {
        eprintln!("no wlvl volumes found");
        return Ok(());
    }
    let attached = attached_paths();
    println!(
        "{:<36}  {:<16}  {:>10}  {:>8}  {:<7}  {:<12}  ALIASES",
        "UUID", "DEVICE", "CHUNK", "CHUNKS", "SB", "ATTACHED"
    );
    for e in &entries {
        let sb = match (e.primary_ok, e.mirror_ok) {
            (true, true) => "ok",
            (true, false) => "mirror!",
            (false, true) => "primary!",
            (false, false) => "none",
        };
        let aliases: Vec<String> = e.aliases.iter().map(|p| p.display().to_string()).collect();
        let att = attached
            .iter()
            .find(|(u, _)| *u == e.uuid)
            .map(|(_, p)| p.as_str())
            .unwrap_or("-");
        println!(
            "{:<36}  {:<16}  {:>10}  {:>8}  {:<7}  {:<12}  {}",
            e.uuid,
            e.path.display(),
            e.chunk_size,
            e.total_chunks,
            sb,
            att,
            aliases.join(" ")
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn attached_paths() -> Vec<(uuid::Uuid, String)> {
    crate::ublk_target::list_attached()
        .into_iter()
        .map(|d| (d.uuid, d.bdev_path))
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn attached_paths() -> Vec<(uuid::Uuid, String)> {
    Vec::new()
}
