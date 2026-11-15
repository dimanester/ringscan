//! I/O layer: the [`ReadAt`] trait and the aligned-read helpers every volume
//! access in this crate goes through.
//!
//! Raw Windows volume handles (`\\.\C:`) only accept reads whose offset and
//! length are multiples of the logical sector size, and with
//! `FILE_FLAG_NO_BUFFERING` the buffer address must be sector-aligned too. To
//! let the engine's `ReadAt` over such a handle stay a thin `ReadFile` wrapper,
//! the parser only ever issues reads that satisfy all of these at once:
//!
//! * the offset is a multiple of [`IO_ALIGN`] (4096, which is a multiple of
//!   every valid NTFS sector size: 256..=4096, powers of two);
//! * the length is a multiple of 4096, except for a read that ends exactly at
//!   the end of the NTFS volume (`total_sectors * bytes_per_sector`) when that
//!   end is not 4096-aligned: its length is still a multiple of
//!   `bytes_per_sector`. Nothing at or past that end is ever read;
//! * the buffer address is 4096-aligned ([`AlignedBuf`]);
//! * a single read is at most [`MAX_READ`] bytes.
//!
//! Unaligned spans (cluster sizes below 4096 can put $MFT runs at any
//! 512-byte boundary) are widened to the enclosing aligned range, read into a
//! bounce buffer and copied.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Offset/length/buffer alignment of every read issued by this crate.
pub const IO_ALIGN: u64 = 4096;

/// Upper bound of a single `read_at` call issued by this crate (the default
/// MFT chunk is 4 MiB; a caller-chosen chunk size is clamped to this).
pub const MAX_READ: usize = 16 << 20;

/// Size of the bounce buffer used for unaligned spans and for salvaging a
/// failed read block by block.
pub(crate) const BOUNCE: usize = 1 << 20;

/// Granularity of the salvage pass after a failed read.
const SALVAGE_BLOCK: u64 = 64 << 10;

/// Positional read of a volume or image. Must fill `buf` completely or fail.
///
/// Implementations must be safe to call from several threads: with
/// [`MftOptions::readers`](crate::MftOptions::readers) > 1 the MFT is read by
/// several threads at once (the default is one reader).
pub trait ReadAt: Sync {
    /// Reads exactly `buf.len()` bytes starting at byte offset `off`.
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()>;
}

impl ReadAt for std::fs::File {
    #[cfg(unix)]
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(self, buf, off)
    }

    #[cfg(windows)]
    fn read_at(&self, mut off: u64, mut buf: &mut [u8]) -> io::Result<()> {
        use std::os::windows::fs::FileExt;
        while !buf.is_empty() {
            match self.seek_read(buf, off) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    buf = &mut buf[n..];
                    off += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    fn read_at(&self, _off: u64, _buf: &mut [u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "positional reads unsupported",
        ))
    }
}

/// In-memory images (tests, replay benchmarks).
impl ReadAt for [u8] {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let start =
            usize::try_from(off).map_err(|_| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        let src = start
            .checked_add(buf.len())
            .and_then(|end| self.get(start..end))
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

impl ReadAt for Vec<u8> {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        self.as_slice().read_at(off, buf)
    }
}

impl<T: ReadAt + ?Sized> ReadAt for &T {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(off, buf)
    }
}

impl<T: ReadAt + ?Sized> ReadAt for Box<T> {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(off, buf)
    }
}

impl<T: ReadAt + ?Sized + Send> ReadAt for std::sync::Arc<T> {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        (**self).read_at(off, buf)
    }
}

/// A zero-initialised byte buffer whose start address is 4096-aligned.
pub(crate) struct AlignedBuf {
    raw: Vec<u8>,
    start: usize,
    len: usize,
}

impl AlignedBuf {
    pub(crate) fn new(len: usize) -> AlignedBuf {
        let align = IO_ALIGN as usize;
        let raw = vec![0u8; len + align];
        let addr = raw.as_ptr().addr();
        let start = (align - addr % align) % align;
        AlignedBuf { raw, start, len }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.raw[self.start..self.start + self.len]
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.raw[self.start..self.start + self.len]
    }
}

/// Read counters shared by the bootstrap code and the reader thread.
#[derive(Default)]
pub(crate) struct IoCounters {
    pub reads: AtomicU64,
    pub bytes: AtomicU64,
    pub max_read: AtomicU64,
    pub errors: AtomicU64,
    pub nanos: AtomicU64,
}

impl IoCounters {
    pub(crate) fn time(&self) -> Duration {
        Duration::from_nanos(self.nanos.load(Ordering::Relaxed))
    }
}

/// The volume plus what is needed to keep reads aligned and in bounds.
pub(crate) struct Disk<'a> {
    pub vol: &'a dyn ReadAt,
    /// First byte past the NTFS volume (`total_sectors * bytes_per_sector`).
    pub end: u64,
    pub io: &'a IoCounters,
}

pub(crate) fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn align_down(x: u64) -> u64 {
    x & !(IO_ALIGN - 1)
}

