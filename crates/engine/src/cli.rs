//! Headless command line: `ringscan --cli <PATH|X:|all> [options]`.
//!
//! [`run_cli`] scans the target with a [`ScanJob`], waits for it and
//! prints a report: the tree to a given depth with sizes, the largest
//! files, totals, the method used (and why the MFT was not), elapsed time,
//! throughput, errors, per-drive space accounting and memory. `--json`
//! prints the same as one JSON object (stable keys, raw byte counts) for
//! scripts and tests. The GUI binary routes `ringscan.exe --cli ...` here;
//! `ringscan-cli` is a stand-alone build of the same entry point.
//!
//! Exit codes: 0 = scanned (individual folders may have been inaccessible,
//! see `errors`), 1 = nothing could be scanned (the target itself could not
//! be listed, or no drives), 2 = usage error.

use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use ringscan_core::{Lang, NodeId, SizeMode, Tree, flags, format, stats};
use serde_json::{Value, json};

use crate::job::{Method, ScanJob, ScanOptions, ScanProgress, Target};

const USAGE: &str = "\
Usage: ringscan --cli <PATH|X:|all> [options]

Scans a folder, a drive (X:) or all fixed drives (all) and prints a report.

Options:
  --json              print one JSON object instead of text
  --method <M>        auto (default: MFT for a whole NTFS drive if the raw
                      volume is readable - usually needs elevation - else
                      walk), mft or walk
  --depth <N>         tree levels to print below the target (default 1)
  --top <N>           number of largest files to list (default 10)
  --threads <N>       walker threads per drive (default 8)
  --logical           report file lengths instead of allocated sizes
  --all-links         walk mode: count every hard link (like Explorer)
                      instead of each file once
  --max-children <N>  entries shown per folder (default 50, 0 = all)
  --progress          progress lines on stderr (default: if a terminal)
  --dump <FILE>       also write every file as a UTF-8 TSV line:
                      logical, allocated, flags, path
  -h, --help          this help
";

/// Parsed command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliArgs {
    pub target: String,
    pub json: bool,
    pub method: Method,
    pub depth: usize,
    pub top: usize,
    pub threads: usize,
    pub logical: bool,
    pub all_links: bool,
    pub max_children: usize,
    pub progress: Option<bool>,
    /// Write every file (non-directory) as TSV to this path.
    pub dump: Option<String>,
}

impl CliArgs {
    /// Parses the arguments after the program name. `--cli <target>` or a
    /// bare target; `--opt value` and `--opt=value` both work. `Ok(None)`
    /// means help was requested.
    pub fn parse(args: &[String]) -> Result<Option<CliArgs>, String> {
        let mut a = CliArgs {
            target: String::new(),
            json: false,
            method: Method::Auto,
            depth: 1,
            top: 10,
            threads: 0,
            logical: false,
            all_links: false,
            max_children: 50,
            progress: None,
            dump: None,
        };
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let (key, inline) = match arg.split_once('=') {
                Some((k, v)) if k.starts_with("--") => (k, Some(v.to_string())),
                _ => (arg.as_str(), None),
            };
            let mut value = |name: &str| -> Result<String, String> {
                match inline.clone() {
                    Some(v) => Ok(v),
                    None => it.next().cloned().ok_or(format!("{name} needs a value")),
                }
            };
            let number = |name: &str, v: String| -> Result<usize, String> {
                v.trim()
                    .parse()
                    .map_err(|_| format!("{name}: not a number: {v}"))
            };
            match key {
                "-h" | "--help" | "/?" => return Ok(None),
                "--cli" => {
                    // `--cli` may also be a bare mode switch before the target.
                    if let Some(v) = inline.clone() {
                        a.target = v;
                    }
                }
                "--json" => a.json = true,
                "--logical" => a.logical = true,
                "--all-links" => a.all_links = true,
                "--progress" => a.progress = Some(true),
                "--no-progress" => a.progress = Some(false),
                "--method" => {
                    let v = value(key)?;
                    a.method = Method::parse(&v).ok_or(format!("unknown method: {v}"))?;
                }
                "--mft" => a.method = Method::Mft,
                "--walk" => a.method = Method::Walk,
                "--depth" => a.depth = number(key, value(key)?)?,
                "--top" => a.top = number(key, value(key)?)?,
                "--threads" => a.threads = number(key, value(key)?)?,
                "--max-children" => a.max_children = number(key, value(key)?)?,
                "--dump" => a.dump = Some(value(key)?),
                k if k.starts_with("--") => return Err(format!("unknown option: {k}")),
                _ if a.target.is_empty() => a.target = arg.clone(),
                _ => return Err(format!("unexpected argument: {arg}")),
            }
        }
        if a.target.is_empty() {
            return Err("missing target (a folder, a drive such as C: or \"all\")".into());
        }
        Ok(Some(a))
    }

    fn options(&self) -> ScanOptions {
        ScanOptions {
            method: self.method,
            threads: self.threads,
            dedupe_hardlinks: !self.all_links,
            ..ScanOptions::default()
        }
    }

    fn mode(&self) -> SizeMode {
        if self.logical {
            SizeMode::Logical
        } else {
            SizeMode::Allocated
        }
    }
}

