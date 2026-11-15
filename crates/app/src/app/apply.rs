//! Executing [`Action`]s: navigation, scans, platform calls and the
//! confirmation flow of destructive actions.
//!
//! Destructive actions run on a worker thread through the engine
//! (`ScanJob::delete` / `empty_recycle_bin`), which also removes the items
//! from the tree and updates the drive's free and unaccounted space; the
//! worker then removes what is gone from the other scans in memory at that
//! moment (`scans::sync_after_delete`) and re-reads the drive list and the
//! Recycle Bin size, so the sidebar shows the space that was freed. If the
//! scan shown meanwhile lost its root folder, the deleted item's parent
//! folder is shown instead.
//!
//! "This PC", a drive card and a folder ("Choose folder…", a breadcrumb
//! above the scanned root) go through `App::show`, which navigates the
//! current tree, switches to a scan in memory or starts a new one (see
//! `scans.rs`).

use std::io;
use std::path::{Path, PathBuf};

use eframe::egui::{Context, ViewportCommand};
use ringscan_core::{NONE, NodeId, flags};
use ringscan_engine::DeleteMode;

use crate::actions::Action;
use crate::dialogs::{Confirm, ConfirmKind};
use crate::platform::{self, ShellCall, SystemInfo};
use crate::scan_source::{RescanError, Scan, target_args};

use super::App;
use super::scans::{self, Want};

impl App {
    // ----- actions ------------------------------------------------------

    /// Real file-system path of a live node; None for virtual nodes, nodes
    /// inside a virtual container and demo data (whose made-up paths must
    /// never reach the shell).
    pub(super) fn path_of(&self, id: NodeId) -> Option<PathBuf> {
        let scan = self.scan.as_ref()?;
        let t = scan.tree.read_recursive();
        if !t.is_live(id) || t.node(id).is_virtual() {
            return None;
        }
        scan.node_path(&t, id)
    }

    /// Shows the outcome of a platform action as a toast. Returns true on
    /// success.
    pub(super) fn report(&mut self, r: io::Result<()>, ok: Option<&str>) -> bool {
        let tr = self.tr();
        let now = self.now();
        match r {
            Ok(()) => {
                if let Some(m) = ok {
                    self.toasts.info(m, now);
                }
                true
            }
            Err(e) if ringscan_win::shell::is_not_recyclable(&e) => {
                self.toasts.error(tr.s.toast_not_recyclable, now);
                false
            }
            Err(e) => {
                self.toasts.error(format!("{}: {e}", tr.s.toast_error), now);
                false
            }
        }
    }

