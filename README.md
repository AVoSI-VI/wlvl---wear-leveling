# wlvl - wear-leveling block device for SD cards and USB drives

`wlvl` is a Linux userspace daemon written in Rust. It puts a virtual block
device (`/dev/ublkbN`, via the kernel's ublk driver) in front of a raw SD card
or USB stick, remaps erase-block-sized chunks so that logical write hot-spots
(filesystem journals, FAT tables, superblocks) do not keep landing on the same
physical sectors, and keeps per-chunk wear and health statistics on the device
itself.

The card's own controller still does its internal wear leveling; `wlvl` spreads
the *logical* workload the controller sees, and gives you host-side visibility
into how hard each region has been written.

## Requirements

- Linux 6.0 or newer with `CONFIG_BLK_DEV_UBLK` (`modprobe ublk_drv`).
  Stock WSL2 kernels do not ship it.
- Root (or `CAP_SYS_ADMIN`) to open `/dev/ublk-control` and to enter a
  private mount namespace.
- Rust 1.85+ to build. `libublk` generates kernel bindings at build time, so
  `libclang` and the kernel UAPI headers (`linux-libc-dev`) must be installed.
- Optional: `smartmontools` for SMART data from USB-SATA/NVMe bridges.

```sh
cargo build --release
sudo install -m 0755 target/release/wlvl /usr/local/bin/wlvl
```

## Quick start

```sh
# 1. Format the raw device (destroys its contents). Prints the volume UUID.
sudo wlvl format --device /dev/sdb --yes
# 2. Expose it. Prints /dev/ublkbN and keeps serving in the background.
sudo wlvl attach --uuid <UUID>
# 3. Use /dev/ublkbN like any disk.
sudo mkfs.ext4 /dev/ublkb0 && sudo mount /dev/ublkb0 /mnt
# 4. Inspect wear and health (live counters while attached).
sudo wlvl status --uuid <UUID>
# 5. Stop serving. Writes the final metadata commit.
sudo umount /mnt && sudo wlvl detach --uuid <UUID>
```

`wlvl list` scans `/sys/class/block` for formatted volumes and shows which ones
are attached.

## Daemon mode (systemd)

```sh
sudo mkdir -p /etc/wlvl
sudo cp contrib/config.example.toml /etc/wlvl/config.toml   # edit UUIDs
sudo cp contrib/wlvl.service /etc/systemd/system/
sudo systemctl enable --now wlvl
```

`wlvl daemon` serves every `[[device]]` in the config (plus any `--uuid` given on
the command line), answers `wlvl status` on `/run/wlvl/control.sock`, and on
SIGTERM commits metadata and deletes the ublk devices.

## Running the OS from the same SD card (Raspberry Pi)

`wlvl` is a userspace daemon, so `/dev/ublkbN` only exists while it runs. The
daemon therefore has to live on a plain partition: keep the boot and root
partitions raw and put the write-heavy data on a wlvl volume in a third
partition. On a Pi almost all SD wear comes from `/var` (logs, journald, apt
caches, databases, containers) and `/home`, not from the mostly-read-only root,
so this layout protects the parts that actually wear out.

```
mmcblk0p1   /boot/firmware   FAT, raw
mmcblk0p2   /                ext4, raw (ideally read-only with a tmpfs overlay)
mmcblk0p3   wlvl volume  ->  /dev/ublkb0  ->  /var, /home, /srv, ...
```

Setup, from a running system with a free third partition:

```sh
# Check the kernel first; Raspberry Pi OS kernels may not ship ublk.
zcat /proc/config.gz | grep BLK_DEV_UBLK   # or: modinfo ublk_drv

sudo wlvl format --device /dev/mmcblk0p3 --yes          # prints <UUID>
sudo wlvl attach --uuid <UUID>                           # prints /dev/ublkb0
sudo mkfs.ext4 -L wldata /dev/ublkb0
sudo mount /dev/ublkb0 /mnt
sudo rsync -aHAX /var/ /mnt/var/ && sudo rsync -aHAX /home/ /mnt/home/
sudo umount /mnt && sudo wlvl detach --uuid <UUID>
```

Then make it permanent. `/etc/wlvl/config.toml`:

```toml
[[device]]
uuid = "<UUID>"
name = "pi-data"
```

`/etc/fstab` (bind mounts from the one ublk filesystem; the `x-systemd.*`
options order them after `wlvl.service` and wait for the device to appear):

```
/dev/ublkb0            /srv/wldata  ext4  defaults,noatime,x-systemd.requires=wlvl.service,x-systemd.device-timeout=30  0 2
/srv/wldata/var        /var         none  bind,x-systemd.requires-mounts-for=/srv/wldata  0 0
/srv/wldata/home       /home        none  bind,x-systemd.requires-mounts-for=/srv/wldata  0 0
```

Install and enable the unit as in the previous section, create `/srv/wldata`,
and reboot. `contrib/wlvl.service` already starts before `local-fs.target`, so the
bind mounts come up during early boot; the raw root partition only needs to
hold the OS and the `wlvl` binary.

Notes:

- `wlvl attach`/`wlvl daemon` refuse a target partition that is mounted, but a
  mounted root on `mmcblk0p2` does not block a volume on `mmcblk0p3`.
- Use a stable identity (the volume UUID, or `path = "/dev/disk/by-id/..."`),
  not `/dev/mmcblk0p3`, if the card may ever move between readers.
- The native SD slot exposes the card as `/dev/mmcblk0`, so `wlvl status` can
  read CID/serial from sysfs; SD cards expose no `life_time`, so health is
  inferred from `wlvl`'s own counters.
- Pairing this with a read-only root (Raspberry Pi OS "Overlay File System" in
  `raspi-config`, or `overlayroot`) removes the remaining writes to the raw
  root partition.

## Configuration

See [`contrib/config.example.toml`](contrib/config.example.toml). Devices are
identified by the volume UUID written at format time, so `/dev/sdX`
renumbering does not matter. A stable `path` such as `/dev/disk/by-id/...` is
accepted as an alternative.

| key | default | meaning |
|---|---|---|
| `metadata_flush_interval_secs` | 30 | timer-driven commit of wear counters |
| `check_interval_secs` | 60 | how often the wear policy runs |
| `rotate_interval_secs` | 600 | minimum spacing between migrations |
| `wear_delta_threshold` | 50 | act when max-min chunk writes exceed this |
| `verify_every` | 0 | read back one in N writes (0 = off) |
| `expected_pe_cycles` | 3000 | endurance assumption for the health score |
| `retire_threshold` | 3 | I/O errors before a chunk is evacuated |
| `hardware_health` | true | query smartctl / mmc sysfs in `status` |

## How it works

### On-disk layout

The device is divided into chunks (default 4 MiB, power of two):

```
chunk 0            superblock (magic, version, volume UUID, geometry, CRC32C)
chunks 1..1+N      metadata slot ring (default N = 8)
remaining chunks   pool: data chunks + spare chunks (default 5%)
last chunk         superblock mirror
```

Metadata (the logical-to-physical mapping plus one record per physical chunk:
write count, I/O errors, verification failures, latency EWMA, state) is written
as a whole image into the next ring slot on every commit, stamped with a
generation number and CRC. Recovery picks the newest valid image, so a torn
write only loses the most recent commit. The ring rotation also spreads the
metadata writes themselves.

Commits happen on a timer, on filesystem FLUSH (rate limited), after each
migration (synchronously, see below) and at shutdown. Counters may therefore
lag by up to one interval after a crash.

### Wear leveling

A worker evaluates the pool periodically:

1. **Retire**: a chunk with too many I/O or verification errors is evacuated
   and never used again.
2. **Dynamic**: the hottest logical chunk sitting on an above-mean-wear
   physical chunk moves to the least-worn spare.
3. **Static**: the coldest logical chunk sitting on a fresh physical chunk
   moves onto the most-worn spare, freeing the fresh chunk for hot data.

Migration never blocks I/O. While copying, reads go to the old chunk and
writes go to both; ranges written during the copy are re-copied. If a chunk is
written continuously, new writes to that one chunk are held for the few
milliseconds needed to finish the last re-copy. The mapping is switched in
memory, **committed to disk while writes still mirror to the old chunk**, and
only then is the old chunk released. A crash at any point leaves an on-disk
mapping whose target holds complete data.

### Health

`wlvl status` reports an inferred health score (0-100) per device from write
counts against `expected_pe_cycles`, error and verification counts and latency
drift, plus a wear histogram and the worst chunks. It also asks the hardware
where possible:

- `smartctl --json -d sat` (then auto) for USB bridges that pass SMART through.
- `/sys/class/block/mmcblkN/device/{life_time,pre_eol_info,cid,...}` for cards
  on a native MMC host. `life_time`/`pre_eol_info` exist on eMMC only; SD
  cards behind USB readers expose nothing, and the report says so.

## Caveats

- The raw device holds `wlvl`'s own format. Access data only through
  `/dev/ublkbN`. `wlvl format` refuses mounted devices and existing volumes
  (unless `--force`).
- Do not mount `/dev/ublkbN` from the same mount namespace as the serving
  process; `wlvl` unshares its mount namespace at start for that reason
  (see the libublk README on the self-deadlock on exit).
- DISCARD is not advertised in this version.

## Development

The on-disk format, mapper, wear policy, persistence and inferred health are
OS-independent and tested against plain files (`cargo test` works on any OS).
The ublk target, device scanning and the daemon are `target_os = "linux"`.

End-to-end testing needs a Linux host or VM with `ublk_drv`:

```sh
truncate -s 1G /tmp/card.img && sudo losetup -fP --show /tmp/card.img   # -> /dev/loopX
sudo wlvl format --device /dev/loopX --yes
sudo RUST_LOG=debug wlvl attach --device /dev/loopX --foreground --check-interval-secs 5 --rotate-interval-secs 5 --wear-delta-threshold 5
# in another shell
sudo mkfs.ext4 /dev/ublkb0 && sudo mount /dev/ublkb0 /mnt && fio --name=w --rw=randwrite --bs=4k --size=64M --directory=/mnt
sudo wlvl status --device /dev/loopX
```
