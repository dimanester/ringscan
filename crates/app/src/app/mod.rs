//! The application: state, frame loop and action handling.
//!
//! # Frame anatomy
//!
//! 1. `raw_input_hook`: the test harness (if any) swaps in scripted input.
//! 2. Harness commands, keyboard shortcuts and finished background jobs
//!    are processed; the scan's progress is sampled.
//! 3. [`App::ensure_model`] rebuilds the chart model under one short read
//!    lock if its inputs changed. While a scan runs the tree's generation
//!    changes constantly, so a generation-only change is applied at most
//!    every 125 ms (8 Hz); focus, settings or search changes apply at once.
//! 4. Panels are drawn (top bar, status bar, sidebar, details, chart).
//!    Widgets only return [`Action`]s.
//! 5. Actions are applied (navigation, scans, dialogs, platform calls).
//!
//! The tree lock is never held across a frame: the UI uses short
//! `read_recursive` sections (model build, the visible list rows, the
//! hovered node's tooltip), and background jobs use their own read locks.
//!
//! Navigation keeps a back stack of focus ids. Ids stay valid after a
//! delete (tombstones), and the focus is moved to its live ancestor when
//! its folder disappears.
//!
//! # Files
//!
//! * `mod.rs` — state, construction, model upkeep, navigation, shortcuts,
//!   search, and the `eframe::App` frame loop;
//! * `chart_view.rs` — the chart area (mesh cache, animation, hit testing,
//!   context menu, center disc, legend, tooltip);
//! * `apply.rs` — executing actions, confirmations and platform calls;
//! * `chrome.rs` — drive cards, breadcrumb segments, taskbar icon;
//! * `scripting.rs` — harness commands and the JSON state dump.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use eframe::egui::{
    self, CentralPanel, Context, Frame, Key, Modifiers, PointerButton, Rect, Stroke, TextureHandle,
    Theme, Ui, ViewportCommand, pos2,
};
use ringscan_core::{NONE, NodeId, stats};

use crate::actions::{Action, MenuTarget};
use crate::chart::Transition;
use crate::details::{self, Details, FileRow, SearchResult};
use crate::dialogs::{self, Answer, Confirm, Toasts};
use crate::harness::Harness;
use crate::i18n::Tr;
use crate::jobs::{self, Slot};
use crate::model::{ChartModel, Highlight, ModelKey, VirtualNames};
use crate::panels;
use crate::platform::{self, DriveInfo, SystemInfo};
use crate::scan_source::{Scan, ScanStatus};
use crate::settings::{self, ColorMode, Settings};
use crate::theme::{self, UiFont};

mod apply;
mod chart_view;
mod chrome;
mod scripting;

use chart_view::{Hit, MeshCache};

/// What the app was started with.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Folder to scan (None = all fixed drives under "This PC").
    pub path: Option<PathBuf>,
    /// Synthetic demo tree with this many nodes instead of a scan.
    pub demo: Option<usize>,
    /// Balanced synthetic tree for performance tests (`--perf-tree`).
    pub perf_tree: bool,
    /// UI test script.
    pub script: Option<String>,
    /// Load and save settings even in script mode (to test persistence;
    /// scripted runs use a throwaway state folder, see main.rs).
    pub keep_state: bool,
}

/// Reference time for age colors in demo mode (synthetic mtimes end at
/// 2026-01-01): 2026-01-31.
const DEMO_NOW: i64 = 1_767_225_600 + 30 * 86_400;
/// Search hits kept (and highlighted).
const SEARCH_LIMIT: usize = 5_000;

type SearchKey = (String, NodeId, u64);

/// Rolling frame-time statistics for the harness log.
#[derive(Default)]
struct FrameStats {
    cpu_ms: Vec<f32>,
    interval_ms: Vec<f32>,
    /// Mesh build times of frames that rebuilt the mesh.
    mesh_ms: Vec<f32>,
    last: Option<Instant>,
    count: u64,
}

impl FrameStats {
    const KEEP: usize = 600;

    fn push(&mut self, cpu: f32, start: Instant) {
        if let Some(last) = self.last {
            self.interval_ms.push((start - last).as_secs_f32() * 1000.0);
        }
        self.last = Some(start);
        self.cpu_ms.push(cpu);
        self.count += 1;
        for v in [&mut self.cpu_ms, &mut self.interval_ms, &mut self.mesh_ms] {
            if v.len() > Self::KEEP {
                v.drain(..v.len() - Self::KEEP);
            }
        }
    }