/// Runs the CLI with the arguments after the program name (`--cli` may be
/// among them) and returns the process exit code.
pub fn run_cli(args: &[String]) -> i32 {
    let a = match CliArgs::parse(args) {
        Ok(Some(a)) => a,
        Ok(None) => {
            print!("{USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("ringscan: {e}\n\n{USAGE}");
            return 2;
        }
    };
    let target = Target::parse(
        &ringscan_win::context_menu::path_from_arg(a.target.as_ref()).to_string_lossy(),
    );
    let job = ScanJob::start(target, a.options());
    let show_progress = a.progress.unwrap_or_else(|| io::stderr().is_terminal());
    let progress = wait_with_progress(&job, show_progress);
    let report = Report::collect(&job, &progress, &a);
    if let Some(path) = &a.dump
        && let Err(e) = dump_files(&job.tree().read(), path)
    {
        eprintln!("ringscan: cannot write {path}: {e}");
        return 1;
    }
    let mut out = io::stdout().lock();
    let written = if a.json {
        writeln!(out, "{}", report.json)
    } else {
        out.write_all(report.text.as_bytes())
    };
    let _ = written.and_then(|()| out.flush());
    report.exit_code
}

/// Waits for the job, printing a progress line every 2 s if asked.
fn wait_with_progress(job: &ScanJob, show: bool) -> ScanProgress {
    let mut last = Instant::now();
    let terminal = io::stderr().is_terminal();
    while !job.is_done() {
        std::thread::sleep(Duration::from_millis(100));
        if show && last.elapsed() >= Duration::from_secs(2) {
            last = Instant::now();
            let p = job.progress();
            let mut line = format!(
                "[{:>6.1} s] {}: {} files, {} dirs, {}, {} errors",
                p.elapsed.as_secs_f64(),
                p.phase.name(),
                format::count(p.files, Lang::En),
                format::count(p.dirs, Lang::En),
                format::size(p.bytes, Lang::En),
                p.errors
            );
            if p.mft_records_total > 0 && p.files == 0 {
                line.push_str(&format!(
                    ", MFT {}/{} records",
                    p.mft_records_done, p.mft_records_total
                ));
            }
            if !p.current.is_empty() {
                line.push_str(" - ");
                line.push_str(&ellipsize(&p.current, 60));
            }
            if terminal {
                eprint!("\r{line:<120.120}");
            } else {
                eprintln!("{line}");
            }
        }
    }
    if show && terminal {
        eprintln!();
    }
    job.wait()
}

/// Writes `logical \t alloc \t flags \t path` for every live file
/// (non-directory, non-virtual) node.
fn dump_files(t: &Tree, path: &str) -> io::Result<()> {
    let mut w = io::BufWriter::new(std::fs::File::create(path)?);
    for (id, n) in t.live_nodes() {
        if n.is_file() {
            writeln!(
                w,
                "{}\t{}\t{}\t{}",
                n.logical(),
                n.alloc(),
                flag_names(n.flags()).join(","),
                t.path(id)
            )?;
        }
    }
    w.flush()
}

fn ellipsize(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        let tail: String = s.chars().skip(n - (max - 1)).collect();
        format!("…{tail}")
    }
}

