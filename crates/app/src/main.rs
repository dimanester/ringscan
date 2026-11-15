//! RingScan: disk usage as a sunburst chart (GUI entry point).
//!
//! ```text
//! ringscan.exe [PATH]            scan PATH (default: all fixed drives under "This PC")
//! ringscan.exe --cli <PATH|X:|all> [options]
//!                                headless report (ringscan-engine's CLI)
//! ringscan.exe --demo[=NODES]    synthetic tree, no disk access (stable screenshots)
//! ringscan.exe --perf-tree       balanced synthetic tree with ~40k visible arcs
//! ringscan.exe --script "CMDS"   UI test harness (see harness.rs)
//! ringscan.exe --script-file F   same, commands read from a file
//! ```
//!
//! Module map: [`app`] (state, frame loop, actions), [`chart`] (sunburst
//! rendering and animation), [`model`] (layout snapshot per focus),
//! [`panels`] / [`details`] / [`dialogs`] / [`widgets`] (UI), [`theme`] and
//! [`colors`] (look), [`i18n`] (all strings), [`settings`] (persisted),
//! [`harness`] (scripted tests), [`console`] (console for `--cli`), and the
//! two seams to the rest of the workspace: [`platform`] (OS actions through
//! `ringscan-win`) and [`scan_source`] (scans through `ringscan-engine`).
//!
//! `--cli` is routed to `ringscan_engine::run_cli` before any window is
//! created; everything else starts the GUI.

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod actions;
mod app;
mod chart;
mod colors;
mod console;
mod details;
mod dialogs;
mod harness;
mod i18n;
mod icon_art;
mod jobs;
mod model;
mod panels;
mod platform;
mod scan_source;
mod settings;
mod theme;
mod widgets;

use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;

/// Default node count of `--demo`.
const DEMO_NODES: usize = 120_000;

#[derive(Debug, Default)]
struct Args {
    opts: app::Options,
    help: bool,
}

fn parse_args(args: impl Iterator<Item = std::ffi::OsString>) -> Result<Args, String> {
    let mut out = Args::default();
    let mut it = args.peekable();
    while let Some(a) = it.next() {
        let s = a.to_string_lossy().to_string();
        match s.as_str() {
            "--help" | "-h" | "/?" => out.help = true,
            "--demo" => out.opts.demo = Some(DEMO_NODES),
            "--perf-tree" => out.opts.perf_tree = true,
            "--script" => {
                let v = it.next().ok_or("--script needs a command list")?;
                out.opts.script = Some(v.to_string_lossy().into_owned());
            }
            "--script-file" => {
                let f = it.next().ok_or("--script-file needs a path")?;
                let text = std::fs::read_to_string(&f)
                    .map_err(|e| format!("{}: {e}", f.to_string_lossy()))?;
                out.opts.script = Some(text);
            }
            _ if s.starts_with("--demo=") => {
                let n = s["--demo=".len()..]
                    .parse::<usize>()
                    .map_err(|e| format!("--demo: {e}"))?;
                out.opts.demo = Some(n.max(10));
            }
            _ if s.starts_with("--") => return Err(format!("unknown option {s}")),
            _ => {
                if out.opts.path.is_some() {
                    return Err(format!("unexpected argument {s}"));
                }
                out.opts.path = Some(PathBuf::from(a));
            }
        }
    }
    Ok(out)
}

const HELP: &str = "RingScan - disk usage as a sunburst chart

usage: ringscan.exe                    scan all fixed drives (\"This PC\")
       ringscan.exe PATH               scan a folder or a drive (D:\\)
       ringscan.exe --cli <PATH|X:|all> [--json] [--depth N] [--top N] ...
                                       headless report; ringscan.exe --cli --help
       ringscan.exe --demo[=NODES] | --perf-tree
                                       synthetic data (no disk access)
       ringscan.exe --script \"CMDS\" | --script-file FILE
                                       UI test harness";

/// GPU setup. A 2D chart needs no discrete GPU, so prefer the low-power
/// adapter (a laptop's discrete GPU stays asleep) and ask wgpu's allocator
/// for small memory blocks instead of its 256 MB defaults. `WGPU_POWER_PREF`
/// still overrides the adapter choice. On Windows only the DX12 backend is
/// compiled in (see Cargo.toml); DX12 falls back to the WARP software
/// adapter on machines without a usable GPU.
fn wgpu_options() -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup, WgpuSetupCreateNew};
    use eframe::wgpu;
    let mut setup = WgpuSetupCreateNew::without_display_handle();
    setup.power_preference =
        wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::LowPower);
    let base = Arc::clone(&setup.device_descriptor);
    setup.device_descriptor = Arc::new(move |adapter| wgpu::DeviceDescriptor {
        memory_hints: wgpu::MemoryHints::MemoryUsage,
        ..base(adapter)
    });
    WgpuConfiguration {
        wgpu_setup: WgpuSetup::CreateNew(setup),
        ..Default::default()
    }
}