    fn summary(v: &[f32]) -> serde_json::Value {
        if v.is_empty() {
            return serde_json::Value::Null;
        }
        let mut s = v.to_vec();
        s.sort_by(f32::total_cmp);
        let avg = s.iter().sum::<f32>() / s.len() as f32;
        let p = |q: f32| s[((s.len() - 1) as f32 * q).round() as usize];
        serde_json::json!({
            "n": s.len(), "avg": avg, "p50": p(0.5), "p95": p(0.95), "max": s[s.len() - 1],
        })
    }
}

pub struct App {
    settings: Settings,
    script_mode: bool,
    /// Settings and egui memory are loaded and saved.
    persist: bool,
    os_uk: bool,
    ui_font: UiFont,
    clock: Instant,
    now_unix: i64,
    scan: Option<Scan>,
    status: ScanStatus,
    focus: NodeId,
    back: Vec<NodeId>,
    selected: Option<NodeId>,
    model: Arc<ChartModel>,
    model_time: f64,
    transition: Option<Transition>,
    /// When the last zoom animation ended (labels fade in after it).
    transition_end: f64,
    mesh: Option<MeshCache>,
    mesh_ms: f32,
    /// Vertices of the last animation frame's mesh.
    anim_vertices: usize,
    label_count: usize,
    chart_rect: Rect,
    chart_hover: Option<NodeId>,
    chart_hit: Option<Hit>,
    context_target: Option<MenuTarget>,
    details: Details,
    search_text: String,
    search_edited: f64,
    search: Slot<SearchKey, (SearchResult, Highlight, u64)>,
    focus_search: bool,
    search_has_focus: bool,
    confirm: Option<Confirm>,
    toasts: Toasts,
    drives: Vec<DriveInfo>,
    recycle: Option<(u64, u64)>,
    /// Drive list and Recycle Bin size being queried in the background.
    sysinfo: Option<Receiver<SystemInfo>>,
    explorer_menu: bool,
    elevated: bool,
    /// Arguments "Restart as administrator" computed in script mode (where
    /// it never runs, so no UAC prompt can appear); shown in the log.
    relaunch_args: Option<Vec<String>>,
    pick: Option<Receiver<Option<PathBuf>>>,
    harness: Option<Harness>,
    frames: FrameStats,
    logo: Option<TextureHandle>,
    taskbar_icon_done: bool,
    actions: Vec<Action>,
    /// Text "copied" in script mode (see `Action::CopyPath`).
    last_copied: Option<String>,
    /// Window title last sent to the OS.
    title: String,
    /// A popup (menu) was open when this frame started.
    popup_was_open: bool,
    /// Results of calls made on the shell thread (`platform::on_shell_thread`).
    shell_tx: std::sync::mpsc::Sender<(platform::ShellCall, std::io::Result<()>)>,
    shell_rx: Receiver<(platform::ShellCall, std::io::Result<()>)>,
    /// A recycle / delete / empty-bin operation running in the background,
    /// with the system facts re-read after it.
    destructive: Option<Receiver<(Confirm, std::io::Result<()>, SystemInfo)>>,
}

fn virtual_names(tr: Tr) -> VirtualNames {
    VirtualNames {
        this_pc: tr.s.this_pc,
        free: tr.s.free_space,
        unaccounted: tr.s.unaccounted,
        orphaned: tr.s.orphaned,
    }
}

/// Queries the drive list and the Recycle Bin size on a background thread
/// (a dead network drive can take up to 1.5 s to answer).
fn spawn_system_info(ctx: &Context) -> Receiver<SystemInfo> {
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new()
        .name("ringscan-sysinfo".into())
        .spawn(move || {
            let _ = tx.send(SystemInfo::query());
            ctx.request_repaint();
        });
    rx
}