fn align_up(x: u64) -> Option<u64> {
    Some(x.checked_add(IO_ALIGN - 1)? & !(IO_ALIGN - 1))
}

impl Disk<'_> {
    /// One raw read; `off`, `buf.len()` and the buffer address must already
    /// be aligned as described in the module docs.
    fn raw_read(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        debug_assert!(off.is_multiple_of(IO_ALIGN));
        debug_assert!(
            (buf.len() as u64).is_multiple_of(IO_ALIGN) || off + buf.len() as u64 == self.end
        );
        debug_assert!(buf.as_ptr().addr().is_multiple_of(IO_ALIGN as usize));
        let t = Instant::now();
        let r = self.vol.read_at(off, buf);
        let ns = t.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        self.io.nanos.fetch_add(ns, Ordering::Relaxed);
        self.io.reads.fetch_add(1, Ordering::Relaxed);
        self.io.bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
        self.io
            .max_read
            .fetch_max(buf.len() as u64, Ordering::Relaxed);
        if r.is_err() {
            self.io.errors.fetch_add(1, Ordering::Relaxed);
        }
        r
    }

    /// Reads `dst.len()` bytes at volume offset `off` into `dst`, which starts
    /// `dst_align` bytes past a 4096-aligned address (callers pass the offset
    /// of `dst` inside an [`AlignedBuf`]). Aligned spans are read in place,
    /// anything else through `bounce`.
    pub(crate) fn read_span(
        &self,
        off: u64,
        dst: &mut [u8],
        dst_align: usize,
        bounce: &mut AlignedBuf,
    ) -> io::Result<()> {
        let len = dst.len() as u64;
        let end = off
            .checked_add(len)
            .ok_or_else(|| invalid("read offset overflow"))?;
        if end > self.end {
            return Err(invalid(format!(
                "read of {len} bytes at {off} crosses the end of the volume ({})",
                self.end
            )));
        }
        let aligned = off.is_multiple_of(IO_ALIGN)
            && len.is_multiple_of(IO_ALIGN)
            && dst_align.is_multiple_of(IO_ALIGN as usize);
        if aligned {
            let mut done = 0usize;
            while done < dst.len() {
                let n = (dst.len() - done).min(MAX_READ);
                self.raw_read(off + done as u64, &mut dst[done..done + n])?;
                done += n;
            }
            return Ok(());
        }
        // Bounce path: widen each sub-span to aligned bounds, read, copy.
        let mut pos = off;
        while pos < end {
            let a0 = align_down(pos);
            let want_end = end.min(a0 + bounce.len() as u64);
            let a1 = align_up(want_end)
                .ok_or_else(|| invalid("read offset overflow"))?
                .min(self.end);
            let n = (a1 - a0) as usize;
            let b = &mut bounce.as_mut_slice()[..n];
            self.raw_read(a0, b)?;
            let from = (pos - a0) as usize;
            let cnt = (want_end - pos) as usize;
            let at = (pos - off) as usize;
            dst[at..at + cnt].copy_from_slice(&b[from..from + cnt]);
            pos = want_end;
        }
        Ok(())
    }

    /// Like [`Disk::read_span`], but a failure does not abort: the span is
    /// re-read in 64 KiB aligned blocks, blocks that still fail are
    /// zero-filled (zeroed MFT records parse as "unused"). Returns the number
    /// of bytes that could not be read and the first error seen.
    pub(crate) fn read_span_salvage(
        &self,
        off: u64,
        dst: &mut [u8],
        dst_align: usize,
        bounce: &mut AlignedBuf,
    ) -> (u64, Option<io::Error>) {
        let first = match self.read_span(off, dst, dst_align, bounce) {
            Ok(()) => return (0, None),
            Err(e) => e,
        };
        let len = dst.len() as u64;
        let Some(end) = off.checked_add(len).filter(|&e| e <= self.end) else {
            dst.fill(0);
            return (len, Some(first));
        };
        let mut lost = 0u64;
        let mut pos = off;
        while pos < end {
            let a0 = align_down(pos);
            let block_end = (a0 + SALVAGE_BLOCK).min(end);
            let a1 = align_up(block_end).unwrap_or(u64::MAX).min(self.end);
            let n = (a1 - a0) as usize;
            let at = (pos - off) as usize;
            let cnt = (block_end - pos) as usize;
            let ok =
                n <= bounce.len() && self.raw_read(a0, &mut bounce.as_mut_slice()[..n]).is_ok();
            if ok {
                let from = (pos - a0) as usize;
                dst[at..at + cnt].copy_from_slice(&bounce.as_slice()[from..from + cnt]);
            } else {
                dst[at..at + cnt].fill(0);
                lost += cnt as u64;
            }
            pos = block_end;
        }
        (lost, Some(first))
    }

    /// Reads an arbitrary (possibly unaligned) span into a fresh vector.
    pub(crate) fn read_vec(&self, off: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut out = AlignedBuf::new(len);
        let mut bounce = AlignedBuf::new(BOUNCE);
        self.read_span(off, out.as_mut_slice(), 0, &mut bounce)?;
        Ok(out.as_slice().to_vec())
    }
}