/// `%TEMP%\ringscan-dev\script-state.ron`: scripted runs keep their
/// settings and window state there instead of the user's real profile
/// (eframe's `persistence_path` is the RON file itself).
fn script_state_file() -> PathBuf {
    std::env::temp_dir()
        .join("ringscan-dev")
        .join("script-state.ron")
}

fn main() -> eframe::Result {
    // No "There is no disk in the drive" popups from probing drives.
    ringscan_win::disable_error_dialogs();
    let raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if raw.iter().any(|a| a == "--cli") {
        let console = console::attach();
        let args: Vec<String> = raw
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let code = ringscan_engine::run_cli(&args);
        console.finish();
        std::process::exit(code);
    }
    let args = match parse_args(raw.into_iter()) {
        Ok(a) => a,
        Err(e) => {
            let console = console::attach();
            eprintln!("ringscan: {e}\n\n{HELP}");
            console.finish();
            std::process::exit(2);
        }
    };
    if args.help {
        let console = console::attach();
        println!("{HELP}");
        console.finish();
        return Ok(());
    }
    let scripted = args.opts.script.is_some();
    // RINGSCAN_KEEP_STATE=1 keeps the scripted runs' state folder between
    // runs (and loads/saves settings there) to test persistence.
    let keep_state = scripted && std::env::var_os("RINGSCAN_KEEP_STATE").is_some();
    let icon = {
        let px = icon_art::render(64);
        egui::IconData {
            rgba: px,
            width: 64,
            height: 64,
        }
    };
    let viewport = egui::ViewportBuilder::default()
        .with_title("RingScan")
        .with_app_id("RingScan")
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([760.0, 520.0])
        .with_icon(Arc::new(icon));
    let mut native = eframe::NativeOptions {
        viewport,
        multisampling: 4,
        centered: true,
        persist_window: !scripted || keep_state,
        wgpu_options: wgpu_options(),
        ..Default::default()
    };
    if scripted {
        let file = script_state_file();
        if !keep_state {
            // Start every scripted run from the same (default) state.
            let _ = std::fs::remove_file(&file);
        }
        native.persistence_path = Some(file);
    }
    let mut opts = args.opts;
    opts.keep_state = keep_state;
    let result = eframe::run_native(
        "RingScan",
        native,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, opts)))),
    );
    if let Err(e) = &result {
        // A GUI-subsystem program has no visible stderr: say it in a box
        // (e.g. no usable Direct3D 12 adapter, not even WARP).
        fatal_message(&format!("RingScan could not start:\n\n{e}"));
    }
    result
}

/// Shows a modal error message box (Windows) or prints to stderr.
fn fatal_message(text: &str) {
    eprintln!("ringscan: {text}");
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (text, title) = (wide(text), wide("RingScan"));
        // SAFETY: NUL-terminated UTF-16 strings, no owner window.
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            )
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: &[&str]) -> Result<Args, String> {
        parse_args(v.iter().map(std::ffi::OsString::from))
    }

    #[test]
    fn args() {
        let a = parse(&["C:\\Program Files"]).unwrap();
        assert_eq!(a.opts.path, Some(PathBuf::from("C:\\Program Files")));
        let a = parse(&["--demo=5000", "--script", "quit"]).unwrap();
        assert_eq!(a.opts.demo, Some(5000));
        assert_eq!(a.opts.script.as_deref(), Some("quit"));
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["/?"]).unwrap().help);
    }
}
