//! Per-queue I/O handling: translate each ublk request into backing-device
//! operations through the mapper. Async pattern as in libublk's `loop.rs`.

use super::{Target, BACKING_FIXED_FD};
use crate::mapper::{IoOutcome, Mapper};
use io_uring::{opcode, squeue, types};
use libublk::helpers::IoBuf;
use libublk::io::{UblkDev, UblkQueue};
use libublk::uring_async::ublk_submit_sqe_async;
use libublk::{BufDesc, UblkError, UblkUringData};
use std::rc::Rc;
use std::time::Instant;

const MAX_EAGAIN_RETRIES: usize = 4;

/// Entry point for one queue thread (`q_fn` of `run_target`).
pub fn queue_main(qid: u16, dev: &UblkDev, target: &Target) {
    let q_rc = match UblkQueue::new(qid, dev) {
        Ok(q) => Rc::new(q),
        Err(e) => {
            tracing::error!(qid, error = %e, "creating ublk queue failed");
            return;
        }
    };
    let exe_rc = Rc::new(smol::LocalExecutor::new());
    let exe = exe_rc.clone();
    let mut tasks = Vec::new();

    for tag in q_rc.tags() {
        let q = q_rc.clone();
        tasks.push(exe.spawn(async move {
            match io_task(&q, tag, target).await {
                Ok(()) | Err(UblkError::QueueIsDown) => {}
                Err(e) => tracing::error!(qid, tag, error = %e, "io task failed"),
            }
        }));
    }

    smol::block_on(exe_rc.run(async move {
        let run_ops = || while exe.try_tick() {};
        let done = || tasks.iter().all(|t| t.is_finished());
        if let Err(e) = libublk::wait_and_handle_io_events(&q_rc, Some(20), run_ops, done).await {
            tracing::error!(qid, error = %e, "queue event loop failed");
        }
    }));
}

/// Per-task state for sampled read-after-write verification.
struct Verifier {
    every: u32,
    counter: u32,
    scratch: IoBuf<u8>,
}

async fn io_task(q: &UblkQueue<'_>, tag: u16, target: &Target) -> Result<(), UblkError> {
    let bytes = q.dev.dev_info.max_io_buf_bytes as usize;
    let mut buf = IoBuf::<u8>::new(bytes);
    let mut verifier = (target.verify_every > 0).then(|| Verifier {
        every: target.verify_every,
        counter: 0,
        scratch: IoBuf::<u8>::new(bytes),
    });
    q.submit_io_prep_cmd(tag, BufDesc::Slice(buf.as_slice()), 0, Some(&buf))
        .await?;
    loop {
        let res = handle_request(q, tag, target, buf.as_mut_slice(), verifier.as_mut()).await;
        q.submit_io_commit_cmd(tag, BufDesc::Slice(buf.as_slice()), res)
            .await?;
    }
}

/// Returns bytes transferred, or a negative errno.
async fn handle_request(
    q: &UblkQueue<'_>,
    tag: u16,
    target: &Target,
    buf: &mut [u8],
    verifier: Option<&mut Verifier>,
) -> i32 {
    let iod = q.get_iod(tag);
    let op = iod.op_flags & 0xff;
    let offset = iod.start_sector << 9;
    let bytes = (iod.nr_sectors as u64) << 9;

    match op {
        libublk::sys::UBLK_IO_OP_FLUSH => {
            let res = flush().await;
            target.flush_signal.notify();
            res
        }
        libublk::sys::UBLK_IO_OP_READ | libublk::sys::UBLK_IO_OP_WRITE => {
            if bytes as usize > buf.len()
                || offset
                    .checked_add(bytes)
                    .is_none_or(|end| end > target.layout.exported_bytes())
            {
                return -libc::EINVAL;
            }
            if op == libublk::sys::UBLK_IO_OP_READ {
                read(target, offset, bytes, buf).await
            } else {
                write(target, offset, bytes, buf, verifier).await
            }
        }
        _ => -libc::EINVAL,
    }
}

async fn flush() -> i32 {
    let sqe = opcode::Fsync::new(types::Fixed(BACKING_FIXED_FD))
        .flags(types::FsyncFlags::DATASYNC)
        .build()
        .flags(squeue::Flags::FIXED_FILE);
    submit(sqe).await
}

