//! Inputs for the window chrome: drive cards, breadcrumb segments and the
//! taskbar icon.

use std::path::PathBuf;

use ringscan_core::{NodeId, Tree, flags};

use crate::panels::{Crumb, CrumbTarget, DriveCard};
use crate::platform;

use super::{App, virtual_names};

impl App {
    // ----- panels -------------------------------------------------------

    pub(super) fn drive_cards(&self) -> Vec<DriveCard> {
        let Some(scan) = &self.scan else {
            return Vec::new();
        };
        let t = scan.tree.read_recursive();
        let focus_drive = drive_of(&t, self.focus);
        if scan.is_demo() {
            // Drives of the synthetic tree: the children of a "This PC"
            // root, or the root itself.
            let roots: Vec<NodeId> = if t.node(t.root()).is_virtual() {
                t.children(t.root())
                    .filter(|&c| t.node(c).is_dir())
                    .collect()
            } else {
                vec![t.root()]
            };
            return roots
                .into_iter()
                .map(|c| {
                    let free: u64 = t
                        .children(c)
                        .filter(|&x| t.node(x).has(flags::FREE))
                        .map(|x| t.size(x, ringscan_core::SizeMode::Allocated))
                        .sum();
                    let used = t.size(c, ringscan_core::SizeMode::Allocated);
                    let letter = t.name(c).chars().next().unwrap_or('?');
                    DriveCard {
                        letter,
                        label: String::new(),
                        fs: "NTFS".into(),
                        kind: platform::DriveKind::Fixed,
                        used: Some(used),
                        total: (free > 0).then_some(used + free),
                        active: focus_drive == Some(letter),
                        this_pc: true,
                    }
                })
                .collect();
        }
        self.drives
            .iter()
            .map(|d| {
                let in_tree = t.find_path(&format!("{}:\\", d.letter));
                let used = match (d.total, d.free) {
                    (Some(total), Some(free)) => Some(total.saturating_sub(free)),
                    _ => in_tree.map(|id| t.size(id, ringscan_core::SizeMode::Allocated)),
                };
                DriveCard {
                    letter: d.letter,
                    label: d.label.clone(),
                    fs: d.fs.clone(),
                    kind: d.kind,
                    used,
                    total: d.total,
                    active: focus_drive == Some(d.letter),
                    this_pc: d.this_pc,
                }
            })
            .collect()
    }

    pub(super) fn crumbs(&self) -> Vec<Crumb> {
        let Some(scan) = &self.scan else {
            return Vec::new();
        };
        let tr = self.tr();
        let t = scan.tree.read_recursive();
        let names = virtual_names(tr);
        let mut chain: Vec<NodeId> = t.ancestors(self.focus).collect();
        chain.reverse();
        chain.push(self.focus);
        let mut out = Vec::new();
        for (k, &id) in chain.iter().enumerate() {
            let target = if id == self.focus {
                CrumbTarget::Current
            } else {
                CrumbTarget::Node(id)
            };
            if k == 0 && !t.node(id).is_virtual() {
                // A scanned folder root "C:\a\b": its ancestors are paths.
                let full = t.name(id).to_string();
                let parts: Vec<&str> = full.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
                let mut acc = String::new();
                for (j, p) in parts.iter().enumerate() {
                    if j == 0 {
                        acc = format!("{p}\\");
                    } else {
                        if !acc.ends_with('\\') {
                            acc.push('\\');
                        }
                        acc.push_str(p);
                    }
                    if j + 1 < parts.len() {
                        out.push(Crumb {
                            label: if j == 0 { acc.clone() } else { p.to_string() },
                            target: CrumbTarget::Path(PathBuf::from(&acc)),
                            is_this_pc: false,
                        });
                    } else {
                        out.push(Crumb {
                            label: if j == 0 { acc.clone() } else { p.to_string() },
                            target: target.clone(),
                            is_this_pc: false,
                        });
                    }
                }
                continue;
            }
            out.push(Crumb {
                label: crate::model::display_name(&t, id, &names),
                target,
                is_this_pc: id == t.root() && t.node(id).is_virtual(),
            });
        }
        out
    }

    pub(super) fn set_taskbar_icon(&mut self, frame: &eframe::Frame) {
        if self.taskbar_icon_done {
            return;
        }
        self.taskbar_icon_done = true;
        #[cfg(windows)]
        if let Some(w) = frame.winit_window() {
            use winit::platform::windows::WindowExtWindows;
            let size = 64;
            let rgba = crate::icon_art::render(size);
            if let Ok(icon) = winit::window::Icon::from_rgba(rgba, size, size) {
                w.set_taskbar_icon(Some(icon));
            }
        }
        #[cfg(not(windows))]
        let _ = frame;
    }
}

/// Drive letter of the drive node containing `id` ("C:\" child of a
/// virtual root, or a root named "C:\...").
fn drive_of(t: &Tree, id: NodeId) -> Option<char> {
    if !t.is_live(id) {
        return None;
    }
    let mut chain: Vec<NodeId> = t.ancestors(id).collect();
    chain.reverse();
    chain.push(id);
    let first = *chain.first()?;
    let named = if t.node(first).is_virtual() {
        *chain.get(1)?
    } else {
        first
    };
    let name = t.name(named);
    let mut cs = name.chars();
    let (l, c) = (cs.next()?, cs.next()?);
    (c == ':' && l.is_ascii_alphabetic()).then(|| l.to_ascii_uppercase())
}
