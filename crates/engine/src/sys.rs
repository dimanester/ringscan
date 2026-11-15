//! Process memory figures (for the CLI report and benchmarks).

/// Peak memory of the current process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryInfo {
    /// Peak working set (Windows) / peak resident set `VmHWM` (Linux), bytes.
    pub peak_resident: u64,
    /// Peak private committed bytes (Windows `PeakPagefileUsage`); 0 where
    /// unknown.
    pub peak_private: u64,
}

/// Peak memory of this process, if the platform reports it.
pub fn memory_info() -> Option<MemoryInfo> {
    imp::memory_info()
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub fn memory_info() -> Option<super::MemoryInfo> {
        // SAFETY: plain-data struct; zero is a valid bit pattern.
        let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        c.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: the pseudo handle of the current process and a correctly
        // sized out-struct.
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        (ok != 0).then_some(super::MemoryInfo {
            peak_resident: c.PeakWorkingSetSize as u64,
            peak_private: c.PeakPagefileUsage as u64,
        })
    }
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn memory_info() -> Option<super::MemoryInfo> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let kib = status.lines().find_map(|l| l.strip_prefix("VmHWM:"))?;
        let kib: u64 = kib.trim().trim_end_matches("kB").trim().parse().ok()?;
        Some(super::MemoryInfo {
            peak_resident: kib * 1024,
            peak_private: 0,
        })
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub fn memory_info() -> Option<super::MemoryInfo> {
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reports_some_peak_memory() {
        if cfg!(any(windows, target_os = "linux")) {
            let m = super::memory_info().expect("memory info");
            assert!(m.peak_resident > 1 << 20, "{m:?}");
        }
    }
}
