//! Test-harness glue: applying [`AppCmd`]s and the JSON state dump
//! (`log <file>`), including "probe" points scripts can aim at.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use eframe::egui::{Context, Pos2, ViewportCommand};
use ringscan_core::{NodeId, flags};

use crate::actions::Action;
use crate::chart::Geometry;
use crate::harness::{self, AppCmd};
use crate::model::Mark;
use crate::settings::{self, ColorMode, LangPref, ThemePref};
use crate::theme::{self};

use super::{App, FrameStats, Hit};

impl App {
    // ----- harness ------------------------------------------------------

    pub(super) fn run_harness_cmds(&mut self, ctx: &Context) {
        let Some(h) = &mut self.harness else { return };
        let cmds = h.take_app_cmds();
        let pending = Arc::clone(&h.shots_pending);
        for cmd in cmds {
            match cmd {
                AppCmd::Resize(v) => ctx.send_viewport_cmd(ViewportCommand::InnerSize(v)),
                AppCmd::Shot(path) => {
                    let pending = Arc::clone(&pending);
                    let ctx2 = ctx.clone();
                    ctx.request_screenshot(move |img| {
                        if let Err(e) = harness::save_png(&path, &img) {
                            eprintln!("[ringscan] screenshot {} failed: {e}", path.display());
                        }
                        pending.fetch_sub(1, Ordering::AcqRel);
                        ctx2.request_repaint();
                    });
                }
                AppCmd::Lang(l) => {
                    self.settings.language = match l.as_str() {
                        "uk" => LangPref::Uk,
                        "en" => LangPref::En,
                        _ => LangPref::Auto,
                    }
                }
                AppCmd::Theme(t) => {
                    self.settings.theme = match t.as_str() {
                        "light" => ThemePref::Light,
                        "dark" => ThemePref::Dark,
                        _ => ThemePref::System,
                    };
                    theme::apply_theme_pref(ctx, self.settings.theme);
                }
                AppCmd::Colors(c) => {
                    self.settings.color_mode = match c.as_str() {
                        "types" => ColorMode::Types,
                        "age" => ColorMode::Age,
                        _ => ColorMode::Folders,
                    }
                }
                AppCmd::Zoom(p) => {
                    let id = self.scan.as_ref().and_then(|s| {
                        let t = s.tree.read_recursive();
                        if p == "/" {
                            Some(t.root())
                        } else {
                            t.find_path(&p)
                        }
                    });
                    match id {
                        Some(id) => self.actions.push(Action::Zoom(id)),
                        None => {
                            let now = self.now();
                            self.toasts
                                .error(format!("{}: {p}", self.tr().s.toast_not_found), now);
                        }
                    }
                }
                AppCmd::Select(p) => {
                    let id = self
                        .scan
                        .as_ref()
                        .and_then(|s| s.tree.read_recursive().find_path(&p));
                    match id {
                        Some(id) => self.actions.push(Action::Select(Some(id))),
                        None => {
                            let now = self.now();
                            self.toasts
                                .error(format!("{}: {p}", self.tr().s.toast_not_found), now);
                        }
                    }
                }
                AppCmd::ScanPath(p) => self.actions.push(Action::ScanPath(p)),
                AppCmd::Drive(l) => self.actions.push(Action::ShowDrive(l)),
                AppCmd::ThisPc => self.actions.push(Action::ScanThisPc),
                AppCmd::Tab(t) => self.settings.right_tab = t.min(2),
                AppCmd::Set(k, v) => {
                    let on = matches!(v.as_str(), "on" | "true" | "1" | "yes");
                    match k.as_str() {
                        "min-arc" => {
                            self.settings.min_arc_deg =
                                v.parse().unwrap_or(self.settings.min_arc_deg)
                        }
                        "depth" => {
                            self.settings.max_depth = v.parse().unwrap_or(self.settings.max_depth)
                        }
                        "free" => self.settings.show_free = on,
                        "size" => {
                            self.settings.size_mode = if v == "logical" {
                                settings::SizePref::Logical
                            } else {
                                settings::SizePref::Allocated
                            }
                        }
                        "panels" => {
                            self.settings.left_panel = on;
                            self.settings.right_panel = on;
                        }
                        _ => {
                            let now = self.now();
                            self.toasts.error(format!("set: {k}?"), now);
                        }
                    }
                    self.settings.sanitize();
                }
                AppCmd::Log(path) => {
                    let v = self.state_json(ctx);
                    let text = serde_json::to_string_pretty(&v).unwrap_or_default();
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    if let Err(e) = std::fs::write(&path, text) {
                        eprintln!("[ringscan] log {} failed: {e}", path.display());
                    }
                }
                AppCmd::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
            }
        }
    }