/// Whether a path argument names an existing folder or drive.
fn scannable(path: &std::path::Path) -> bool {
    match crate::scan_source::target_for(path) {
        ringscan_engine::Target::Drive(c) => std::fs::metadata(format!("{c}:\\")).is_ok(),
        ringscan_engine::Target::Path(p) => std::fs::metadata(p).is_ok_and(|m| m.is_dir()),
        ringscan_engine::Target::AllFixedDrives => true,
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, opts: Options) -> App {
        let ctx = &cc.egui_ctx;
        let ui_font = theme::install(ctx);
        let script_mode = opts.script.is_some();
        let persist = !script_mode || opts.keep_state;
        let mut settings: Settings = if persist {
            cc.storage
                .and_then(|s| eframe::get_value(s, settings::KEY))
                .unwrap_or_default()
        } else {
            Settings::default()
        };
        settings.sanitize();
        theme::apply_theme_pref(ctx, settings.theme);

        let mut toasts = Toasts::default();
        let harness = match opts.script.as_deref().map(Harness::parse) {
            Some(Ok(h)) => Some(h),
            Some(Err(e)) => {
                eprintln!("[ringscan] bad script: {e}");
                toasts.error(format!("script: {e}"), 0.0);
                // Still run, but quit right away.
                Harness::parse("wait 0.5; quit").ok()
            }
            None => None,
        };
        if script_mode {
            // Scripted runs never touch the user's Explorer menu entry.
            platform::use_test_registry_key();
        }
        let os_uk = platform::ui_lang_ukrainian();
        let demo = opts.demo.is_some() || opts.perf_tree;
        let scan = match (&opts.demo, &opts.path) {
            _ if opts.perf_tree => Scan::demo_uniform(7, 4),
            (Some(nodes), _) => Scan::demo(7, *nodes),
            (None, Some(p)) if scannable(p) => Scan::path(p),
            (None, Some(p)) => {
                // A missing folder or drive: say so and show "This PC".
                let tr = Tr::new(settings.lang(os_uk));
                toasts.error(format!("{}: {}", tr.s.toast_not_found, p.display()), 0.0);
                Scan::this_pc()
            }
            (None, None) => Scan::this_pc(),
        };
        let logo = {
            let px = crate::icon_art::render(64);
            let img = egui::ColorImage::from_rgba_unmultiplied([64, 64], &px);
            Some(ctx.load_texture("logo", img, egui::TextureOptions::LINEAR))
        };
        let now_unix = if demo {
            DEMO_NOW
        } else {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(DEMO_NOW)
        };
        let dark = ctx.theme() == Theme::Dark;
        let (shell_tx, shell_rx) = std::sync::mpsc::channel();
        let empty_key = ModelKey {
            generation: u64::MAX,
            focus: NONE,
            params: settings.layout_params(),
            color_mode: settings.color_mode,
            dark,
            search: 0,
            now: now_unix,
        };
        App {
            os_uk,
            ui_font,
            clock: Instant::now(),
            now_unix,
            status: ScanStatus::default(),
            focus: 0,
            back: Vec::new(),
            selected: None,
            model: Arc::new(ChartModel::empty(empty_key)),
            model_time: -1.0,
            transition: None,
            transition_end: -1.0,
            mesh: None,
            mesh_ms: 0.0,
            anim_vertices: 0,
            label_count: 0,
            chart_rect: Rect::NOTHING,
            chart_hover: None,
            chart_hit: None,
            context_target: None,
            details: Details::default(),
            search_text: String::new(),
            search_edited: 0.0,
            search: Slot::default(),
            focus_search: false,
            search_has_focus: false,
            confirm: None,
            toasts,
            drives: Vec::new(),
            recycle: None,
            // Demo screenshots stay independent of the machine.
            sysinfo: (!demo).then(|| spawn_system_info(ctx)),
            explorer_menu: platform::context_menu_installed(),
            elevated: platform::is_elevated(),
            relaunch_args: None,
            pick: None,
            harness,
            frames: FrameStats::default(),
            logo,
            taskbar_icon_done: false,
            actions: Vec::new(),
            last_copied: None,
            title: String::new(),
            popup_was_open: false,
            shell_tx,
            shell_rx,
            destructive: None,
            scan: Some(scan),
            script_mode,
            persist,
            settings,
        }
    }

    fn tr(&self) -> Tr {
        Tr::new(self.settings.lang(self.os_uk))
    }

    fn now(&self) -> f64 {
        self.clock.elapsed().as_secs_f64()
    }

    fn scanning(&self) -> bool {
        self.status.running
    }

    /// Synthetic demo data: paths must never reach the shell.
    fn demo_data(&self) -> bool {
        self.scan.as_ref().is_some_and(Scan::is_demo)
    }

    fn apply_system_info(&mut self, info: SystemInfo) {
        self.drives = info.drives;
        self.recycle = info.recycle;
    }

    fn menu_caps(&self) -> crate::actions::MenuCaps {
        crate::actions::MenuCaps {
            real_files: !self.demo_data(),
            can_rescan: !self.scanning() && !self.demo_data(),
        }
    }

    // ----- model --------------------------------------------------------

    /// Current search highlight and its serial (0 = none).
    fn highlight(&self) -> Option<(&Highlight, u64)> {
        if self.search_text.trim().is_empty() {
            return None;
        }
        self.search.get().map(|r| (&r.1, r.2))
    }

    /// Rebuilds the chart model if its inputs changed (see module docs).
    fn ensure_model(&mut self, ctx: &Context, force: bool) {
        let Some(scan) = &self.scan else { return };
        let now = self.now();
        let dark = ctx.theme() == Theme::Dark;
        let highlight = self.highlight();
        let names = virtual_names(self.tr());
        let tree = scan.tree.read_recursive();
        let key = ModelKey {
            generation: tree.generation(),
            focus: self.focus,
            params: self.settings.layout_params(),
            color_mode: self.settings.color_mode,
            dark,
            search: highlight.map_or(0, |h| h.1),
            now: self.now_unix,
        };
        let old = self.model.key;
        if old == key && !force {
            return;
        }
        let only_generation = ModelKey {
            generation: old.generation,
            ..key
        } == old;
        if only_generation && !force && self.status.running && now - self.model_time < 0.125 {
            ctx.request_repaint_after(std::time::Duration::from_millis(30));
            return;
        }
        let model = ChartModel::build(&tree, key, highlight.map(|h| h.0), &names);
        drop(tree);
        self.focus = model.focus();
        self.model = Arc::new(model);
        self.model_time = now;
    }

    // ----- navigation ---------------------------------------------------

    /// Moves the focus to `target`, animating when one contains the other.
    fn navigate(&mut self, ctx: &Context, target: NodeId, push: bool) {
        let Some(scan) = &self.scan else { return };
        if target == self.focus || target == NONE {
            return;
        }
        let old_focus = self.focus;
        let old_model = Arc::clone(&self.model);
        let (inward, outward, chain_in, chain_out) = {
            let t = scan.tree.read_recursive();
            if !t.is_live(target) || !t.node(target).is_dir() {
                return;
            }
            let inward = t.is_ancestor(old_focus, target);
            let outward = t.is_ancestor(target, old_focus);
            // target and its ancestors below old_focus (zoom in), or
            // old_focus and its ancestors below target (zoom out).
            let chain = |from: NodeId, stop: NodeId| -> Vec<(NodeId, NodeId)> {
                let mut v = Vec::new();
                let mut cur = from;
                while cur != stop && cur != NONE {
                    let p = t.node(cur).parent();
                    v.push((cur, p));
                    cur = p;
                }
                v
            };
            (
                inward,
                outward,
                if inward {
                    chain(target, old_focus)
                } else {
                    Vec::new()
                },
                if outward {
                    chain(old_focus, target)
                } else {
                    Vec::new()
                },
            )
        };
        if push {
            self.back.push(old_focus);
            if self.back.len() > 200 {
                self.back.remove(0);
            }
        }
        self.focus = target;
        self.ensure_model(ctx, true);
        let new_model = Arc::clone(&self.model);
        let now = self.now();
        let anchor_in = |m: &ChartModel, chain: &[(NodeId, NodeId)]| -> Option<usize> {
            for &(id, parent) in chain {
                if let Some(i) = m.index_of(id) {
                    return Some(i);
                }
                if let Some(i) = m.arcs.iter().position(|a| {
                    matches!(a.kind, ringscan_core::layout::ArcKind::Rest { parent: p, .. } if p == parent)
                }) {
                    return Some(i);
                }
            }
            None
        };
        let idx = if inward {
            anchor_in(&old_model, &chain_in)
        } else if outward {
            anchor_in(&new_model, &chain_out)
        } else {
            None
        };
        let (outer, inner) = if inward {
            (old_model, new_model)
        } else {
            (new_model, old_model)
        };
        self.transition = match (idx, inward || outward) {
            (Some(i), true) if !outer.arcs.is_empty() && !inner.arcs.is_empty() => {
                let a = &outer.arcs[i];
                let anchor_top_mid = (self.settings.color_mode == ColorMode::Folders)
                    .then(|| outer.arcs[outer.top_of(i)].mid());
                Some(Transition {
                    anchor: (a.a0, a.a1, a.depth),
                    anchor_top_mid,
                    outer,
                    inner,
                    inward,
                    start: now,
                })
            }
            _ => None,
        };
        self.chart_hit = None;
        ctx.request_repaint();
    }

    fn start_scan(&mut self, scan: Scan) {
        self.scan = Some(scan);
        // A key no real tree has: the next frame always rebuilds.
        self.model = Arc::new(ChartModel::empty(ModelKey {
            generation: u64::MAX,
            focus: NONE,
            ..self.model.key
        }));
        self.focus = 0;
        self.back.clear();
        self.selected = None;
        self.transition = None;
        self.mesh = None;
        self.model_time = -1.0;
        self.details = Details::default();
        self.search.clear();
        self.status = ScanStatus::default();
    }

    // ----- input --------------------------------------------------------

    fn shortcuts(&mut self, ctx: &Context) {
        if self.confirm.is_some() {
            return;
        }
        let typing = ctx.egui_wants_keyboard_input();
        let target = self.chart_hover.or(self.details.hovered).or(self.selected);
        let selected = self.selected;
        let mut acts = Vec::new();
        let mut clear_search = false;
        let mut find = false;
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::COMMAND, Key::F) {
                find = true;
            }
            if i.pointer.button_pressed(PointerButton::Extra1) {
                acts.push(Action::Back);
            }
            if typing {
                return;
            }
            if i.consume_key(Modifiers::ALT, Key::ArrowLeft)
                || i.consume_key(Modifiers::NONE, Key::Backspace)
            {
                acts.push(Action::Back);
            }
            if i.consume_key(Modifiers::ALT, Key::ArrowUp) {
                acts.push(Action::Up);
            }
            if i.consume_key(Modifiers::NONE, Key::F5) {
                acts.push(Action::Rescan(NONE));
            }
            if i.consume_key(Modifiers::SHIFT, Key::Delete) {
                acts.extend(selected.map(|s| Action::AskDelete(vec![s])));
            } else if i.consume_key(Modifiers::NONE, Key::Delete) {
                acts.extend(selected.map(|s| Action::AskRecycle(vec![s])));
            }
            let copy_event = i.events.iter().any(|e| matches!(e, egui::Event::Copy));
            let copy_key = i.consume_key(Modifiers::COMMAND, Key::C);
            if copy_event || copy_key {
                acts.extend(target.map(Action::CopyPath));
            }
            if i.key_pressed(Key::Escape) {
                clear_search = true;
            }
        });
        if find {
            self.focus_search = true;
        }
        if clear_search {
            if !self.search_text.is_empty() {
                self.search_text.clear();
                self.search.clear();
            } else {
                self.selected = None;
            }
        }
        self.actions.extend(acts);
    }

    // ----- search -------------------------------------------------------

    fn update_search(&mut self, ctx: &Context) {
        self.search.poll();
        let q = self.search_text.trim().to_string();
        if q.is_empty() {
            return;
        }
        let now = self.now();
        if now - self.search_edited < 0.2 {
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
            return;
        }
        let Some(scan) = &self.scan else { return };
        let generation = scan.tree.read_recursive().generation();
        let bucket = jobs::gen_bucket(generation, self.status.running, now, 2.0);
        let focus = self.focus;
        let mode = self.settings.size_mode.into();
        let tree = Arc::clone(&scan.tree);
        let query = q.clone();
        self.search.ensure(ctx, (q, focus, bucket), move || {
            let t = tree.read();
            let ids = stats::search(&t, focus, &query, SEARCH_LIMIT);
            let mut hl = Highlight::default();
            for &id in &ids {
                hl.matches.insert(id);
                for a in t.ancestors(id) {
                    if !hl.ancestors.insert(a) || a == focus {
                        break;
                    }
                }
            }
            let hits = ids
                .iter()
                .map(|&id| FileRow {
                    id,
                    name: t.name(id).to_string(),
                    folder: t.path(t.node(id).parent()),
                    size: t.size(id, mode),
                })
                .collect::<Vec<_>>();
            let capped = hits.len() >= SEARCH_LIMIT;
            static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
            (SearchResult { hits, capped }, hl, serial)
        });
    }
}

