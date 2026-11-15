//! Windows integration tests for the public API of ringscan-win.
//!
//! A fixture of known files is built under
//! `%TEMP%\ringscan-dev\win-tests-<pid>\<test>` (compressed and sparse
//! files, a junction, Cyrillic/emoji names, a name with an unpaired
//! surrogate, a name ending in a dot, a >300-char path, hidden+system and
//! read-only files) and what `list_dir` reports is compared with independent
//! sources: per-file handles (`FILE_STANDARD_INFO`, file index),
//! `GetCompressedFileSizeW`, std metadata and PowerShell's `Get-ChildItem`.
//! Destructive calls only ever touch files created here.
//!
//! Run: `cargo test -p ringscan-win --target x86_64-pc-windows-gnu -- --nocapture`
#![cfg(windows)]

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, UNIX_EPOCH};

use ringscan_win::attrs::*;
use ringscan_win::{DriveKind, WinEntry, drives, list_dir, shell};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_STANDARD_INFO, FileStandardInfo, GetCompressedFileSizeW,
    GetFileInformationByHandle, GetFileInformationByHandleEx, SetFileAttributesW,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FILE_ZERO_DATA_INFORMATION, FSCTL_SET_SPARSE, FSCTL_SET_ZERO_DATA,
};

// --- fixture helpers ---------------------------------------------------------------

/// A fresh scratch dir for one test; removed (with the crate's own
/// `delete_permanently`) when dropped.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let dir = std::env::temp_dir()
            .join("ringscan-dev")
            .join(format!("win-tests-{}", std::process::id()))
            .join(name);
        if dir.exists() {
            shell::delete_permanently(std::slice::from_ref(&dir)).unwrap();
        }
        fs::create_dir_all(&dir).unwrap();
        Fixture { dir }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(e) = shell::delete_permanently(std::slice::from_ref(&self.dir)) {
            eprintln!("cleanup of {} failed: {e}", self.dir.display());
        }
        // The per-process parent goes away with the last fixture.
        let _ = fs::remove_dir(self.dir.parent().unwrap());
    }
}

