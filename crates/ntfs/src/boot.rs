//! NTFS boot sector (BIOS parameter block) parsing and validation.

use crate::io::invalid;
use crate::le;
use std::io;

/// Geometry of an NTFS volume, decoded from its boot sector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VolumeInfo {
    /// Logical sector size: 256..=4096, a power of two.
    pub bytes_per_sector: u32,
    /// Cluster size: a power of two, at most 2 MiB.
    pub bytes_per_cluster: u32,
    /// MFT record size: a power of two, 512..=65536 (1024 or 4096 in practice).
    pub record_size: u32,
    /// First cluster of `$MFT`.
    pub mft_lcn: u64,
    /// First cluster of `$MFTMirr` (copy of records 0..=3).
    pub mft_mirr_lcn: u64,
    /// Clusters in the volume (`total_sectors / sectors_per_cluster`).
    pub total_clusters: u64,
    /// Sectors in the volume as stored in the boot sector. The NTFS volume
    /// ends at `total_sectors * bytes_per_sector`; nothing past it is read.
    pub total_sectors: u64,
    /// Volume serial number.
    pub serial: u64,
}

impl VolumeInfo {
    /// Byte size of the NTFS volume (`total_sectors * bytes_per_sector`).
    pub fn volume_size(&self) -> u64 {
        self.total_sectors
            .saturating_mul(self.bytes_per_sector as u64)
    }
}

/// Largest cluster size Windows supports (since Windows 10 1709).
const MAX_CLUSTER: u64 = 2 << 20;

/// Decodes and validates the boot sector in `b` (at least 512 bytes).
pub(crate) fn parse_boot_sector(b: &[u8]) -> io::Result<VolumeInfo> {
    if b.len() < 512 {
        return Err(invalid("boot sector shorter than 512 bytes"));
    }
    if b.get(3..11) != Some(b"NTFS    ".as_slice()) {
        return Err(invalid(
            "not an NTFS volume (boot sector OEM id is not \"NTFS    \")",
        ));
    }
    let bps = le::u16_at(b, 0x0B).unwrap_or(0) as u64;
    if !(256..=4096).contains(&bps) || !bps.is_power_of_two() {
        return Err(invalid(format!("invalid bytes per sector: {bps}")));
    }
    // Sectors per cluster: 1..=128 directly; larger clusters (Windows 10+)
    // store a negative exponent: 0xF4 = -12 -> 2^12 sectors.
    let spc_raw = b[0x0D];
    let spc: u64 = match spc_raw {
        1..=0x80 if spc_raw.is_power_of_two() => spc_raw as u64,
        0xF0..=0xFF => 1u64 << (256 - spc_raw as u32),
        _ => {
            return Err(invalid(format!(
                "invalid sectors per cluster: {spc_raw:#x}"
            )));
        }
    };
    let cluster = bps * spc;
    if cluster > MAX_CLUSTER {
        return Err(invalid(format!(
            "cluster size {cluster} is larger than 2 MiB"
        )));
    }
    let total_sectors = le::u64_at(b, 0x28).unwrap_or(0);
    if total_sectors == 0 || total_sectors.checked_mul(bps).is_none() {
        return Err(invalid(format!("invalid total sectors: {total_sectors}")));
    }
    let total_clusters = total_sectors / spc;
    let mft_lcn = le::u64_at(b, 0x30).unwrap_or(u64::MAX);
    if mft_lcn >= total_clusters {
        return Err(invalid(format!(
            "$MFT cluster {mft_lcn} is outside the volume ({total_clusters} clusters)"
        )));
    }
    let mft_mirr_lcn = le::u64_at(b, 0x38).unwrap_or(0);
    let record_size = decode_record_size(b[0x40] as i8, cluster)
        .ok_or_else(|| invalid(format!("invalid clusters per MFT record: {:#x}", b[0x40])))?;
    Ok(VolumeInfo {
        bytes_per_sector: bps as u32,
        bytes_per_cluster: cluster as u32,
        record_size: record_size as u32,
        mft_lcn,
        mft_mirr_lcn,
        total_clusters,
        total_sectors,
        serial: le::u64_at(b, 0x48).unwrap_or(0),
    })
}