/// Every flag with its report name.
const FLAG_NAMES: [(u16, &str); 14] = [
    (flags::DIR, "dir"),
    (flags::REPARSE, "reparse"),
    (flags::COMPRESSED, "compressed"),
    (flags::SPARSE, "sparse"),
    (flags::HIDDEN, "hidden"),
    (flags::SYSTEM, "system"),
    (flags::ERROR, "error"),
    (flags::CLOUD, "cloud"),
    (flags::HARDLINK, "hardlink"),
    (flags::RECYCLE, "recycle"),
    (flags::METAFILE, "metafile"),
    (flags::FREE, "free"),
    (flags::UNACCOUNTED, "unaccounted"),
    (flags::VIRTUAL, "virtual"),
];

/// Short names of the flags set in `f`.
pub fn flag_names(f: u16) -> Vec<&'static str> {
    FLAG_NAMES
        .iter()
        .filter(|(bit, _)| f & bit != 0)
        .map(|&(_, name)| name)
        .collect()
}

/// Children of `id` ordered by size in `mode`, largest first (ties: name).
fn sorted_children(t: &Tree, id: NodeId, mode: SizeMode) -> Vec<NodeId> {
    let mut kids: Vec<NodeId> = t.children(id).collect();
    kids.sort_by(|&a, &b| {
        t.size(b, mode)
            .cmp(&t.size(a, mode))
            .then_with(|| t.name(a).cmp(t.name(b)))
    });
    kids
}

struct Report {
    text: String,
    json: Value,
    exit_code: i32,
}

