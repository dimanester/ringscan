//! User intents, and the context menu that produces most of them.
//!
//! Widgets never mutate the app directly: they return [`Action`]s, which
//! `App::apply` executes after the frame's UI code ran. That keeps borrows
//! simple (a list row does not need `&mut App`) and gives the keyboard,
//! the chart, the list and the menu one shared code path.

use std::path::PathBuf;

use eframe::egui::{self, Button, RichText, Ui};
use ringscan_core::NodeId;

use crate::i18n::Tr;
use crate::theme::{self, icon};

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Make a folder the focus (animated if related to the current one).
    Zoom(NodeId),
    Up,
    Back,
    Select(Option<NodeId>),
    Rescan(NodeId),
    CancelScan,
    /// Stop the cached scans that are still running (status bar).
    CancelBackgroundScans,
    Open(NodeId),
    Reveal(NodeId),
    CopyPath(NodeId),
    Properties(NodeId),
    /// Ask for confirmation, then move to the Recycle Bin.
    AskRecycle(Vec<NodeId>),
    /// Ask for confirmation, then delete permanently.
    AskDelete(Vec<NodeId>),
    AskEmptyBin,
    ScanThisPc,
    ScanPath(PathBuf),
    /// Show a drive: from the current tree, a scan in memory, or a new
    /// scan of it.
    ShowDrive(char),
    PickFolder,
    InstalledApps,
    RunAsAdmin,
    SetExplorerMenu(bool),
    /// Open the "Support RingScan" dialog.
    ShowSupport,
    /// Open a fixed `https://` link in the default browser.
    OpenUrl(&'static str),
    /// Copy a fixed text (a donation address) to the clipboard.
    CopyText(&'static str),
}

impl Action {
    /// Made for the tree that was current when the action was made: it
    /// carries one of its node ids or acts on its view (focus, back stack,
    /// selection) or its scan. Meaningless once another tree is shown.
    pub fn tree_bound(&self) -> bool {
        match self {
            Action::Zoom(_)
            | Action::Up
            | Action::Back
            | Action::Select(_)
            | Action::Rescan(_)
            | Action::CancelScan
            | Action::Open(_)
            | Action::Reveal(_)
            | Action::CopyPath(_)
            | Action::Properties(_)
            | Action::AskRecycle(_)
            | Action::AskDelete(_)
            | Action::RunAsAdmin => true,
            Action::AskEmptyBin
            | Action::CancelBackgroundScans
            | Action::ScanThisPc
            | Action::ScanPath(_)
            | Action::ShowDrive(_)
            | Action::PickFolder
            | Action::InstalledApps
            | Action::SetExplorerMenu(_)
            | Action::ShowSupport
            | Action::OpenUrl(_)
            | Action::CopyText(_) => false,
        }
    }
}

/// What the context menu needs to know about its target.
#[derive(Clone, Debug)]
pub struct MenuTarget {
    pub id: NodeId,
    pub zoomable: bool,
    pub is_dir: bool,
    /// Virtual node (free space, "This PC", ...): no file actions.
    pub is_virtual: bool,
    pub is_focus: bool,
}

/// What the context menu may offer besides navigation.
#[derive(Clone, Copy, Debug)]
pub struct MenuCaps {
    /// The tree mirrors real files (false for synthetic demo data, whose
    /// paths must never reach the shell).
    pub real_files: bool,
    /// A rescan can start (no scan running, real data).
    pub can_rescan: bool,
}

fn item(ui: &mut Ui, glyph: &str, text: &str, enabled: bool, shortcut: Option<&str>) -> bool {
    let pal = theme::current(ui.ctx());
    let icon = RichText::new(glyph)
        .font(theme::icon_font(14.0))
        .color(if enabled {
            pal.text_dim
        } else {
            pal.text_faint
        });
    let mut b = Button::new((icon, text)).frame_when_inactive(false);
    if let Some(s) = shortcut {
        b = b.shortcut_text(RichText::new(s).color(pal.text_faint).size(12.0));
    }
    ui.add_enabled(enabled, b).clicked()
}

/// Fills a context menu for `t`; returns the chosen action.
pub fn node_menu(ui: &mut Ui, tr: Tr, t: &MenuTarget, caps: MenuCaps) -> Option<Action> {
    let s = tr.s;
    ui.set_min_width(250.0);
    let real = !t.is_virtual && caps.real_files;
    let can_rescan = caps.can_rescan;
    let mut out = None;
    if t.is_dir
        && item(
            ui,
            icon::ZOOM_IN,
            s.menu_zoom,
            t.zoomable && !t.is_focus,
            None,
        )
    {
        out = Some(Action::Zoom(t.id));
    }
    if item(ui, icon::OPEN, s.menu_open, real, None) {
        out = Some(Action::Open(t.id));
    }
    if item(ui, icon::EXPLORER, s.menu_reveal, real, None) {
        out = Some(Action::Reveal(t.id));
    }
    if item(ui, icon::COPY, s.menu_copy_path, real, Some("Ctrl+C")) {
        out = Some(Action::CopyPath(t.id));
    }
    if t.is_dir
        && item(
            ui,
            icon::REFRESH,
            s.menu_rescan,
            real && can_rescan,
            Some("F5"),
        )
    {
        out = Some(Action::Rescan(t.id));
    }
    ui.separator();
    if item(ui, icon::DELETE, s.menu_recycle, real, Some("Del")) {
        out = Some(Action::AskRecycle(vec![t.id]));
    }
    {
        let pal = theme::current(ui.ctx());
        let glyph = RichText::new(icon::DELETE)
            .font(theme::icon_font(14.0))
            .color(pal.danger);
        let text =
            RichText::new(s.menu_delete).color(if real { pal.danger } else { pal.text_faint });
        let b = Button::new((glyph, text))
            .frame_when_inactive(false)
            .shortcut_text(RichText::new("Shift+Del").color(pal.text_faint).size(12.0));
        if ui.add_enabled(real, b).clicked() {
            out = Some(Action::AskDelete(vec![t.id]));
        }
    }
    ui.separator();
    if item(ui, icon::INFO, s.menu_properties, real, None) {
        out = Some(Action::Properties(t.id));
    }
    if out.is_some() {
        ui.close();
    }
    out
}

/// Shows `node_menu` as the context menu of `resp`.
pub fn attach_menu(
    resp: &egui::Response,
    tr: Tr,
    t: Option<&MenuTarget>,
    caps: MenuCaps,
) -> Option<Action> {
    let t = t?;
    let mut out = None;
    resp.context_menu(|ui| {
        out = node_menu(ui, tr, t, caps);
    });
    out
}
