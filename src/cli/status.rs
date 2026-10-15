//! `wlvl status`: live report from the daemon when attached, otherwise read from the device.

use super::StatusArgs;
use crate::backing::{self, BackingDevice};
#[cfg(unix)]
use crate::control::{self, Request, Response};
use crate::health::{HealthParams, StatusReport};
use crate::ondisk::Volume;
use anyhow::{bail, Context, Result};
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use uuid::Uuid;

pub fn run(a: StatusArgs) -> Result<()> {
    let params = HealthParams {
        expected_pe_cycles: a.expected_pe_cycles,
        ..Default::default()
    };
    let with_hw = !a.no_hardware;

    let report = match (a.uuid, a.device.as_deref()) {
        (None, None) => return list_live(&a),
        (uuid, device) => {
            let path = backing::resolve(uuid, device).context("resolving device")?;
            let uuid = match uuid {
                Some(u) => u,
                None => {
                    crate::backing::ScanEntry::probe(&path)
                        .with_context(|| format!("{} holds no wlvl volume", path.display()))?
                        .uuid
                }
            };
            match query_live(uuid, a.control_socket.as_deref(), with_hw) {
                Some(r) => r,
                None => offline(&path, uuid, &params, with_hw)?,
            }
        }
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_table(&report);
    }
    Ok(())
}

fn offline(path: &Path, uuid: Uuid, params: &HealthParams, with_hw: bool) -> Result<StatusReport> {
    let dev =
        BackingDevice::open(path, false).with_context(|| format!("opening {}", path.display()))?;
    let vol =
        Volume::open(dev).with_context(|| format!("opening wlvl volume on {}", path.display()))?;
    if vol.uuid() != uuid {
        bail!(
            "{} holds volume {}, not {}",
            path.display(),
            vol.uuid(),
            uuid
        );
    }
    Ok(StatusReport::offline(path, &vol, params, with_hw))
}

#[cfg(unix)]
fn candidate_sockets(uuid: Uuid, explicit: Option<&Path>) -> Vec<PathBuf> {
    match explicit {
        Some(p) => vec![p.to_path_buf()],
        None => vec![
            PathBuf::from(control::DEFAULT_SOCKET),
            control::per_volume_socket(uuid),
        ],
    }
}

#[cfg(unix)]
fn query_live(uuid: Uuid, explicit: Option<&Path>, hardware: bool) -> Option<StatusReport> {
    for sock in candidate_sockets(uuid, explicit) {
        match control::query(&sock, &Request::Status { uuid, hardware }) {
            Ok(Response::Status { status }) => return Some(*status),
            Ok(Response::Error { message }) => {
                tracing::debug!(socket = %sock.display(), message, "daemon has no such volume")
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(socket = %sock.display(), error = %e, "control socket unavailable")
            }
        }
    }
    None
}

#[cfg(not(unix))]
fn query_live(_uuid: Uuid, _explicit: Option<&Path>, _hardware: bool) -> Option<StatusReport> {
    None
}

