//! `wlvl`: a userspace wear-leveling block device for SD cards and USB drives.
//!
//! The OS-independent parts (on-disk format, mapping, wear policy, inferred
//! health) live here so they can be tested against a plain file anywhere. The
//! Linux-only ublk target and device helpers are gated on `target_os = "linux"`.

pub mod backing;
pub mod cli;
pub mod config;
pub mod control;
#[cfg(target_os = "linux")]
pub mod daemon;
pub mod health;
pub mod iopath;
pub mod mapper;
pub mod ondisk;
pub mod persist;
#[cfg(target_os = "linux")]
pub mod ublk_target;
pub mod wear;