    pub(super) fn apply(&mut self, ctx: &Context, action: Action) {
        let tr = self.tr();
        let now = self.now();
        match action {
            Action::Zoom(id) => self.navigate(ctx, id, true),
            Action::Up => {
                if let Some(scan) = &self.scan {
                    let p = scan.tree.read_recursive().node(self.focus).parent();
                    if p != NONE {
                        self.navigate(ctx, p, true);
                    }
                }
            }
            Action::Back => {
                while let Some(prev) = self.back.pop() {
                    let live = self
                        .scan
                        .as_ref()
                        .is_some_and(|s| s.tree.read_recursive().is_live(prev));
                    if live && prev != self.focus {
                        self.navigate(ctx, prev, false);
                        break;
                    }
                }
            }
            Action::Select(id) => self.selected = id,
            Action::Rescan(id) => {
                let id = if id == NONE { self.focus } else { id };
                let Some(scan) = self.scan.as_mut() else {
                    return;
                };
                match scan.rescan(id) {
                    Ok(()) | Err(RescanError::Demo) => {}
                    Err(RescanError::Busy) => self.toasts.info(tr.s.toast_busy, now),
                    Err(RescanError::Failed(e)) => {
                        self.toasts.error(format!("{}: {e}", tr.s.toast_error), now);
                    }
                }
            }
            Action::CancelScan => {
                if let Some(s) = &mut self.scan {
                    s.cancel();
                }
            }
            Action::CancelBackgroundScans => {
                // Only the running ones: a finished scan stays complete.
                for c in self.cache.iter_mut() {
                    c.scan.cancel();
                }
            }
            Action::Open(id) => {
                if let Some(p) = self.path_of(id) {
                    self.shell_call(ctx, ShellCall::Action, move || platform::open(&p));
                }
            }
            Action::Reveal(id) => {
                if let Some(p) = self.path_of(id) {
                    self.shell_call(ctx, ShellCall::Action, move || platform::reveal(&p));
                }
            }
            Action::Properties(id) => {
                if let Some(p) = self.path_of(id) {
                    self.shell_call(ctx, ShellCall::Action, move || platform::properties(&p));
                }
            }
            Action::CopyPath(id) => {
                if let Some(p) = self.path_of(id) {
                    let p = p.to_string_lossy().into_owned();
                    // Scripted runs must not overwrite the user's clipboard:
                    // the text goes to the state log instead.
                    if self.script_mode {
                        self.last_copied = Some(p);
                    } else {
                        ctx.copy_text(p);
                    }
                    self.toasts.info(tr.s.toast_copied, now);
                }
            }
            Action::AskRecycle(ids) | Action::AskDelete(ids) if ids.is_empty() => {}
            Action::AskRecycle(ids) => self.ask(ConfirmKind::Recycle, ids),
            Action::AskDelete(ids) => self.ask(ConfirmKind::Delete, ids),
            Action::AskEmptyBin if self.demo_data() => {}
            Action::AskEmptyBin => {
                self.confirm = Some(Confirm::new(
                    ConfirmKind::EmptyBin,
                    self.scan_id(),
                    Vec::new(),
                    Vec::new(),
                    self.recycle.map_or(0, |r| r.0),
                ));
            }
            Action::ScanThisPc => self.show(ctx, &Want::ThisPc, Scan::this_pc),
            Action::ScanPath(p) => self.show(ctx, &Want::for_path(&p), || Scan::path(&p)),
            Action::ShowDrive(letter) => {
                self.show(ctx, &Want::Drive(letter), || Scan::drive(letter));
            }
            Action::PickFolder => {
                if self.pick.is_none() {
                    let (tx, rx) = std::sync::mpsc::channel();
                    let title = tr.s.choose_folder.trim_end_matches('…').to_string();
                    let ctx2 = ctx.clone();
                    let _ = std::thread::Builder::new()
                        .name("ringscan-pick".into())
                        .spawn(move || {
                            let _ = tx.send(platform::pick_folder(&title));
                            ctx2.request_repaint();
                        });
                    self.pick = Some(rx);
                }
            }
            Action::InstalledApps => {
                self.shell_call(ctx, ShellCall::Action, platform::open_installed_apps);
            }
            Action::RunAsAdmin => {
                // Restart on the same target ("This PC", a drive or folder).
                let args = self
                    .scan
                    .as_ref()
                    .and_then(|s| s.target())
                    .map(target_args)
                    .unwrap_or_default();
                if self.script_mode {
                    // Never trigger a UAC prompt from a test script.
                    self.relaunch_args = Some(args);
                    self.toasts.info(tr.s.toast_script_skipped, now);
                    return;
                }
                // The UAC prompt blocks the calling thread until answered.
                self.shell_call(ctx, ShellCall::Relaunch, move || {
                    platform::relaunch_elevated(&args)
                });
            }
            Action::SetExplorerMenu(on) => {
                let text = match tr.lang {
                    ringscan_core::Lang::Uk => "Відкрити в RingScan",
                    ringscan_core::Lang::En => "Open in RingScan",
                };
                let r = platform::set_context_menu(on, text);
                self.report(r, None);
                // Show what the registry says now, whatever happened.
                self.explorer_menu = platform::context_menu_installed();
            }
        }
    }

    /// Runs a shell call on the shell thread; its result comes back through
    /// [`App::finish_shell_calls`].
    fn shell_call(
        &mut self,
        ctx: &Context,
        kind: ShellCall,
        f: impl FnOnce() -> io::Result<()> + Send + 'static,
    ) {
        let tx = self.shell_tx.clone();
        let ctx2 = ctx.clone();
        let queued = platform::on_shell_thread(move || {
            let _ = tx.send((kind, f()));
            ctx2.request_repaint();
        });
        if !queued {
            self.report(Err(io::Error::other("shell thread unavailable")), None);
        }
    }

    /// Reports finished shell calls; a successful "Restart as administrator"
    /// closes this window (the elevated copy has started).
    pub(super) fn finish_shell_calls(&mut self, ctx: &Context) {
        while let Ok((kind, result)) = self.shell_rx.try_recv() {
            match (kind, result) {
                (ShellCall::Relaunch, Ok(())) => ctx.send_viewport_cmd(ViewportCommand::Close),
                // The user declined the UAC prompt: nothing to report.
                (_, Err(e)) if platform::is_cancelled(&e) => {}
                (_, r) => {
                    self.report(r, None);
                }
            }
        }
    }

    /// Opens the confirmation dialog for deleting `ids`, keeping only items
    /// that have a real path and may be deleted (not the scanned root, a
    /// drive, a virtual node or an NTFS metafile).
    pub(super) fn ask(&mut self, kind: ConfirmKind, ids: Vec<NodeId>) {
        let Some(scan) = &self.scan else { return };
        if scan.is_demo() {
            return;
        }
        let t = scan.tree.read_recursive();
        let mode = self.settings.size_mode.into();
        let mut items = Vec::new();
        for id in ids {
            if !t.is_live(id) || id == t.root() {
                continue;
            }
            let n = t.node(id);
            if n.is_virtual() || n.has(flags::METAFILE) {
                continue;
            }
            let Some(path) = scan.node_path(&t, id) else {
                continue;
            };
            let is_drive = path
                .to_str()
                .and_then(ringscan_engine::lister::drive_root_letter)
                .is_some();
            if !is_drive {
                items.push((id, path, t.size(id, mode)));
            }
        }
        drop(t);
        if items.is_empty() {
            return;
        }
        let total = items.iter().map(|i| i.2).sum();
        let ids = items.iter().map(|i| i.0).collect();
        let paths = items.into_iter().map(|i| i.1).collect();
        self.confirm = Some(Confirm::new(kind, scan.id, ids, paths, total));
    }