impl eframe::App for App {
    fn raw_input_hook(&mut self, _ctx: &Context, raw: &mut egui::RawInput) {
        let now = self.now();
        // Ask the jobs directly: `self.status` is only sampled in `ui`.
        let done = self.scan.as_ref().is_some_and(|s| !s.is_running());
        if let Some(h) = &mut self.harness {
            h.drive(raw, now, done);
        }
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let started = Instant::now();
        let ctx = ui.ctx().clone();
        self.set_taskbar_icon(frame);
        self.run_harness_cmds(&ctx);
        if self.harness.is_some() {
            ctx.request_repaint();
        }
        self.finish_destructive();
        self.finish_shell_calls(&ctx);
        if let Some(rx) = &self.sysinfo
            && let Ok(info) = rx.try_recv()
        {
            self.sysinfo = None;
            self.apply_system_info(info);
        }
        if let Some(rx) = &self.pick
            && let Ok(res) = rx.try_recv()
        {
            self.pick = None;
            if let Some(p) = res {
                self.actions.push(Action::ScanPath(p));
            }
        }
        if let Some(scan) = &mut self.scan {
            self.status = scan.status();
            if self.status.running {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
        let popup_open = egui::Popup::is_any_open(&ctx);
        // A click that closes a menu must not also zoom the chart.
        self.popup_was_open = popup_open;
        if !popup_open {
            self.shortcuts(&ctx);
        }
        self.update_search(&ctx);
        self.ensure_model(&ctx, false);

        let tr = self.tr();
        let pal = theme::current(&ctx);
        let now = self.now();

        // Top bar.
        let crumbs = self.crumbs();
        let can_up = self
            .scan
            .as_ref()
            .is_some_and(|s| s.tree.read_recursive().node(self.focus).parent() != NONE);
        let can_rescan = self.scan.as_ref().is_some_and(|s| !s.is_demo());
        let before = self.settings.clone();
        let tb_in = panels::ToolbarIn {
            tr,
            pal,
            can_back: !self.back.is_empty(),
            can_up,
            can_rescan,
            scanning: self.scanning(),
            crumbs: &crumbs,
            elevated: self.elevated,
            explorer_menu: self.explorer_menu,
            focus_search: std::mem::take(&mut self.focus_search),
        };
        let mut search_text = std::mem::take(&mut self.search_text);
        let mut settings = std::mem::take(&mut self.settings);
        let tb_out = egui::Panel::top("toolbar")
            .frame(
                Frame::new()
                    .fill(pal.side)
                    .inner_margin(egui::Margin::symmetric(8, 6)),
            )
            .show_separator_line(false)
            .show(ui, |ui| {
                panels::toolbar(ui, &tb_in, &mut search_text, &mut settings)
            })
            .inner;
        self.search_text = search_text;
        self.settings = settings;
        if tb_out.search_changed {
            self.search_edited = now;
            self.search.clear();
            if self.search_text.trim().is_empty() {
                self.ensure_model(&ctx, false);
            }
        }
        self.search_has_focus = tb_out.search_has_focus;
        self.actions.extend(tb_out.actions);
        if self.settings.theme != before.theme {
            theme::apply_theme_pref(&ctx, self.settings.theme);
        }
        self.settings.sanitize();

        // Status bar.
        let st_in = panels::StatusIn {
            tr,
            pal,
            status: &self.status,
            method: self.scan.as_ref().map(|s| s.method),
            has_scan: self.scan.is_some(),
            elevated: self.elevated,
            focus_size: self.model.center.size,
            focus_files: self.model.center.files,
            time: ctx.input(|i| i.time),
        };
        egui::Panel::bottom("status")
            .frame(
                Frame::new()
                    .fill(pal.side)
                    .inner_margin(egui::Margin::symmetric(12, 4)),
            )
            .show_separator_line(false)
            .exact_size(30.0)
            .show(ui, |ui| panels::status_bar(ui, &st_in));

        // Side panels never take more than ~58 % of the window together.
        let win_w = ctx.content_rect().width();
        // Sidebar.
        if self.settings.left_panel {
            let cards = self.drive_cards();
            let this_pc_active = self
                .scan
                .as_ref()
                .is_some_and(|s| self.focus == 0 && s.tree.read_recursive().node(0).is_virtual());
            let sb_in = panels::SidebarIn {
                tr,
                pal,
                drives: &cards,
                this_pc_active,
                recycle: self.recycle,
                logo: self.logo.as_ref(),
                system_actions: !self.demo_data(),
            };
            let acts = egui::Panel::left("sidebar")
                .frame(
                    Frame::new()
                        .fill(pal.side)
                        .inner_margin(egui::Margin::symmetric(10, 4)),
                )
                .show_separator_line(false)
                .resizable(true)
                .default_size(240.0)
                .size_range(200.0..=(win_w * 0.24).clamp(200.0, 380.0))
                .show(ui, |ui| panels::sidebar(ui, &sb_in))
                .inner;
            self.actions.extend(acts);
        }

        // The content layer (chart + details) on the Mica-like window
        // background: a lighter surface with a rounded top-left corner and
        // a hairline on its top and left edges, as in Windows 11 apps.
        {
            let r = ui.available_rect_before_wrap();
            let p = ui.painter();
            p.rect_filled(
                r,
                egui::CornerRadius {
                    nw: 8,
                    ne: 0,
                    sw: 0,
                    se: 0,
                },
                pal.bg,
            );
            let (x, y) = (r.left() + 0.5, r.top() + 0.5);
            let mut pts = vec![pos2(x, r.bottom())];
            for k in 0..=8 {
                let a = std::f32::consts::FRAC_PI_2 * k as f32 / 8.0;
                pts.push(pos2(x + 8.0 - 8.0 * a.cos(), y + 8.0 - 8.0 * a.sin()));
            }
            pts.push(pos2(r.right(), y));
            p.add(egui::Shape::line(pts, Stroke::new(1.0, pal.layer_border)));
        }

        // Details.
        if self.settings.right_panel
            && let Some(scan) = &self.scan
        {
            let tree = Arc::clone(&scan.tree);
            let generation = tree.read_recursive().generation();
            let search_res = if self.search_text.trim().is_empty() {
                None
            } else {
                self.search.get().map(|r| r.0.clone())
            };
            let search_busy = !self.search_text.trim().is_empty() && search_res.is_none();
            let inputs = details::Inputs {
                tree: &tree,
                model: &self.model,
                tr,
                pal,
                mode: self.settings.size_mode.into(),
                show_free: self.settings.show_free,
                focus: self.focus,
                generation,
                scanning: self.scanning(),
                menu_caps: self.menu_caps(),
                now,
                chart_hover: self.chart_hover,
                selected: self.selected,
                names: virtual_names(tr),
                search: search_res.as_ref(),
                search_busy,
            };
            let mut tab = self.settings.right_tab;
            let mut sort = self.settings.sort;
            let mut asc = self.settings.sort_ascending;
            let details = &mut self.details;
            let out = egui::Panel::right("details")
                .frame(Frame::new().inner_margin(egui::Margin::symmetric(8, 4)))
                .resizable(true)
                .default_size(380.0)
                .size_range(280.0..=(win_w * 0.34).clamp(280.0, 900.0))
                .show(ui, |ui| {
                    details.ui(ui, &inputs, &mut tab, &mut sort, &mut asc)
                })
                .inner;
            self.settings.right_tab = tab;
            self.settings.sort = sort;
            self.settings.sort_ascending = asc;
            self.actions.extend(out.actions);
        } else {
            self.details.hovered = None;
        }

        // Chart.
        CentralPanel::default()
            .frame(Frame::new())
            .show(ui, |ui| self.chart_ui(ui));

        // Dialogs and toasts.
        if let Some(mut c) = self.confirm.take() {
            match dialogs::show_confirm(&ctx, &mut c, tr, &pal) {
                Answer::Pending => self.confirm = Some(c),
                Answer::Cancel => {}
                Answer::Confirm => self.run_confirmed(&ctx, c),
            }
        }
        let area = self.chart_rect;
        self.toasts.show(&ctx, area, &pal, now);

        // Apply what the UI asked for.
        for a in std::mem::take(&mut self.actions) {
            self.apply(&ctx, a);
        }
        if self.transition.is_some() {
            ctx.request_repaint();
        }
        let title = if self.model.center.name.is_empty() {
            "RingScan".to_string()
        } else {
            format!("{} - RingScan", self.model.center.name)
        };
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
        self.publish_probes();
        self.frames
            .push(started.elapsed().as_secs_f32() * 1000.0, started);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if self.persist {
            eframe::set_value(storage, settings::KEY, &self.settings);
        }
    }

    fn persist_egui_memory(&self) -> bool {
        self.persist
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        theme::pal(visuals.dark_mode).side.to_normalized_gamma_f32()
    }
}