async fn read(target: &Target, offset: u64, bytes: u64, buf: &mut [u8]) -> i32 {
    let mut done = 0u64;
    for (lchunk, in_chunk, len) in target.layout.split_range(offset, bytes) {
        let pchunk = target.mapper.resolve_read(lchunk);
        let poff = target.layout.chunk_offset(pchunk) + in_chunk;
        let ptr = buf[done as usize..].as_mut_ptr();
        let sqe = opcode::Read::new(types::Fixed(BACKING_FIXED_FD), ptr, len as u32)
            .offset(poff)
            .build()
            .flags(squeue::Flags::FIXED_FILE);
        let started = Instant::now();
        let res = submit_retrying(sqe).await;
        target.mapper.end_read(
            pchunk,
            IoOutcome {
                latency: started.elapsed(),
                ok: res == len as i32,
            },
        );
        if res < 0 {
            return res;
        }
        if res as u64 != len {
            return -libc::EIO;
        }
        done += len;
    }
    done as i32
}

async fn write(
    target: &Target,
    offset: u64,
    bytes: u64,
    buf: &[u8],
    mut verifier: Option<&mut Verifier>,
) -> i32 {
    let mut done = 0u64;
    for (lchunk, in_chunk, len) in target.layout.split_range(offset, bytes) {
        let resolved = target.mapper.begin_write(lchunk);
        let ptr = buf[done as usize..].as_ptr();
        let started = Instant::now();
        let mut res = write_segment(target, resolved.primary, in_chunk, ptr, len).await;
        if let Some(mirror) = resolved.mirror {
            let mres = write_segment(target, mirror, in_chunk, ptr, len).await;
            if res >= 0 && mres != len as i32 {
                res = if mres < 0 { mres } else { -libc::EIO };
            }
        }
        let ok = res == len as i32;
        target.mapper.end_write(
            lchunk,
            in_chunk,
            len,
            IoOutcome {
                latency: started.elapsed(),
                ok,
            },
        );
        if res < 0 {
            return res;
        }
        if !ok {
            return -libc::EIO;
        }
        if let Some(v) = verifier.as_deref_mut() {
            v.counter += 1;
            if v.counter >= v.every {
                v.counter = 0;
                verify_segment(
                    target,
                    resolved.primary,
                    in_chunk,
                    &buf[done as usize..(done + len) as usize],
                    v,
                )
                .await;
            }
        }
        done += len;
    }
    done as i32
}

/// Read the segment back and compare; mismatches are recorded, never returned to the kernel.
async fn verify_segment(
    target: &Target,
    pchunk: u64,
    in_chunk: u64,
    expected: &[u8],
    v: &mut Verifier,
) {
    let len = expected.len();
    let poff = target.layout.chunk_offset(pchunk) + in_chunk;
    let sqe = opcode::Read::new(
        types::Fixed(BACKING_FIXED_FD),
        v.scratch.as_mut_ptr(),
        len as u32,
    )
    .offset(poff)
    .build()
    .flags(squeue::Flags::FIXED_FILE);
    let res = submit_retrying(sqe).await;
    if res != len as i32 || &v.scratch.as_slice()[..len] != expected {
        tracing::warn!(
            pchunk,
            offset = poff,
            res,
            "read-after-write verification failed"
        );
        target.mapper.record_verify_failure(pchunk);
    }
}

async fn write_segment(
    target: &Target,
    pchunk: u64,
    in_chunk: u64,
    ptr: *const u8,
    len: u64,
) -> i32 {
    let poff = target.layout.chunk_offset(pchunk) + in_chunk;
    let sqe = opcode::Write::new(types::Fixed(BACKING_FIXED_FD), ptr, len as u32)
        .offset(poff)
        .build()
        .flags(squeue::Flags::FIXED_FILE);
    submit_retrying(sqe).await
}

async fn submit(sqe: squeue::Entry) -> i32 {
    ublk_submit_sqe_async(sqe, UblkUringData::Target as u64)
        .await
        .unwrap_or(-libc::EIO)
}

async fn submit_retrying(sqe: squeue::Entry) -> i32 {
    let mut res = -libc::EAGAIN;
    for _ in 0..MAX_EAGAIN_RETRIES {
        res = submit(sqe.clone()).await;
        if res != -libc::EAGAIN {
            return res;
        }
    }
    res
}