impl Report {
    fn collect(job: &ScanJob, p: &ScanProgress, a: &CliArgs) -> Report {
        let drives = job.drives();
        let tree = job.tree().read();
        let t = &*tree;
        let mode = a.mode();
        let root = t.root();
        let memory = crate::sys::memory_info();
        // Real paths (the tree's own `path` uses `\` everywhere).
        let path_of = |id: NodeId| {
            job.node_path_in(t, id)
                .map_or_else(|| t.path(id), |p| p.display().to_string())
        };
        // One pass over the arena: entries per flag, and folders with errors.
        let mut flag_counts = [0u64; 16];
        let mut error_paths = Vec::new();
        for (id, n) in t.live_nodes() {
            for (bit, count) in flag_counts.iter_mut().enumerate() {
                if n.flags() & (1 << bit) != 0 {
                    *count += 1;
                }
            }
            if n.has(flags::ERROR) && error_paths.len() < 50 {
                error_paths.push(path_of(id));
            }
        }
        let hardlinks = flag_counts[flags::HARDLINK.trailing_zeros() as usize];
        let flag_json: serde_json::Map<String, Value> = FLAG_NAMES
            .iter()
            .map(|&(bit, name)| {
                let count = flag_counts[bit.trailing_zeros() as usize];
                (name.to_string(), json!(count))
            })
            .collect();
        let largest = if a.top > 0 {
            stats::largest_files(t, root, mode, a.top)
        } else {
            Vec::new()
        };
        let scanned_nothing = (t.node(root).has(flags::ERROR) && !t.has_children(root))
            || (t.node(root).is_virtual() && p.units.is_empty());
        let threads = a.options().walk_threads();
        let method_text = |m: Option<Method>| m.map_or("mixed", |m| m.name());

        // ----- JSON -------------------------------------------------------
        let units: Vec<Value> = p
            .units
            .iter()
            .map(|u| {
                json!({
                    "name": u.name,
                    "drive": u.drive.map(|c| c.to_string()),
                    "method": u.method.map(Method::name),
                    "note": u.note,
                    "files": u.files,
                    "dirs": u.dirs,
                    "bytes": u.bytes,
                    "errors": u.errors,
                    "elapsed_ms": u.elapsed.as_millis() as u64,
                    "mft": u.mft.as_ref().map(|m| json!({
                        "records": m.records,
                        "unreadable_records": m.unreadable_records,
                        "read_ms": m.read_time.as_millis() as u64,
                        "build_ms": m.build_time.as_millis() as u64,
                        "hardlinks": m.build.hardlinks,
                        "orphans": m.build.orphans,
                        "metafiles": m.build.metafiles,
                    })),
                })
            })
            .collect();
        let drives_json: Vec<Value> = drives
            .iter()
            .map(|d| {
                json!({
                    "letter": d.info.letter.to_string(),
                    "label": d.info.label,
                    "fs": d.info.fs,
                    "cluster": d.info.cluster,
                    "disk": d.info.disk_number,
                    "total": d.space.total,
                    "free": d.space.free,
                    "used": d.space.used,
                    "scanned": d.space.scanned,
                    "unaccounted": d.space.unaccounted,
                    "overcount": d.space.overcount,
                    "method": d.method.map(Method::name),
                    "note": d.note,
                })
            })
            .collect();
        let json = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "target": job.target().to_string(),
            "root": t.name(root),
            "method": p.method.map(Method::name),
            "size_mode": if a.logical { "logical" } else { "allocated" },
            "threads": threads,
            "hardlinks_counted_once": !a.all_links,
            "elapsed_ms": p.elapsed.as_millis() as u64,
            "cancelled": p.cancelled,
            "files": p.files,
            "dirs": p.dirs,
            "errors": p.errors,
            "files_per_sec": p.files_per_sec().round(),
            "entries_per_sec": p.entries_per_sec().round(),
            "total": {
                "alloc": t.size(root, SizeMode::Allocated),
                "logical": t.size(root, SizeMode::Logical),
                "files": t.files(root),
                "dirs": t.dirs(root),
                "hardlink_entries": hardlinks,
            },
            "entries_by_flag": flag_json,
            "error_paths": error_paths,
            "memory": {
                "nodes": t.live_len(),
                "tree_bytes": t.memory_bytes(),
                "peak_resident": memory.map(|m| m.peak_resident),
                "peak_private": memory.map(|m| m.peak_private),
            },
            "units": units,
            "drives": drives_json,
            "tree": node_json(t, root, a.depth, mode, a.max_children),
            "largest_files": largest.iter().map(|&id| json!({
                "path": path_of(id),
                "alloc": t.node(id).alloc(),
                "logical": t.node(id).logical(),
            })).collect::<Vec<_>>(),
        });

        // ----- text -------------------------------------------------------
        let en = Lang::En;
        let mut s = String::new();
        let mut line = |l: String| {
            s.push_str(&l);
            s.push('\n');
        };
        line(format!(
            "RingScan {} - {}",
            env!("CARGO_PKG_VERSION"),
            job.target()
        ));
        line(format!(
            "Method:   {}{}",
            method_text(p.method),
            if p.cancelled { " (cancelled)" } else { "" }
        ));
        for u in &p.units {
            if p.units.len() > 1 || u.note.is_some() {
                line(format!(
                    "  {:<10} {:<5} {}",
                    u.name,
                    u.method.map_or("-", Method::name),
                    u.note.as_deref().unwrap_or("")
                ));
            }
            if let Some(m) = &u.mft {
                line(format!(
                    "  {:<10} MFT: {} records read in {:.2} s, tree built in {:.2} s, {} hard links, {} orphans, {} unreadable",
                    u.name,
                    format::count(m.records, en),
                    m.read_time.as_secs_f64(),
                    m.build_time.as_secs_f64(),
                    format::count(m.build.hardlinks, en),
                    format::count(m.build.orphans, en),
                    m.unreadable_records
                ));
            }
        }
        let walked = p.units.iter().any(|u| u.method == Some(Method::Walk));
        line(format!(
            "Elapsed:  {:.2} s{} ({} files/s, {} entries/s)",
            p.elapsed.as_secs_f64(),
            if walked {
                format!(" with {threads} walker threads")
            } else {
                String::new()
            },
            format::count(p.files_per_sec().round() as u64, en),
            format::count(p.entries_per_sec().round() as u64, en)
        ));
        line(format!(
            "Found:    {} files, {} folders, {} errors (inaccessible folders/entries)",
            format::count(t.files(root), en),
            format::count(t.dirs(root), en),
            format::count(p.errors, en)
        ));
        line(format!(
            "Size:     {} allocated, {} logical{}",
            format::size(t.size(root, SizeMode::Allocated), en),
            format::size(t.size(root, SizeMode::Logical), en),
            if a.all_links {
                " (every hard link counted)".to_string()
            } else if hardlinks > 0 {
                format!(
                    " (hard links counted once; {} extra links)",
                    format::count(hardlinks, en)
                )
            } else {
                String::new()
            }
        ));
        line(format!(
            "Memory:   {} nodes, tree {}{}",
            format::count(t.live_len() as u64, en),
            format::size(t.memory_bytes() as u64, en),
            memory.map_or(String::new(), |m| {
                let mut x = format!(", peak working set {}", format::size(m.peak_resident, en));
                if m.peak_private > 0 {
                    x.push_str(&format!(
                        ", peak private {}",
                        format::size(m.peak_private, en)
                    ));
                }
                x
            })
        ));
        for d in &drives {
            let sp = &d.space;
            line(String::new());
            line(format!(
                "Drive {}: {} ({}), total {}",
                d.info.letter,
                if d.info.label.is_empty() {
                    "-"
                } else {
                    &d.info.label
                },
                d.info.fs,
                format::size(sp.total, en)
            ));
            let pct = |v: u64| format::percent(ratio(v, sp.total), en);
            line(format!(
                "  scanned      {:>10}  {:>6}",
                format::size(sp.scanned, en),
                pct(sp.scanned)
            ));
            line(format!(
                "  unaccounted  {:>10}  {:>6}{}",
                format::size(sp.unaccounted, en),
                pct(sp.unaccounted),
                if sp.overcount > 0 {
                    format!(
                        "  (scan found {} more than used)",
                        format::size(sp.overcount, en)
                    )
                } else {
                    String::new()
                }
            ));
            line(format!(
                "  free         {:>10}  {:>6}",
                format::size(sp.free, en),
                pct(sp.free)
            ));
            line(format!(
                "  sum          {:>10}  (total {})",
                format::size(sp.scanned + sp.unaccounted + sp.free, en),
                format::size(sp.total, en)
            ));
        }
        line(String::new());
        line(format!(
            "Tree ({} size, {} level{}):",
            if a.logical { "logical" } else { "allocated" },
            a.depth,
            if a.depth == 1 { "" } else { "s" }
        ));
        let mut tree_lines = Vec::new();
        node_text(t, root, 0, a.depth, mode, a.max_children, &mut tree_lines);
        if let Some(first) = tree_lines.first_mut()
            && let Some(p) = job.node_path_in(t, root)
        {
            *first = first.replacen(&t.path(root), &p.display().to_string(), 1);
        }
        for l in tree_lines {
            line(l);
        }
        if !largest.is_empty() {
            line(String::new());
            line(format!("Largest files ({}):", largest.len()));
            for &id in &largest {
                line(format!(
                    "  {:>10}  {}",
                    format::size(t.size(id, mode), en),
                    path_of(id)
                ));
            }
        }
        if !error_paths.is_empty() {
            line(String::new());
            line(format!(
                "Not fully readable ({} folders/entries{}):",
                format::count(flag_counts[flags::ERROR.trailing_zeros() as usize], en),
                if error_paths.len() > 10 {
                    ", first 10"
                } else {
                    ""
                }
            ));
            for p in error_paths.iter().take(10) {
                line(format!("  {p}"));
            }
        }
        if scanned_nothing {
            line(String::new());
            line("Nothing could be scanned.".into());
        }
        Report {
            text: s,
            json,
            exit_code: if scanned_nothing { 1 } else { 0 },
        }
    }
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

