//! Mapping-pairs (runlist) decoding and VCN -> LCN mapping.
//!
//! A runlist is a sequence of `(header, length, lcn_delta)` tuples. The
//! header's low nibble is the byte count of the length, the high nibble the
//! byte count of the LCN delta (0 = sparse run, no clusters on disk). Both are
//! little-endian *signed*; deltas are relative to the previous non-sparse run
//! and restart from 0 in every attribute extent. A zero header ends the list.

use crate::io::{AlignedBuf, Disk, invalid};
use std::io;

/// One run of clusters: `len` clusters starting at `vcn` stored at `lcn`
/// (`None` for a sparse run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Run {
    pub vcn: u64,
    pub lcn: Option<u64>,
    pub len: u64,
}

/// Decodes the mapping pairs in `mp` (attribute extent starting at
/// `start_vcn`), appending to `out`. Returns the VCN after the last run.
/// Fails on truncation, zero/negative lengths, negative LCNs or overflow; on
/// failure `out` may hold the runs decoded so far.
pub(crate) fn decode(mp: &[u8], start_vcn: u64, out: &mut Vec<Run>) -> Result<u64, &'static str> {
    let mut pos = 0usize;
    let mut vcn = start_vcn;
    let mut lcn: i64 = 0;
    loop {
        let Some(&h) = mp.get(pos) else {
            return Err("runlist not terminated");
        };
        if h == 0 {
            return Ok(vcn);
        }
        let len_n = (h & 0x0F) as usize;
        let off_n = (h >> 4) as usize;
        if len_n == 0 || len_n > 8 || off_n > 8 {
            return Err("bad runlist header");
        }
        let len = read_signed(mp, pos + 1, len_n).ok_or("runlist truncated")?;
        if len <= 0 {
            return Err("non-positive run length");
        }
        pos += 1 + len_n;
        let run_lcn = if off_n == 0 {
            None
        } else {
            let delta = read_signed(mp, pos, off_n).ok_or("runlist truncated")?;
            pos += off_n;
            lcn = lcn.checked_add(delta).ok_or("LCN overflow")?;
            if lcn < 0 {
                return Err("negative LCN");
            }
            Some(lcn as u64)
        };
        out.push(Run {
            vcn,
            lcn: run_lcn,
            len: len as u64,
        });
        vcn = vcn.checked_add(len as u64).ok_or("VCN overflow")?;
    }
}

/// Little-endian signed integer of `n` (1..=8) bytes at `pos`.
fn read_signed(b: &[u8], pos: usize, n: usize) -> Option<i64> {
    let s = b.get(pos..pos.checked_add(n)?)?;
    let mut v: i64 = if s[n - 1] & 0x80 != 0 { -1 } else { 0 };
    for &byte in s.iter().rev() {
        v = (v << 8) | byte as i64;
    }
    Some(v)
}

/// A runlist covering VCNs `0..clusters()` without gaps, used to read
/// non-resident data ($MFT itself, a non-resident $ATTRIBUTE_LIST).
#[derive(Debug, Default, Clone)]
pub(crate) struct RunMap {
    pub runs: Vec<Run>,
    pub cluster: u64,
}

/// A contiguous piece of a byte range mapped to the volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Piece {
    /// Volume byte offset, `None` for sparse (zero) data.
    pub phys: Option<u64>,
    pub len: u64,
    /// Offset of the piece inside the requested range.
    pub dst: u64,
}

impl RunMap {
    /// Clusters mapped from VCN 0.
    pub(crate) fn clusters(&self) -> u64 {
        self.runs.last().map_or(0, |r| r.vcn + r.len)
    }

    /// Bytes mapped from VCN 0.
    pub(crate) fn bytes(&self) -> u64 {
        self.clusters().saturating_mul(self.cluster)
    }

    /// Appends runs that must continue exactly at `clusters()`; runs must lie
    /// inside the volume (`total_clusters`).
    pub(crate) fn extend(&mut self, runs: &[Run], total_clusters: u64) -> io::Result<()> {
        for r in runs {
            if r.vcn != self.clusters() {
                return Err(invalid(format!("runlist gap or overlap at VCN {}", r.vcn)));
            }
            if let Some(l) = r.lcn
                && l.checked_add(r.len).is_none_or(|e| e > total_clusters)
            {
                return Err(invalid(format!(
                    "run at LCN {l} (+{}) is outside the volume",
                    r.len
                )));
            }
            if r.vcn
                .checked_add(r.len)
                .and_then(|e| e.checked_mul(self.cluster))
                .is_none()
            {
                return Err(invalid("runlist too long"));
            }
            match self.runs.last_mut() {
                // Merge physically contiguous neighbours (extent boundaries).
                Some(last) if matches!((last.lcn, r.lcn), (Some(a), Some(b)) if a + last.len == b) => {
                    last.len += r.len
                }
                Some(last) if last.lcn.is_none() && r.lcn.is_none() => last.len += r.len,
                _ => self.runs.push(*r),
            }
        }
        Ok(())
    }

    /// Calls `f` for each piece of the byte range `start..end` (which must be
    /// inside `bytes()`), in order.
    pub(crate) fn pieces(&self, start: u64, end: u64, mut f: impl FnMut(Piece)) {
        let cs = self.cluster;
        let first_vcn = start / cs;
        let mut i = self.runs.partition_point(|r| r.vcn + r.len <= first_vcn);
        let mut pos = start;
        while pos < end {
            let Some(r) = self.runs.get(i) else { return };
            let run_start = r.vcn * cs;
            let run_end = (r.vcn + r.len) * cs;
            let piece_end = run_end.min(end);
            let phys = r.lcn.map(|l| l * cs + (pos - run_start));
            f(Piece {
                phys,
                len: piece_end - pos,
                dst: pos - start,
            });
            pos = piece_end;
            i += 1;
        }
    }