fn verbatim(p: &Path) -> PathBuf {
    let mut s = OsString::from(r"\\?\");
    s.push(p.as_os_str());
    s.into()
}

fn wide_z(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Attribute-only handle (works for files, directories and links).
fn open_attrs(p: &Path) -> fs::File {
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(7)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(p)
        .unwrap_or_else(|e| panic!("open {}: {e}", p.display()))
}

/// (AllocationSize, EndOfFile) from FILE_STANDARD_INFO.
fn standard_info(p: &Path) -> (u64, u64) {
    let f = open_attrs(p);
    // SAFETY: plain data out-parameter of the size passed.
    unsafe {
        let mut info: FILE_STANDARD_INFO = std::mem::zeroed();
        let ok = GetFileInformationByHandleEx(
            f.as_raw_handle(),
            FileStandardInfo,
            (&raw mut info).cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        );
        assert_ne!(ok, 0, "FileStandardInfo {}", p.display());
        (info.AllocationSize as u64, info.EndOfFile as u64)
    }
}

/// The NTFS file index (what FILE_ID_BOTH_DIR_INFO calls FileId).
fn file_index(p: &Path) -> u64 {
    let f = open_attrs(p);
    // SAFETY: plain data out-parameter.
    unsafe {
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        assert_ne!(GetFileInformationByHandle(f.as_raw_handle(), &mut info), 0);
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)
    }
}

/// Bytes actually used on disk for compressed/sparse files (else = size).
fn compressed_size(p: &Path) -> u64 {
    let w = wide_z(p);
    let mut high = 0u32;
    // SAFETY: NUL-terminated path, valid out-pointer.
    let low = unsafe { GetCompressedFileSizeW(w.as_ptr(), &mut high) };
    assert_ne!(low, u32::MAX, "GetCompressedFileSizeW {}", p.display());
    (u64::from(high) << 32) | u64::from(low)
}

fn set_attributes(p: &Path, attrs: u32) {
    let w = wide_z(p);
    // SAFETY: NUL-terminated path.
    assert_ne!(
        unsafe { SetFileAttributesW(w.as_ptr(), attrs) },
        0,
        "SetFileAttributesW {}",
        p.display()
    );
}

/// A sparse file of `logical` bytes with one 64 KiB data block at
/// `logical / 2`; a second block written at 0 is punched out again with
/// FSCTL_SET_ZERO_DATA.
fn make_sparse(p: &Path, logical: u64) {
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(p)
        .unwrap();
    let mut ret = 0u32;
    // SAFETY: no buffers.
    let ok = unsafe {
        DeviceIoControl(
            f.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(ok, 0, "FSCTL_SET_SPARSE: {}", io::Error::last_os_error());
    f.set_len(logical).unwrap();
    let block = vec![0xA5u8; 64 << 10];
    f.seek(SeekFrom::Start(logical / 2)).unwrap();
    f.write_all(&block).unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all(&block).unwrap();
    f.sync_all().unwrap();
    let zero = FILE_ZERO_DATA_INFORMATION {
        FileOffset: 0,
        BeyondFinalZero: 64 << 10,
    };
    // SAFETY: input buffer is `zero`, sized correctly.
    let ok = unsafe {
        DeviceIoControl(
            f.as_raw_handle(),
            FSCTL_SET_ZERO_DATA,
            (&raw const zero).cast(),
            size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
            std::ptr::null_mut(),
            0,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(ok, 0, "FSCTL_SET_ZERO_DATA: {}", io::Error::last_os_error());
}

fn run(cmd: &str, args: &[&OsStr]) -> std::process::Output {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("{cmd}: {e}"));
    assert!(
        out.status.success(),
        "{cmd} {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn mklink_junction(link: &Path, target: &Path) {
    run(
        "cmd.exe",
        &[
            "/c".as_ref(),
            "mklink".as_ref(),
            "/J".as_ref(),
            link.as_os_str(),
            target.as_os_str(),
        ],
    );
}

/// Base64 for `powershell -EncodedCommand` (avoids all quoting issues).
fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                s.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// Runs a PowerShell script, returns stdout (UTF-8).
fn powershell(script: &str) -> String {
    let script = format!("[Console]::OutputEncoding = [Text.Encoding]::UTF8\n{script}");
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let enc = base64(&utf16);
    let out = run(
        "powershell.exe",
        &[
            "-NoProfile".as_ref(),
            "-NonInteractive".as_ref(),
            "-EncodedCommand".as_ref(),
            enc.as_ref(),
        ],
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn ps_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "''"))
}

/// Get-ChildItem -Force: name -> (length or -1 for dirs, attributes, FILETIME).
fn powershell_listing(dir: &Path) -> HashMap<String, (i64, u32, i64)> {
    let script = format!(
        "Get-ChildItem -Force -LiteralPath {} | ForEach-Object {{ \
           $len = if ($_.PSIsContainer) {{ -1 }} else {{ $_.Length }}; \
           @($_.Name, $len, [int]$_.Attributes, $_.LastWriteTimeUtc.ToFileTimeUtc()) -join [char]9 }}",
        ps_quote(dir)
    );
    powershell(&script)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let f: Vec<&str> = l.trim_end_matches('\r').split('\t').collect();
            (
                f[0].to_owned(),
                (
                    f[1].parse().unwrap(),
                    f[2].parse::<i64>().unwrap() as u32,
                    f[3].parse().unwrap(),
                ),
            )
        })
        .collect()
}

fn listing(dir: &Path) -> HashMap<String, WinEntry> {
    let mut out = vec![];
    list_dir(dir, &mut out).unwrap_or_else(|e| panic!("list_dir {}: {e}", dir.display()));
    let n = out.len();
    let map: HashMap<String, WinEntry> = out.into_iter().map(|e| (e.name.clone(), e)).collect();
    assert_eq!(map.len(), n, "duplicate names in {}", dir.display());
    map
}

fn unix_secs(p: &Path) -> i64 {
    let m = fs::symlink_metadata(p).unwrap().modified().unwrap();
    m.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

// --- tests -------------------------------------------------------------------------------

#[test]
fn list_dir_reports_sizes_attributes_and_names() {
    let fx = Fixture::new("list");
    let d = &fx.dir;
    for (name, size) in [
        ("size-0.bin", 0u64),
        ("size-1.bin", 1),
        ("size-4095.bin", 4095),
        ("size-4096.bin", 4096),
        ("size-1MiB.bin", MIB),
    ] {
        let data: Vec<u8> = (0..size).map(|i| (i * 7919 % 251) as u8).collect();
        fs::write(d.join(name), data).unwrap();
    }
    let text = "RingScan compresses well. ".repeat(40_330);
    fs::write(d.join("compressed.txt"), &text.as_bytes()[..MIB as usize]).unwrap();
    run(
        "compact.exe",
        &[
            "/c".as_ref(),
            "/q".as_ref(),
            d.join("compressed.txt").as_os_str(),
        ],
    );
    make_sparse(&d.join("sparse.bin"), GIB);
    fs::write(d.join("hidden-system.txt"), b"hs").unwrap();
    set_attributes(
        &d.join("hidden-system.txt"),
        FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM,
    );
    fs::write(d.join("readonly.txt"), b"ro").unwrap();
    set_attributes(&d.join("readonly.txt"), FILE_ATTRIBUTE_READONLY);
    let unicode = ["Привіт, світ.txt", "emoji 😀🎉.txt", "日本語のファイル.txt"];
    for name in unicode {
        fs::write(d.join(name), name.as_bytes()).unwrap();
    }
    fs::create_dir(d.join("folder")).unwrap();
    fs::write(d.join("folder").join("inner.txt"), b"inner").unwrap();
    mklink_junction(&d.join("junction"), &d.join("folder"));
    let lone: Vec<u16> = "lone-"
        .encode_utf16()
        .chain([0xDC00])
        .chain(".txt".encode_utf16())
        .collect();
    fs::write(d.join(OsString::from_wide(&lone)), b"lone").unwrap();
    let dot_dir = verbatim(&d.join("trailing-dot."));
    fs::create_dir(&dot_dir).unwrap();
    fs::write(dot_dir.join("x.txt"), b"x").unwrap();
    let deep = d
        .join("long")
        .join("a".repeat(120))
        .join("b".repeat(120))
        .join("c".repeat(120));
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("deep.txt"), b"deep").unwrap();
    assert!(deep.as_os_str().len() > 300);

    let t = Instant::now();
    let got = listing(d);
    eprintln!("listed {} entries in {:?}", got.len(), t.elapsed());

    let mut expected: Vec<String> = [
        "size-0.bin",
        "size-1.bin",
        "size-4095.bin",
        "size-4096.bin",
        "size-1MiB.bin",
        "compressed.txt",
        "sparse.bin",
        "hidden-system.txt",
        "readonly.txt",
        "folder",
        "junction",
        "lone-\u{FFFD}.txt",
        "trailing-dot.",
        "long",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain(unicode.iter().map(|s| s.to_string()))
    .collect();
    expected.sort();
    let mut names: Vec<String> = got.keys().cloned().collect();
    names.sort();
    assert_eq!(names, expected);

    eprintln!(
        "{:<26} {:>12} {:>10} {:>10} {:>8} {:>10} {:>10}",
        "name", "logical", "alloc", "std.alloc", "attrs", "tag", "on-disk"
    );
    for e in got.values() {
        // Verbatim, so that `trailing-dot.` is not normalized away.
        let path = verbatim(&e.path_in(d));
        let (std_alloc, std_eof) = standard_info(&path);
        let on_disk = if e.is_dir { 0 } else { compressed_size(&path) };
        eprintln!(
            "{:<26} {:>12} {:>10} {:>10} {:>8x} {:>10x} {:>10}",
            e.name, e.logical, e.alloc, std_alloc, e.attrs, e.reparse_tag, on_disk
        );
        assert_eq!(
            e.is_dir,
            path.symlink_metadata().unwrap().file_type().is_dir() || e.is_link(),
            "{}",
            e.name
        );
        assert_eq!(e.file_id, file_index(&path), "file id of {}", e.name);
        assert_eq!(e.mtime, unix_secs(&path), "mtime of {}", e.name);
        if !e.is_dir {
            assert_eq!(e.logical, std_eof, "logical size of {}", e.name);
            assert_eq!(e.alloc, std_alloc, "allocation of {}", e.name);
        }
    }

    // Plain sizes.
    for (name, size) in [
        ("size-0.bin", 0u64),
        ("size-1.bin", 1),
        ("size-4095.bin", 4095),
        ("size-4096.bin", 4096),
        ("size-1MiB.bin", MIB),
    ] {
        let e = &got[name];
        assert_eq!(e.logical, size, "{name}");
        assert!(!e.is_dir && e.raw_name.is_none() && e.reparse_tag == 0);
        if size >= 4096 {
            assert_eq!(e.alloc, size.div_ceil(4096) * 4096, "{name}");
        }
    }
    assert_eq!(got["size-0.bin"].alloc, 0);

    // Compressed: allocation well below the logical size, = on-disk size.
    let c = &got["compressed.txt"];
    assert!(c.attrs & FILE_ATTRIBUTE_COMPRESSED != 0);
    assert_eq!(c.logical, MIB);
    assert!(c.alloc < c.logical / 4, "compressed alloc {}", c.alloc);
    assert_eq!(c.alloc, compressed_size(&d.join("compressed.txt")));

    // Sparse: 1 GiB logical, one 64 KiB block allocated.
    let s = &got["sparse.bin"];
    assert!(s.attrs & FILE_ATTRIBUTE_SPARSE_FILE != 0);
    assert_eq!(s.logical, GIB);
    assert_eq!(s.alloc, 64 << 10);
    assert_eq!(s.alloc, compressed_size(&d.join("sparse.bin")));

    // Attributes.
    let hs = &got["hidden-system.txt"];
    assert_eq!(
        hs.attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM),
        FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM
    );
    assert!(got["readonly.txt"].attrs & FILE_ATTRIBUTE_READONLY != 0);

    // Junction: listed as a directory link with the mount-point tag.
    let j = &got["junction"];
    assert!(j.is_dir && j.is_reparse_point() && j.is_link());
    assert_eq!(j.reparse_tag, IO_REPARSE_TAG_MOUNT_POINT);
    assert!(!got["folder"].is_link() && got["folder"].reparse_tag == 0);

    // Names: exact for Unicode, lossy + exact raw for the lone surrogate.
    for name in unicode {
        assert!(got[name].raw_name.is_none(), "{name}");
    }
    let l = &got["lone-\u{FFFD}.txt"];
    assert_eq!(l.raw_name.as_deref(), Some(lone.as_slice()));
    assert_eq!(l.os_name(), OsString::from_wide(&lone));
    assert_eq!(fs::read(l.path_in(d)).unwrap(), b"lone");

    // A name ending in a dot is reachable through path_in (verbatim inside).
    let td = &got["trailing-dot."];
    assert!(td.is_dir);
    assert!(listing(&td.path_in(d)).contains_key("x.txt"));

    // Long path (> 300 chars) given in plain form.
    let deep_list = listing(&deep);
    assert_eq!(deep_list["deep.txt"].logical, 4);

    // Following the junction lists the target.
    assert!(listing(&d.join("junction")).contains_key("inner.txt"));

    // Independent source: PowerShell Get-ChildItem -Force.
    let ps = powershell_listing(d);
    assert_eq!(
        ps.len(),
        got.len(),
        "PowerShell sees {:?}",
        ps.keys().collect::<Vec<_>>()
    );
    for (name, (len, attrs, ft)) in &ps {
        let Some(e) = got.get(name) else {
            assert!(
                name.starts_with("lone-"),
                "PowerShell entry {name:?} not listed"
            );
            continue;
        };
        if *len >= 0 {
            assert_eq!(e.logical, *len as u64, "Get-ChildItem Length of {name}");
        }
        assert_eq!(e.attrs, *attrs, "Get-ChildItem Attributes of {name}");
        assert_eq!(
            e.mtime,
            ft / 10_000_000 - 11_644_473_600,
            "Get-ChildItem LastWriteTimeUtc of {name}"
        );
    }
}

#[test]
fn list_dir_paths_errors_and_big_directories() {
    let fx = Fixture::new("paths");
    let d = &fx.dir;
    fs::create_dir(d.join("sub")).unwrap();
    fs::write(d.join("sub").join("f.txt"), b"f").unwrap();
    fs::write(d.join("file.txt"), b"f").unwrap();

    // Forward slashes, `..` and verbatim input all reach the same directory.
    let fwd = PathBuf::from(format!("{}/sub", d.display()).replace('\\', "/"));
    let dotdot = d.join("sub").join("..").join("sub");
    for p in [fwd, dotdot, verbatim(&d.join("sub"))] {
        assert!(listing(&p).contains_key("f.txt"), "{}", p.display());
    }

    // Errors leave `out` untouched.
    let sentinel = listing(d).remove("file.txt").unwrap();
    let mut out = vec![sentinel.clone()];
    let e = list_dir(&d.join("missing"), &mut out).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
    let e = list_dir(&d.join("file.txt"), &mut out).unwrap_err();
    eprintln!("list_dir(file) -> {:?} {e}", e.kind());
    let e = list_dir(Path::new(r"C:\System Volume Information"), &mut out).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
    assert_eq!(out, vec![sentinel]);

    // Many buffer refills: compare with std::fs::read_dir (FindFirstFileExW).
    for dir in [r"C:\Windows\WinSxS", r"C:\Windows\System32"] {
        let t = Instant::now();
        let ours = listing(Path::new(dir));
        let t_ours = t.elapsed();
        let t = Instant::now();
        let std_names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let t_std = t.elapsed();
        eprintln!(
            "{dir}: {} entries, list_dir {t_ours:?}, std read_dir {t_std:?}",
            ours.len()
        );
        assert_eq!(ours.len(), std_names.len(), "{dir}");
        assert!(std_names.iter().all(|n| ours.contains_key(n)), "{dir}");
    }

    // A UNC path (\\wsl.localhost\...) when provided by the test runner.
    if let Some(unc) = std::env::var_os("RINGSCAN_TEST_UNC_DIR") {
        let unc = PathBuf::from(unc).join(format!("list-{}", std::process::id()));
        fs::create_dir_all(unc.join("sub")).unwrap();
        for (name, size) in [("a.bin", 0usize), ("b.bin", 5000), ("Привіт.txt", 3)] {
            fs::write(unc.join(name), vec![1u8; size]).unwrap();
        }
        let ours = listing(&unc);
        for e in ours.values() {
            eprintln!(
                "UNC {:<12} dir={} logical={} alloc={} attrs={:x} id={:x}",
                e.name, e.is_dir, e.logical, e.alloc, e.attrs, e.file_id
            );
        }
        let mut names: Vec<&str> = ours.keys().map(String::as_str).collect();
        names.sort();
        assert_eq!(names, ["a.bin", "b.bin", "sub", "Привіт.txt"]);
        assert_eq!(
            (ours["b.bin"].logical, ours["Привіт.txt"].logical),
            (5000, 3)
        );
        assert!(ours["b.bin"].alloc >= 5000 && ours["sub"].is_dir);
        shell::delete_permanently(&[unc]).unwrap();
    }
}

#[test]
fn delete_permanently_removes_links_not_their_targets() {
    let fx = Fixture::new("delete");
    let d = &fx.dir;
    fs::create_dir(d.join("keep")).unwrap();
    fs::write(d.join("keep").join("precious.txt"), b"keep me").unwrap();
    let victim = d.join("victim");
    fs::create_dir_all(victim.join("nested").join("deeper")).unwrap();
    fs::write(victim.join("nested").join("deeper").join("f.txt"), b"f").unwrap();
    fs::write(victim.join("ro.txt"), b"ro").unwrap();
    set_attributes(&victim.join("ro.txt"), FILE_ATTRIBUTE_READONLY);
    fs::write(victim.join("hs.txt"), b"hs").unwrap();
    set_attributes(
        &victim.join("hs.txt"),
        FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_READONLY,
    );
    mklink_junction(&victim.join("link-to-keep"), &d.join("keep"));
    mklink_junction(&d.join("alone-link"), &d.join("keep"));
    fs::write(d.join("ro-alone.txt"), b"ro").unwrap();
    set_attributes(&d.join("ro-alone.txt"), FILE_ATTRIBUTE_READONLY);
    let lone: Vec<u16> = "gone-".encode_utf16().chain([0xD800]).collect();
    fs::write(d.join(OsString::from_wide(&lone)), b"x").unwrap();
    let deep = d.join("long").join("x".repeat(150)).join("y".repeat(150));
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("deep.txt"), b"deep").unwrap();

    shell::delete_permanently(std::slice::from_ref(&victim)).unwrap();
    assert!(!victim.exists());
    assert_eq!(
        fs::read(d.join("keep").join("precious.txt")).unwrap(),
        b"keep me",
        "junction target must survive"
    );

    shell::delete_permanently(&[d.join("alone-link"), d.join("ro-alone.txt")]).unwrap();
    assert!(d.join("alone-link").symlink_metadata().is_err() && !d.join("ro-alone.txt").exists());
    assert!(d.join("keep").join("precious.txt").exists());

    let entry = listing(d)
        .into_values()
        .find(|e| e.raw_name.is_some())
        .expect("lone surrogate entry");
    shell::delete_permanently(&[entry.path_in(d)]).unwrap();
    assert!(!d.join(OsString::from_wide(&lone)).exists());

    shell::delete_permanently(&[deep.join("deep.txt")]).unwrap();
    assert!(!deep.join("deep.txt").exists() && deep.exists());
    shell::delete_permanently(&[d.join("long")]).unwrap();
    assert!(!d.join("long").exists());

    let left: Vec<String> = listing(d).into_keys().collect();
    assert_eq!(left, vec!["keep".to_string()]);
}

#[test]
fn drives_match_get_volume() {
    let t = Instant::now();
    let list = drives();
    let took = t.elapsed();
    for d in &list {
        eprintln!("{d:?}");
    }
    eprintln!("drives() took {took:?}");
    assert!(
        took < Duration::from_millis(2500),
        "drives() must not hang: {took:?}"
    );
    assert!(list.windows(2).all(|w| w[0].letter < w[1].letter));

    // Get-Volume: every lettered fixed volume must match exactly in size.
    let ps = powershell(
        "Get-Volume | Where-Object { $_.DriveLetter -and $_.DriveType -eq 'Fixed' } | ForEach-Object { \
           @($_.DriveLetter, $_.Size, $_.SizeRemaining, $_.FileSystem, $_.AllocationUnitSize) -join [char]9 }",
    );
    let mut seen = 0;
    for line in ps.lines().filter(|l| !l.trim().is_empty()) {
        let f: Vec<&str> = line.trim().split('\t').collect();
        let letter = f[0].chars().next().unwrap();
        let (size, remaining): (u64, u64) = (f[1].parse().unwrap(), f[2].parse().unwrap());
        let d = list
            .iter()
            .find(|d| d.letter == letter)
            .unwrap_or_else(|| panic!("{letter}: missing"));
        eprintln!(
            "Get-Volume {letter}: size {size} free {remaining} fs {} cluster {}",
            f[3], f[4]
        );
        assert_eq!(d.kind, DriveKind::Fixed);
        assert_eq!(d.fs, f[3]);
        assert_eq!(d.cluster.to_string(), f[4]);
        assert_eq!(d.total, size, "{letter}: total");
        // Other processes write meanwhile: allow 2 GiB of drift.
        assert!(
            d.free.abs_diff(remaining) < 2 * GIB,
            "{letter}: free {} vs {remaining}",
            d.free
        );
        assert!(d.disk_number.is_some(), "{letter}: disk number");
        seen += 1;
    }
    assert!(seen >= 1);
    let c = list.iter().find(|d| d.letter == 'C').expect("C:");
    assert_eq!((c.fs.as_str(), c.kind), ("NTFS", DriveKind::Fixed));
    if let Some(d) = list.iter().find(|d| d.letter == 'D') {
        eprintln!("D: {d:?}");
    }
}

#[test]
fn process_info_matches_system_tools() {
    let elevated = ringscan_win::is_elevated();
    let groups = run("whoami.exe", &["/groups".as_ref()]);
    // S-1-16-12288 = High Mandatory Level: present only in elevated tokens.
    let high = String::from_utf8_lossy(&groups.stdout).contains("S-1-16-12288");
    assert_eq!(elevated, high);
    // The display language: the user's PreferredUILanguages if set, else the
    // system UI language. (CurrentUICulture is no good here: console hosts
    // fall back to English when the console code page does not fit.)
    let lang = powershell(
        "$p = (Get-ItemProperty 'HKCU:\\Control Panel\\Desktop' -Name PreferredUILanguages -ErrorAction SilentlyContinue).PreferredUILanguages\n\
         if ($p) { $p[0] } else { [Globalization.CultureInfo]::InstalledUICulture.Name }",
    );
    assert_eq!(
        ringscan_win::ui_lang_ukrainian(),
        lang.trim().starts_with("uk"),
        "display language {lang:?}"
    );
    eprintln!("elevated = {elevated}, display language = {}", lang.trim());
}

/// Touches the user's clipboard, so it only runs on request
/// (`-- --ignored`) and only when the clipboard is completely empty (nothing
/// can be lost); it empties the clipboard again afterwards.
#[test]
#[ignore]
fn clipboard_round_trip() {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, CountClipboardFormats, EmptyClipboard, GetClipboardData, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    // SAFETY: plain FFI call.
    if unsafe { CountClipboardFormats() } != 0 {
        eprintln!("clipboard is not empty: skipped");
        return;
    }
    let text = "RingScan clipboard test: Привіт 😀 C:\\Temp\\x";
    shell::copy_to_clipboard(text).unwrap();
    // SAFETY: standard clipboard read; the handle stays owned by the system.
    let got = unsafe {
        assert_ne!(OpenClipboard(std::ptr::null_mut()), 0);
        let h = GetClipboardData(13 /* CF_UNICODETEXT */);
        let mut units = vec![];
        if !h.is_null() {
            let p = GlobalLock(h).cast::<u16>();
            let mut i = 0;
            while *p.add(i) != 0 {
                units.push(*p.add(i));
                i += 1;
            }
            GlobalUnlock(h);
        }
        EmptyClipboard();
        CloseClipboard();
        String::from_utf16(&units).unwrap()
    };
    assert_eq!(got, text);
}