    /// Screen points of a few kinds of arcs (the widest of each kind) and
    /// of the confirmation dialog's buttons, so scripts can aim at them
    /// (`click @dir`): `dir` (a folder on ring 1), `file`, `free`, `rest`
    /// ("smaller items"), `center`, `ok`, `cancel`; the sidebar's
    /// `support` link, `choose-folder` button and `recycle-bin` card and,
    /// while the Support dialog is open, its `jar`,
    /// `copy-usdt`, `copy-btc`, `donate-page` and `close` buttons;
    /// `extension`: the middle of the widest extension above a virtual
    /// leaf (unaccounted, reserved or free space).
    pub(super) fn probe_points(&self) -> Vec<(&'static str, Pos2)> {
        let m = &self.model;
        let geo = Geometry::new(self.chart_rect, m.depth());
        let point_of = |pred: &dyn Fn(&crate::model::ArcInfo) -> bool| -> Option<Pos2> {
            m.arcs
                .iter()
                .filter(|a| pred(a))
                .max_by(|x, y| {
                    let w = |a: &crate::model::ArcInfo| a.span() * geo.bound(i32::from(a.depth));
                    w(x).total_cmp(&w(y))
                })
                .map(|a| {
                    let d = i32::from(a.depth);
                    geo.point((geo.bound(d - 1) + geo.bound(d)) * 0.5, a.mid())
                })
        };
        let mut out = vec![
            ("rest", point_of(&|a| a.is_rest())),
            ("dir", point_of(&|a| a.zoomable && a.depth == 1)),
            (
                "file",
                point_of(&|a| {
                    a.flags & flags::DIR == 0 && !a.is_rest() && a.flags & flags::VIRTUAL == 0
                }),
            ),
            ("free", point_of(&|a| a.flags & flags::FREE != 0)),
            ("center", Some(geo.center)),
            ("extension", {
                let outer = geo.bound(i32::from(m.depth()));
                m.extended
                    .iter()
                    .map(|&i| &m.arcs[i as usize])
                    .max_by(|x, y| x.span().total_cmp(&y.span()))
                    .map(|a| geo.point((geo.bound(i32::from(a.depth)) + outer) * 0.5, a.mid()))
            }),
        ];
        if let Some(c) = &self.confirm {
            out.push(("ok", c.ok_rect.map(|r| r.center())));
            out.push(("cancel", c.cancel_rect.map(|r| r.center())));
        }
        for &(k, r) in &self.sidebar_probes {
            out.push((k, Some(r.center())));
        }
        for &(k, r) in &self.support.probes {
            out.push((k, Some(r.center())));
        }
        out.into_iter().filter_map(|(k, p)| Some((k, p?))).collect()
    }

    /// Hands the current probe points to the harness (once per frame).
    pub(super) fn publish_probes(&mut self) {
        if self.harness.is_none() {
            return;
        }
        let probes = self
            .probe_points()
            .into_iter()
            .map(|(k, p)| (k.to_string(), p))
            .collect();
        if let Some(h) = &mut self.harness {
            h.probes = probes;
        }
    }

