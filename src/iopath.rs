//! Synchronous reference implementation of the request path over [`BlockIo`].
//!
//! `ublk_target::io` implements the same semantics asynchronously on io_uring;
//! this version is used by tests, by the migration worker and by offline tools.

use crate::mapper::{IoOutcome, Mapper};
use crate::ondisk::BlockIo;
use std::io;
use std::time::Instant;

/// Read `buf.len()` bytes at logical `offset`.
pub fn read<M: Mapper + ?Sized, B: BlockIo + ?Sized>(
    m: &M,
    io: &B,
    offset: u64,
    buf: &mut [u8],
) -> io::Result<()> {
    let layout = m.layout();
    check_range(layout.exported_bytes(), offset, buf.len() as u64)?;
    let mut done = 0usize;
    for (lchunk, in_chunk, len) in layout.split_range(offset, buf.len() as u64) {
        let pchunk = m.resolve_read(lchunk);
        let poff = layout.chunk_offset(pchunk) + in_chunk;
        let started = Instant::now();
        let res = io.read_at(&mut buf[done..done + len as usize], poff);
        m.end_read(
            pchunk,
            IoOutcome {
                latency: started.elapsed(),
                ok: res.is_ok(),
            },
        );
        res?;
        done += len as usize;
    }
    Ok(())
}

/// Write `buf` at logical `offset`, mirroring to a migration destination when one exists.
pub fn write<M: Mapper + ?Sized, B: BlockIo + ?Sized>(
    m: &M,
    io: &B,
    offset: u64,
    buf: &[u8],
) -> io::Result<()> {
    let layout = m.layout();
    check_range(layout.exported_bytes(), offset, buf.len() as u64)?;
    let mut done = 0usize;
    for (lchunk, in_chunk, len) in layout.split_range(offset, buf.len() as u64) {
        let resolved = m.begin_write(lchunk);
        let data = &buf[done..done + len as usize];
        let started = Instant::now();
        let mut res = io.write_at(data, layout.chunk_offset(resolved.primary) + in_chunk);
        if let Some(mirror) = resolved.mirror {
            let mres = io.write_at(data, layout.chunk_offset(mirror) + in_chunk);
            if res.is_ok() {
                res = mres;
            }
        }
        m.end_write(
            lchunk,
            in_chunk,
            len,
            IoOutcome {
                latency: started.elapsed(),
                ok: res.is_ok(),
            },
        );
        res?;
        done += len as usize;
    }
    Ok(())
}

fn check_range(exported: u64, offset: u64, len: u64) -> io::Result<()> {
    match offset.checked_add(len) {
        Some(end) if end <= exported => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "request beyond end of device",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapper::ChunkMapper;
    use crate::ondisk::layout::MIB;
    use crate::ondisk::{FormatOptions, Volume};

    fn setup() -> (std::fs::File, ChunkMapper) {
        let f = tempfile::tempfile().unwrap();
        f.set_len(128 * MIB).unwrap();
        let vol = Volume::format(
            f,
            &FormatOptions {
                chunk_size: 4 * MIB,
                meta_slots: 2,
                spare_pct: 10,
                ..Default::default()
            },
        )
        .unwrap();
        let (io, _sb, layout, _ring, md) = vol.into_parts();
        (io, ChunkMapper::new(layout, md))
    }

    #[test]
    fn write_read_roundtrip_across_chunk_boundary() {
        let (io, m) = setup();
        let cs = m.layout().chunk_size;
        let data: Vec<u8> = (0..16384u32).map(|i| (i % 251) as u8).collect();
        let off = cs - 8192;
        write(&m, &io, off, &data).unwrap();
        let mut back = vec![0u8; data.len()];
        read(&m, &io, off, &mut back).unwrap();
        assert_eq!(back, data);
        // Data must physically sit at pool_start + {0,1}.
        let p0 = m.layout().chunk_offset(m.layout().pool_start()) + cs - 8192;
        let mut raw = vec![0u8; 8192];
        io.read_at(&mut raw, p0).unwrap();
        assert_eq!(&raw[..], &data[..8192]);
        m.with_metadata(|md| {
            assert_eq!(md.stats[m.layout().pool_start() as usize].write_count, 1);
            assert_eq!(
                md.stats[m.layout().pool_start() as usize + 1].write_count,
                1
            );
        });
    }

    #[test]
    fn rejects_out_of_range() {
        let (io, m) = setup();
        let end = m.layout().exported_bytes();
        assert!(write(&m, &io, end - 512, &[0u8; 1024]).is_err());
        assert!(read(&m, &io, end, &mut [0u8; 1]).is_err());
        write(&m, &io, end - 512, &[1u8; 512]).unwrap();
    }
}