fn list_live(a: &StatusArgs) -> Result<()> {
    #[cfg(unix)]
    {
        let sock = a
            .control_socket
            .clone()
            .unwrap_or_else(|| PathBuf::from(control::DEFAULT_SOCKET));
        match control::query(&sock, &Request::List) {
            Ok(Response::List { volumes }) => {
                if a.json {
                    println!("{}", serde_json::to_string_pretty(&volumes)?);
                } else if volumes.is_empty() {
                    eprintln!("daemon at {} has no attached volumes", sock.display());
                } else {
                    for v in volumes {
                        println!(
                            "{}  {}  {}",
                            v.uuid,
                            v.device.display(),
                            v.bdev.unwrap_or_else(|| "-".into())
                        );
                    }
                }
                Ok(())
            }
            Ok(other) => bail!("unexpected response: {other:?}"),
            Err(e) => bail!(
                "no daemon at {} ({e}); pass --uuid or --device for an offline report",
                sock.display()
            ),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = a;
        bail!("pass --uuid or --device")
    }
}

fn print_table(r: &StatusReport) {
    let h = &r.health;
    println!("volume      {}", r.uuid);
    println!("device      {}", r.device.display());
    println!("attached    {}", r.attached.as_deref().unwrap_or("no"));
    println!(
        "source      {}",
        if r.live {
            "live daemon counters"
        } else {
            "on-disk metadata (may lag the last flush)"
        }
    );
    println!("generation  {}", r.generation);
    let g = &r.geometry;
    println!(
        "geometry    {} chunks x {} bytes; {} data, {} spare, {} meta slots; exported {} bytes",
        g.total_chunks, g.chunk_size, g.data_chunks, g.spare_chunks, g.meta_slots, g.exported_bytes
    );
    println!();
    println!(
        "health      device {}/100  (min chunk {}, mean chunk {})",
        h.device_score, h.min_chunk_score, h.mean_chunk_score
    );
    println!(
        "endurance   {}.{:02}% of expected P/E cycles used (mean writes/chunk {})",
        h.endurance_used_centi_pct / 100,
        h.endurance_used_centi_pct % 100,
        h.wear.mean
    );
    println!(
        "wear        min {}  max {}  delta {}  pool {}  free {}  retired {}",
        h.wear.min,
        h.wear.max,
        h.wear.max.saturating_sub(h.wear.min),
        h.wear.pool_chunks,
        h.wear.free_chunks,
        h.wear.retired_chunks
    );
    println!(
        "errors      {} chunks with I/O errors, {} with verify failures",
        h.chunks_with_errors, h.chunks_with_verify_failures
    );
    println!(
        "latency     min {}us  mean {}us  max {}us",
        h.latency.min_us, h.latency.mean_us, h.latency.max_us
    );
    println!("histogram   (write count -> chunks)");
    let widest = h
        .histogram
        .iter()
        .map(|b| b.count)
        .max()
        .unwrap_or(1)
        .max(1);
    for b in &h.histogram {
        let bar = "#".repeat((b.count * 40 / widest) as usize);
        println!("  {:>8}-{:<8} {:>7} {}", b.lo, b.hi, b.count, bar);
    }
    if !h.worst_chunks.is_empty() {
        println!("worst chunks (phys, score, writes, errors, verify, latency us)");
        for c in &h.worst_chunks {
            println!(
                "  {:>8} {:>5} {:>8} {:>6} {:>6} {:>8}",
                c.pchunk, c.score, c.write_count, c.io_errors, c.verify_failures, c.latency_ewma_us
            );
        }
    }
    if let Some(m) = &r.migration {
        println!(
            "migration   logical {} : phys {} -> {} ({:?})",
            m.lchunk, m.src, m.dst, m.phase
        );
    }
    if let Some(w) = &r.wear {
        println!(
            "worker      {} migrations done, {} failed, {} chunks retired",
            w.migrations_done, w.migrations_failed, w.chunks_retired
        );
        if let Some(e) = &w.last_error {
            println!("            last error: {e}");
        }
    }
    if let Some(hw) = &r.hardware {
        println!();
        match (&hw.smart.available, &hw.smart.error) {
            (true, _) => {
                println!(
                    "smart       {} {}  passed={:?}  temp={:?}C  hours={:?}  pct_used={:?}",
                    hw.smart.model.as_deref().unwrap_or("?"),
                    hw.smart.serial.as_deref().unwrap_or(""),
                    hw.smart.smart_passed,
                    hw.smart.temperature_c,
                    hw.smart.power_on_hours,
                    hw.smart.percentage_used
                );
                for a in &hw.smart.attributes {
                    println!(
                        "  {:>3} {:<28} value={:?} worst={:?} thresh={:?} raw={}",
                        a.id, a.name, a.value, a.worst, a.thresh, a.raw
                    );
                }
            }
            (false, e) => println!(
                "smart       unavailable ({})",
                e.as_deref().unwrap_or("no data")
            ),
        }
        if hw.mmc.available {
            println!(
                "mmc         type={} name={} serial={} life_time={:?}/{:?} pre_eol={}",
                hw.mmc.card_type.as_deref().unwrap_or("?"),
                hw.mmc.name.as_deref().unwrap_or("?"),
                hw.mmc.serial.as_deref().unwrap_or("?"),
                hw.mmc.life_time_a,
                hw.mmc.life_time_b,
                hw.mmc.pre_eol.as_deref().unwrap_or("n/a")
            );
        } else {
            println!(
                "mmc         unavailable ({})",
                hw.mmc.note.as_deref().unwrap_or("")
            );
        }
    }
    println!();
    println!("note        {}", h.note);
}