    /// Starts a confirmed destructive action on a background thread (the
    /// shell may show its own progress or conflict dialogs); the result is
    /// applied by [`App::finish_destructive`].
    pub(super) fn run_confirmed(&mut self, ctx: &Context, c: Confirm) {
        if self.destructive.is_some() {
            let now = self.now();
            self.toasts.info(self.tr().s.toast_busy, now);
            return;
        }
        // Only the scan the items were chosen in knows their ids (a switch
        // dismisses the dialog; this is the safety net).
        let job = self
            .scan
            .as_ref()
            .filter(|s| s.id == c.scan)
            .and_then(|s| s.job().cloned());
        // The other scans in memory when the operation has finished (also
        // those started while it ran) lose the deleted items too; the scan
        // it ran in was updated by the engine.
        let jobs = self.jobs.clone();
        let except = if job.is_some() { c.scan } else { 0 };
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("ringscan-shell-op".into())
            .spawn(move || {
                let result = match (&c.kind, &job) {
                    (ConfirmKind::Recycle, Some(j)) => j.delete(&c.ids, DeleteMode::Recycle),
                    (ConfirmKind::Delete, Some(j)) => j.delete(&c.ids, DeleteMode::Permanent),
                    (ConfirmKind::EmptyBin, Some(j)) => j.empty_recycle_bin(None),
                    (ConfirmKind::EmptyBin, None) => platform::empty_recycle_bin(None),
                    (_, None) => Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "no scan to delete from",
                    )),
                };
                let others = jobs.alive(except);
                let roots_gone =
                    scans::sync_after_delete(&others, &c.kind, &c.paths, &scans::OnDisk);
                drop(others);
                let _ = tx.send((c, result, SystemInfo::query(), roots_gone));
                ctx.request_repaint();
            });
        if spawned.is_ok() {
            self.destructive = Some(rx);
        }
    }

    /// Applies the outcome of a finished destructive action (the engine and
    /// `scans::sync_after_delete` have already updated the trees) and
    /// reports it.
    pub(super) fn finish_destructive(&mut self, ctx: &Context) {
        let Some(rx) = &self.destructive else { return };
        let Ok((c, result, info, roots_gone)) = rx.try_recv() else {
            return;
        };
        self.destructive = None;
        self.apply_system_info(info);
        let tr = self.tr();
        let msg = match c.kind {
            ConfirmKind::Recycle => tr.s.toast_recycled,
            ConfirmKind::Delete | ConfirmKind::EmptyBin => tr.s.toast_deleted,
        };
        self.report(result, Some(msg));
        // A cached scan of a folder that was deleted shows nothing real.
        self.cache
            .retain(|cached| !roots_gone.contains(&cached.scan.id));
        if roots_gone.contains(&self.scan_id()) {
            self.leave_deleted_root(ctx, &c.paths);
        }
        // Deleted items are tombstones now, in whichever tree they were
        // (cached views drop theirs when restored).
        if let (Some(s), Some(sel)) = (&self.scan, self.selected)
            && !s.tree.read_recursive().is_live(sel)
        {
            self.selected = None;
        }
    }

    /// The root folder of the scan shown was deleted (the delete ran in
    /// another scan and the user switched to this one meanwhile): says so,
    /// drops this scan and shows the folder the deleted item was in, from a
    /// tree in memory if one has it, else by scanning it.
    fn leave_deleted_root(&mut self, ctx: &Context, deleted: &[PathBuf]) {
        let Some(gone) = self.scan.take() else { return };
        let root = {
            let t = gone.tree.read_recursive();
            t.name(t.root()).to_string()
        };
        // Its view holds ids of the dropped tree.
        let _ = self.take_view();
        self.focus = 0;
        drop(gone);
        let now = self.now();
        let msg = format!("{}: {root}", self.tr().s.toast_not_found);
        self.toasts.error(msg, now);
        let parent = deleted
            .iter()
            .find(|p| scans::path_within(&root, &p.to_string_lossy()))
            .and_then(|p| p.parent())
            .map(Path::to_path_buf);
        match parent {
            Some(p) => self.show(ctx, &Want::for_path(&p), || Scan::path(&p)),
            None => self.show(ctx, &Want::ThisPc, Scan::this_pc),
        }
    }
}