fn node_json(t: &Tree, id: NodeId, depth: usize, mode: SizeMode, max_children: usize) -> Value {
    let n = t.node(id);
    let mut v = json!({
        "name": t.name(id),
        "alloc": n.alloc(),
        "logical": n.logical(),
        "flags": flag_names(n.flags()),
    });
    if n.is_dir() {
        v["files"] = json!(t.files(id));
        v["dirs"] = json!(t.dirs(id));
        if depth > 0 {
            let kids = sorted_children(t, id, mode);
            let shown = if max_children == 0 {
                kids.len()
            } else {
                kids.len().min(max_children)
            };
            v["children"] = Value::Array(
                kids[..shown]
                    .iter()
                    .map(|&c| node_json(t, c, depth - 1, mode, max_children))
                    .collect(),
            );
            if shown < kids.len() {
                let rest = &kids[shown..];
                v["more"] = json!({
                    "count": rest.len(),
                    "alloc": rest.iter().map(|&c| t.node(c).alloc()).sum::<u64>(),
                    "logical": rest.iter().map(|&c| t.node(c).logical()).sum::<u64>(),
                });
            }
        }
    }
    v
}

fn node_text(
    t: &Tree,
    id: NodeId,
    level: usize,
    depth: usize,
    mode: SizeMode,
    max_children: usize,
    out: &mut Vec<String>,
) {
    let en = Lang::En;
    let n = t.node(id);
    let parent = n.parent();
    let share = if level == 0 || parent == ringscan_core::NONE {
        1.0
    } else {
        ratio(t.size(id, mode), t.size(parent, mode))
    };
    let mut name = if level == 0 {
        t.path(id)
    } else {
        t.name(id).to_string()
    };
    if name.is_empty() {
        name = t.name(id).to_string();
    }
    let mut notes = Vec::new();
    if n.is_dir() && !n.is_virtual() {
        notes.push(format!("{} files", format::count(t.files(id), en)));
    }
    for (bit, label) in [
        (flags::REPARSE, "link, not followed"),
        (flags::ERROR, "not fully readable"),
        (flags::CLOUD, "cloud"),
        (flags::HARDLINK, "hard link"),
        (flags::METAFILE, "NTFS metadata"),
    ] {
        if n.has(bit) {
            notes.push(label.to_string());
        }
    }
    let size = if n.has(flags::FREE) {
        n.alloc()
    } else {
        t.size(id, mode)
    };
    out.push(format!(
        "{:>10} {:>7}  {}{}{}{}",
        format::size(size, en),
        if n.has(flags::FREE) {
            String::new()
        } else {
            format::percent(share, en)
        },
        "  ".repeat(level),
        name,
        if n.is_dir() && level > 0 && !n.is_virtual() && !name.ends_with(['\\', '/']) {
            "\\"
        } else {
            ""
        },
        if notes.is_empty() {
            String::new()
        } else {
            format!("  ({})", notes.join(", "))
        }
    ));
    if level < depth && n.is_dir() {
        let kids = sorted_children(t, id, mode);
        let shown = if max_children == 0 {
            kids.len()
        } else {
            kids.len().min(max_children)
        };
        for &c in &kids[..shown] {
            node_text(t, c, level + 1, depth, mode, max_children, out);
        }
        if shown < kids.len() {
            let rest = &kids[shown..];
            let size: u64 = rest.iter().map(|&c| t.size(c, mode)).sum();
            out.push(format!(
                "{:>10} {:>7}  {}... {} more items",
                format::size(size, en),
                "",
                "  ".repeat(level + 1),
                format::count(rest.len() as u64, en)
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_options() {
        let a = CliArgs::parse(&args(&[
            "--cli",
            r"C:\Windows",
            "--json",
            "--method",
            "walk",
            "--depth=2",
            "--top",
            "5",
            "--threads",
            "16",
            "--logical",
            "--all-links",
            "--max-children",
            "0",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(a.target, r"C:\Windows");
        assert_eq!(a.dump, None);
        assert!(a.json && a.logical && a.all_links);
        assert_eq!(
            (a.method, a.depth, a.top, a.threads, a.max_children),
            (Method::Walk, 2, 5, 16, 0)
        );
        let b = CliArgs::parse(&args(&["all"])).unwrap().unwrap();
        assert_eq!(
            (b.target.as_str(), b.depth, b.top, b.method),
            ("all", 1, 10, Method::Auto)
        );
        assert_eq!(CliArgs::parse(&args(&["--help"])), Ok(None));
        assert!(CliArgs::parse(&args(&[])).is_err());
        assert!(CliArgs::parse(&args(&["x", "--depth"])).is_err());
        assert!(CliArgs::parse(&args(&["x", "--depth", "z"])).is_err());
        assert!(CliArgs::parse(&args(&["x", "--bogus"])).is_err());
        assert!(CliArgs::parse(&args(&["x", "y"])).is_err());
        assert!(CliArgs::parse(&args(&["x", "--method", "fast"])).is_err());
    }

    #[test]
    fn flag_names_lists_bits() {
        assert_eq!(flag_names(flags::DIR | flags::HIDDEN), ["dir", "hidden"]);
        assert!(flag_names(0).is_empty());
    }
}