    pub(super) fn state_json(&self, ctx: &Context) -> serde_json::Value {
        let (focus_path, hovered, selected, tree_info) = match &self.scan {
            Some(s) => {
                let t = s.tree.read_recursive();
                let name = |id: NodeId| -> serde_json::Value {
                    if t.is_live(id) {
                        serde_json::json!({ "id": id, "name": t.name(id), "path": t.path(id) })
                    } else {
                        serde_json::Value::Null
                    }
                };
                (
                    t.path(self.focus),
                    self.chart_hover
                        .map(name)
                        .unwrap_or(serde_json::Value::Null),
                    self.selected.map(name).unwrap_or(serde_json::Value::Null),
                    serde_json::json!({
                        "nodes": t.len(), "live": t.live_len(), "generation": t.generation(),
                        "memory_mb": t.memory_bytes() as f64 / 1048576.0,
                    }),
                )
            }
            None => (
                String::new(),
                serde_json::Value::Null,
                serde_json::Value::Null,
                serde_json::Value::Null,
            ),
        };
        let m = &self.model;
        let rest = m.arcs.iter().filter(|a| a.is_rest()).count();
        let hit = match self.chart_hit {
            Some(Hit::Center) => serde_json::json!("center"),
            Some(Hit::Arc(i)) => {
                serde_json::json!({ "arc": i, "depth": m.arcs.get(i).map(|a| a.depth) })
            }
            None => serde_json::Value::Null,
        };
        let st = &self.status;
        let screen = ctx.content_rect();
        let units: Vec<serde_json::Value> = st
            .units
            .iter()
            .map(|u| {
                serde_json::json!({ "name": u.name, "method": u.method.map(|m| m.name()),
                                    "note": u.note.as_ref().map(|n| self.tr().unit_note(n)) })
            })
            .collect();
        let drives: Vec<serde_json::Value> = self
            .drives
            .iter()
            .map(|d| {
                serde_json::json!({ "letter": d.letter.to_string(), "label": d.label, "fs": d.fs,
                                    "kind": format!("{:?}", d.kind), "total": d.total, "free": d.free,
                                    "this_pc": d.this_pc })
            })
            .collect();
        // Space accounting of the scanned drives, as the engine sees it.
        let drive_space: Vec<serde_json::Value> = self
            .scan
            .as_ref()
            .and_then(|s| s.job())
            .map(|j| j.drives())
            .unwrap_or_default()
            .into_iter()
            .map(|d| {
                serde_json::json!({ "letter": d.info.letter.to_string(), "total": d.space.total,
                                    "free": d.space.free, "used": d.space.used, "scanned": d.space.scanned,
                                    "reserved": d.space.reserved, "unaccounted": d.space.unaccounted,
                                    "overcount": d.space.overcount, "method": d.method.map(|m| m.name()),
                                    "note": d.note.as_ref().map(ToString::to_string) })
            })
            .collect();
        // Scans kept in memory besides the current one, most recently used
        // first (`generation` and `elapsed` unchanged = not scanned again).
        let cache: Vec<serde_json::Value> = self
            .cache
            .iter()
            .map(|c| {
                let t = c.scan.tree.read_recursive();
                let v = &c.view;
                serde_json::json!({ "id": c.scan.id, "target": target_name(&c.scan),
                                    "running": c.scan.is_running(), "cancelled": c.scan.is_cancelled(),
                                    "files": t.files(t.root()), "live": t.live_len(), "generation": t.generation(),
                                    "memory_mb": t.memory_bytes() as f64 / 1048576.0,
                                    "focus": t.is_live(v.focus).then(|| t.path(v.focus)),
                                    "back_stack": v.back.len(),
                                    "selected": v.selected.filter(|&s| t.is_live(s)).map(|s| t.path(s)) })
            })
            .collect();
        let memory = ringscan_engine::sys::memory_info().map(|m| {
            serde_json::json!({ "peak_resident_mb": m.peak_resident as f64 / 1048576.0,
                                "peak_private_mb": m.peak_private as f64 / 1048576.0 })
        });
        let probes: serde_json::Map<String, serde_json::Value> = self
            .probe_points()
            .into_iter()
            .map(|(k, p)| (k.to_string(), serde_json::json!([p.x.round(), p.y.round()])))
            .collect();
        serde_json::json!({
            "probes": probes,
            "focus": { "id": self.focus, "path": focus_path, "name": m.center.name, "size": m.center.size,
                       "files": m.center.files, "dirs": m.center.dirs },
            "back_stack": self.back.len(),
            "hovered": hovered,
            "hit": hit,
            "selected": selected,
            "layout": { "arcs": m.arcs.len(), "rest_arcs": rest, "depth": m.depth(), "total": m.center.display,
                        "matches": m.arcs.iter().filter(|a| a.mark == Mark::Match).count() },
            "mesh": { "vertices": self.mesh.as_ref().map_or(0, |c| c.mesh.vertices.len()),
                      "triangles": self.mesh.as_ref().map_or(0, |c| c.mesh.indices.len() / 3),
                      "build_ms": self.mesh_ms, "builds_ms": FrameStats::summary(&self.frames.mesh_ms),
                      "anim_vertices": self.anim_vertices },
            "labels": self.label_count,
            "transition": self.transition.is_some(),
            "frames": { "count": self.frames.count, "cpu_ms": FrameStats::summary(&self.frames.cpu_ms),
                        "interval_ms": FrameStats::summary(&self.frames.interval_ms) },
            "scan": { "id": self.scan_id(), "target": self.scan.as_ref().map(target_name),
                      "background": self.background_scans().len(),
                      "running": st.running, "cancelled": st.cancelled, "files": st.files, "dirs": st.dirs,
                      "bytes": st.bytes, "errors": st.errors, "elapsed_s": st.elapsed.as_secs_f64(),
                      "current": st.current, "mft": st.mft, "units": units,
                      "method": format!("{:?}", self.scan.as_ref().map(|s| s.method)) },
            "system": { "drives": drives, "drive_space": drive_space, "recycle_bin": self.recycle,
                        "elevated": self.elevated, "explorer_menu": self.explorer_menu,
                        "explorer_menu_exe": crate::platform::context_menu_exe(),
                        "os_uk": self.os_uk, "relaunch_args": self.relaunch_args, "memory": memory,
                        "busy": self.destructive.is_some() },
            "tree": tree_info,
            "cache": cache,
            "search": { "text": self.search_text,
                        "hits": self.search_result().map_or(0, |r| r.0.hits.len()) },
            "settings": {
                "lang": format!("{:?}", self.tr().lang), "theme": format!("{:?}", ctx.theme()),
                "color_mode": format!("{:?}", self.settings.color_mode),
                "size_mode": format!("{:?}", self.settings.size_mode),
                "max_depth": self.settings.max_depth, "min_arc_deg": self.settings.min_arc_deg,
                "show_free": self.settings.show_free, "right_tab": self.settings.right_tab,
            },
            "window": { "width": screen.width(), "height": screen.height(), "ppp": ctx.pixels_per_point() },
            "chart_rect": [self.chart_rect.min.x, self.chart_rect.min.y, self.chart_rect.max.x, self.chart_rect.max.y],
            "font": self.ui_font,
            "toasts": self.toasts.items.iter().map(|t| t.text.clone()).collect::<Vec<_>>(),
            "clipboard": self.last_copied,
            "support": { "open": self.support.open, "opened_url": self.opened_url },
            "title": self.title,
            "confirm": self.confirm.as_ref().map(|c| format!("{:?}", c.kind)),
            "errors": self.harness.as_ref().map(|h| h.errors.clone()).unwrap_or_default(),
        })
    }
}

/// What a scan was started for, for the log ("demo" for synthetic data).
fn target_name(scan: &crate::scan_source::Scan) -> String {
    scan.target()
        .map_or_else(|| "demo".to_string(), |t| t.to_string())
}