    /// Reads the byte range `start..start + dst.len()` into `dst` (which
    /// starts at a 4096-aligned address). Sparse pieces are zero-filled.
    pub(crate) fn read(
        &self,
        disk: &Disk,
        start: u64,
        dst: &mut [u8],
        bounce: &mut AlignedBuf,
    ) -> io::Result<()> {
        let end = start + dst.len() as u64;
        if end > self.bytes() {
            return Err(invalid("read beyond the mapped runlist"));
        }
        let mut res = Ok(());
        self.pieces(start, end, |p| {
            if res.is_err() {
                return;
            }
            let d = &mut dst[p.dst as usize..(p.dst + p.len) as usize];
            match p.phys {
                None => d.fill(0),
                Some(off) => res = disk.read_span(off, d, p.dst as usize, bounce),
            }
        });
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(mp: &[u8]) -> Result<Vec<Run>, &'static str> {
        let mut v = Vec::new();
        decode(mp, 0, &mut v).map(|_| v)
    }

    #[test]
    fn decodes_runs() {
        // 0x21: 1-byte length, 2-byte offset. 0x18 clusters at 0x5634,
        // then 0x10 clusters at delta -0x34 (1 byte, 0xCC), then sparse 4.
        let mp = [0x21, 0x18, 0x34, 0x56, 0x11, 0x10, 0xCC, 0x01, 0x04, 0x00];
        let runs = dec(&mp).unwrap();
        assert_eq!(
            runs,
            vec![
                Run {
                    vcn: 0,
                    lcn: Some(0x5634),
                    len: 0x18
                },
                Run {
                    vcn: 0x18,
                    lcn: Some(0x5600),
                    len: 0x10
                },
                Run {
                    vcn: 0x28,
                    lcn: None,
                    len: 4
                },
            ]
        );
        // Start VCN of a later extent.
        let mut v = Vec::new();
        assert_eq!(decode(&[0x11, 0x05, 0x10, 0x00], 100, &mut v), Ok(105));
        assert_eq!(
            v[0],
            Run {
                vcn: 100,
                lcn: Some(0x10),
                len: 5
            }
        );
    }

    #[test]
    fn delta_after_sparse_is_relative_to_last_real_run() {
        let mp = [0x11, 0x02, 0x20, 0x01, 0x03, 0x11, 0x02, 0x10, 0x00];
        let runs = dec(&mp).unwrap();
        assert_eq!(
            runs[2],
            Run {
                vcn: 5,
                lcn: Some(0x30),
                len: 2
            }
        );
    }

    #[test]
    fn rejects_malformed() {
        assert!(dec(&[]).is_err()); // no terminator
        assert!(dec(&[0x11, 0x05]).is_err()); // truncated
        assert!(dec(&[0x10, 0x05, 0x00]).is_err()); // zero length nibble
        assert!(dec(&[0x11, 0x00, 0x05, 0x00]).is_err()); // zero length
        assert!(dec(&[0x11, 0xFF, 0x05, 0x00]).is_err()); // negative length
        assert!(dec(&[0x11, 0x01, 0xFF, 0x00]).is_err()); // negative LCN
        assert!(dec(&[0x19, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0x00]).is_err()); // 9-byte length
        // Three huge sparse runs overflow the VCN.
        let huge = [0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F];
        let mp: Vec<u8> = huge
            .iter()
            .chain(&huge)
            .chain(&huge)
            .copied()
            .chain([0])
            .collect();
        assert!(dec(&mp).is_err());
    }

    #[test]
    fn runmap_pieces_and_merge() {
        let mut m = RunMap {
            runs: Vec::new(),
            cluster: 512,
        };
        m.extend(
            &[
                Run {
                    vcn: 0,
                    lcn: Some(10),
                    len: 3,
                },
                Run {
                    vcn: 3,
                    lcn: Some(13),
                    len: 2,
                },
            ],
            1000,
        )
        .unwrap();
        assert_eq!(m.runs.len(), 1, "contiguous runs merge");
        m.extend(
            &[
                Run {
                    vcn: 5,
                    lcn: Some(100),
                    len: 4,
                },
                Run {
                    vcn: 9,
                    lcn: None,
                    len: 1,
                },
            ],
            1000,
        )
        .unwrap();
        assert!(
            m.extend(
                &[Run {
                    vcn: 11,
                    lcn: Some(1),
                    len: 1
                }],
                1000
            )
            .is_err(),
            "gap"
        );
        assert!(
            m.extend(
                &[Run {
                    vcn: 10,
                    lcn: Some(999),
                    len: 2
                }],
                1000
            )
            .is_err(),
            "outside volume"
        );
        let mut got = Vec::new();
        m.pieces(1024, 5 * 512 + 700, |p| got.push(p));
        assert_eq!(
            got,
            vec![
                Piece {
                    phys: Some(12 * 512),
                    len: 1536,
                    dst: 0
                },
                Piece {
                    phys: Some(100 * 512),
                    len: 700,
                    dst: 1536
                },
            ]
        );
        got.clear();
        m.pieces(9 * 512, 10 * 512, |p| got.push(p));
        assert_eq!(
            got,
            vec![Piece {
                phys: None,
                len: 512,
                dst: 0
            }]
        );
    }
}