/// "Clusters per MFT record": positive = that many clusters, negative =
/// `2^-n` bytes (used whenever a record is smaller than a cluster).
fn decode_record_size(v: i8, cluster: u64) -> Option<u64> {
    let size = match v {
        1..=127 => cluster.checked_mul(v as u64)?,
        -16..=-9 => 1u64 << -(v as i32),
        _ => return None,
    };
    (size.is_power_of_two() && (512..=65536).contains(&size)).then_some(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot(bps: u16, spc: u8, total: u64, mft_lcn: u64, cpr: i8) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[0x0B..0x0D].copy_from_slice(&bps.to_le_bytes());
        b[0x0D] = spc;
        b[0x28..0x30].copy_from_slice(&total.to_le_bytes());
        b[0x30..0x38].copy_from_slice(&mft_lcn.to_le_bytes());
        b[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
        b[0x40] = cpr as u8;
        b[0x48..0x50].copy_from_slice(&0x1234_5678_9abc_def0u64.to_le_bytes());
        b[0x1FE] = 0x55;
        b[0x1FF] = 0xAA;
        b
    }

    #[test]
    fn typical_volume() {
        let v = parse_boot_sector(&boot(512, 8, 1_000_000, 4, -10)).unwrap();
        assert_eq!(v.bytes_per_sector, 512);
        assert_eq!(v.bytes_per_cluster, 4096);
        assert_eq!(v.record_size, 1024);
        assert_eq!(v.total_clusters, 125_000);
        assert_eq!(v.mft_lcn, 4);
        assert_eq!(v.serial, 0x1234_5678_9abc_def0);
        assert_eq!(v.volume_size(), 512_000_000);
    }

    #[test]
    fn large_clusters_use_negative_exponent() {
        // 0xF4 = -12: 4096 sectors of 512 bytes = 2 MiB clusters.
        let v = parse_boot_sector(&boot(512, 0xF4, 1 << 30, 3, -10)).unwrap();
        assert_eq!(v.bytes_per_cluster, 2 << 20);
        assert_eq!(v.total_clusters, (1 << 30) / 4096);
        // 0xF7 = -9: 512 sectors of 512 bytes = 256 KiB.
        assert_eq!(
            parse_boot_sector(&boot(512, 0xF7, 1 << 30, 3, -10))
                .unwrap()
                .bytes_per_cluster,
            256 << 10
        );
        // 4096-byte sectors, 0xF8 = -8: 256 * 4096 = 1 MiB.
        assert_eq!(
            parse_boot_sector(&boot(4096, 0xF8, 1 << 24, 3, -12))
                .unwrap()
                .bytes_per_cluster,
            1 << 20
        );
        // Larger than 2 MiB is rejected.
        assert!(parse_boot_sector(&boot(4096, 0xF4, 1 << 30, 3, -12)).is_err());
    }

    #[test]
    fn record_size_encodings() {
        // Positive: clusters per record (512-byte clusters, 2 per record).
        let v = parse_boot_sector(&boot(512, 1, 100_000, 4, 2)).unwrap();
        assert_eq!(v.record_size, 1024);
        // 4096-byte records on a 4Kn volume.
        assert_eq!(
            parse_boot_sector(&boot(4096, 1, 100_000, 4, -12))
                .unwrap()
                .record_size,
            4096
        );
        // 64 KiB clusters with 1 KiB records.
        assert_eq!(
            parse_boot_sector(&boot(512, 128, 1 << 22, 4, -10))
                .unwrap()
                .record_size,
            1024
        );
        // Not a power of two / out of range.
        assert!(parse_boot_sector(&boot(512, 1, 100_000, 4, 3)).is_err());
        assert!(parse_boot_sector(&boot(512, 1, 100_000, 4, 0)).is_err());
        assert!(parse_boot_sector(&boot(512, 1, 100_000, 4, -8)).is_err());
        assert!(parse_boot_sector(&boot(512, 128, 1 << 22, 4, 4)).is_err()); // 256 KiB
    }

    #[test]
    fn rejects_garbage() {
        let mut b = boot(512, 8, 1_000_000, 4, -10);
        b[3] = b'X';
        assert!(parse_boot_sector(&b).is_err());
        for bps in [0u16, 128, 513, 8192] {
            assert!(
                parse_boot_sector(&boot(bps, 8, 1_000_000, 4, -10)).is_err(),
                "bps {bps}"
            );
        }
        for spc in [0u8, 3, 0x81, 0xEF] {
            assert!(
                parse_boot_sector(&boot(512, spc, 1_000_000, 4, -10)).is_err(),
                "spc {spc}"
            );
        }
        assert!(parse_boot_sector(&boot(512, 8, 0, 4, -10)).is_err());
        assert!(parse_boot_sector(&boot(512, 8, u64::MAX, 4, -10)).is_err());
        assert!(parse_boot_sector(&boot(512, 8, 1_000_000, 125_000, -10)).is_err());
        assert!(parse_boot_sector(&b[..100]).is_err());
    }
}
