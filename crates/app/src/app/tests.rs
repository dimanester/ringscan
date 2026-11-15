//! Whole-frame tests of the window: the real `App` on synthetic demo data,
//! driven headless through `egui::Context::run_ui` (no GPU, no window).

use eframe::egui::{self, Context, Event, Key, Modifiers, Pos2, RawInput, Rect, vec2};
use ringscan_core::NodeId;

use super::{App, Options};
use crate::actions::Action;

/// An `App` showing demo data in a window of `size` points.
struct Rig {
    ctx: Context,
    app: App,
    frame: eframe::Frame,
    size: egui::Vec2,
    time: f64,
}

impl Rig {
    fn new(w: f32, h: f32) -> Rig {
        let ctx = Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let app = App::new(
            &cc,
            Options {
                demo: Some(3_000),
                ..Options::default()
            },
        );
        let mut rig = Rig {
            ctx,
            app,
            frame: eframe::Frame::_new_kittest(),
            size: vec2(w, h),
            time: 0.0,
        };
        rig.frames(3);
        rig
    }

    /// Runs one frame with `events`; returns the text it put on the
    /// clipboard, if any.
    fn step(&mut self, events: Vec<Event>) -> Option<String> {
        // Past every animation (zoom transitions take 220 ms).
        self.time += 0.5;
        let raw = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)),
            time: Some(self.time),
            events,
            ..RawInput::default()
        };
        let Rig {
            ctx, app, frame, ..
        } = self;
        let mut out = ctx.run_ui(raw, |ui| eframe::App::ui(app, ui, frame));
        // Nothing renders: drop the texture uploads.
        out.textures_delta.clear();
        out.platform_output
            .commands
            .into_iter()
            .find_map(|c| match c {
                egui::OutputCommand::CopyText(t) => Some(t),
                _ => None,
            })
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            self.step(Vec::new());
        }
    }

    fn act(&mut self, a: Action) {
        self.app.actions.push(a);
        self.frames(2);
    }

    /// Zooms into a drive and one of its folders (so Back has somewhere to
    /// go) and selects a file in that folder; returns the file.
    fn zoom_and_select(&mut self) -> NodeId {
        let (drive, dir, file) = {
            let t = self.app.scan.as_ref().expect("demo scan").tree.read();
            let drive = t
                .children(t.root())
                .find(|&c| t.node(c).is_dir())
                .expect("a drive");
            let dir = t
                .children(drive)
                .find(|&c| t.node(c).is_dir() && t.children(c).any(|f| !t.node(f).is_dir()))
                .expect("a folder with files");
            let file = t
                .children(dir)
                .find(|&f| !t.node(f).is_dir())
                .expect("a file");
            (drive, dir, file)
        };
        self.act(Action::Zoom(drive));
        self.act(Action::Zoom(dir));
        self.act(Action::Select(Some(file)));
        assert_eq!(self.app.focus, dir);
        assert_eq!(self.app.selected, Some(file));
        file
    }
}

fn key(key: Key, modifiers: Modifiers) -> Event {
    Event::Key {
        key,
        physical_key: Some(key),
        pressed: true,
        repeat: false,
        modifiers,
    }
}

fn screen(rig: &Rig) -> Rect {
    Rect::from_min_size(Pos2::ZERO, rig.size)
}

#[test]
fn shortcuts_stand_down_while_the_support_dialog_is_open() {
    let mut rig = Rig::new(1280.0, 800.0);
    let file = rig.zoom_and_select();
    let (focus, back, scan) = (rig.app.focus, rig.app.back.len(), rig.app.scan_id());

    // What the shortcuts alone ask for (demo data refuses file actions
    // later, so the whole frame cannot show Delete and Ctrl+C).
    let shortcut_actions = |rig: &mut Rig| {
        rig.time += 0.5;
        let raw = RawInput {
            screen_rect: Some(screen(rig)),
            time: Some(rig.time),
            events: vec![
                key(Key::Delete, Modifiers::NONE),
                Event::Copy,
                key(Key::Backspace, Modifiers::NONE),
                key(Key::F5, Modifiers::NONE),
                key(Key::Escape, Modifiers::NONE),
            ],
            ..RawInput::default()
        };
        let Rig { ctx, app, .. } = rig;
        ctx.run_ui(raw, |ui| app.shortcuts(ui.ctx()))
            .textures_delta
            .clear();
        std::mem::take(&mut rig.app.actions)
    };
    // Control: with no dialog open the keys do act (and Esc clears the
    // selection); restore it.
    let acts = shortcut_actions(&mut rig);
    assert!(acts.contains(&Action::AskRecycle(vec![file])), "{acts:?}");
    assert!(acts.contains(&Action::CopyPath(file)), "{acts:?}");
    assert!(acts.contains(&Action::Back), "{acts:?}");
    assert!(
        acts.contains(&Action::Rescan(ringscan_core::NONE)),
        "{acts:?}"
    );
    assert_eq!(rig.app.selected, None);
    rig.app.selected = Some(file);

    rig.act(Action::ShowSupport);
    assert!(rig.app.support.open);
    assert_eq!(shortcut_actions(&mut rig), Vec::new());
    assert_eq!(rig.app.selected, Some(file));
    let copied = rig.step(vec![
        key(Key::Delete, Modifiers::NONE),
        Event::Copy,
        key(Key::C, Modifiers::COMMAND),
        key(Key::Backspace, Modifiers::NONE),
        key(Key::ArrowLeft, Modifiers::ALT),
        key(Key::F5, Modifiers::NONE),
        key(Key::Delete, Modifiers::SHIFT),
    ]);
    rig.frames(2);
    assert!(rig.app.confirm.is_none(), "Delete asked to recycle");
    assert_eq!(copied, None, "Ctrl+C copied the selected path");
    assert!(rig.app.toasts.items.is_empty(), "a toast appeared");
    assert_eq!(rig.app.focus, focus, "Backspace navigated");
    assert_eq!(rig.app.back.len(), back);
    assert_eq!(rig.app.scan_id(), scan, "F5 rescanned");
    assert!(rig.app.support.open);

    // Esc closes the dialog only: the selection stays.
    rig.step(vec![key(Key::Escape, Modifiers::NONE)]);
    rig.frames(1);
    assert!(!rig.app.support.open, "Esc did not close the dialog");
    assert_eq!(
        rig.app.selected,
        Some(file),
        "Esc also cleared the selection"
    );
    assert_eq!(rig.app.focus, focus);
}

/// The dialog and its Close button fit the window at its minimum size
/// (760 x 520), and at full HD everything shows without scrolling.
#[test]
fn support_dialog_fits_the_window() {
    for (w, h) in [(760.0, 520.0), (1366.0, 600.0), (1920.0, 1080.0)] {
        let mut rig = Rig::new(w, h);
        rig.act(Action::ShowSupport);
        rig.frames(2);
        let screen = screen(&rig);
        let dialog = rig.app.support.rect.expect("dialog drawn");
        assert!(screen.contains_rect(dialog), "{w}x{h}: {dialog:?}");
        let probes = &rig.app.support.probes;
        for name in ["jar", "copy-usdt", "copy-btc", "donate-page", "close"] {
            let r = probes
                .iter()
                .find(|(k, _)| *k == name)
                .map(|&(_, r)| r)
                .unwrap_or_else(|| panic!("{w}x{h}: no {name} probe"));
            assert!(
                dialog.contains_rect(r),
                "{w}x{h}: {name} {r:?} outside {dialog:?}"
            );
        }
    }
}

/// "Choose folder…" never sits under the Recycle Bin card or the other
/// bottom items (the demo has two drives; `panels` tests cover four).
#[test]
fn sidebar_blocks_do_not_overlap_in_a_low_window() {
    let mut rig = Rig::new(760.0, 520.0);
    rig.frames(2);
    let probe = |name: &str| {
        rig.app
            .sidebar_probes
            .iter()
            .find(|(k, _)| *k == name)
            .map(|&(_, r)| r)
            .unwrap_or_else(|| panic!("no {name} probe"))
    };
    let (pick, bin, link) = (
        probe("choose-folder"),
        probe("recycle-bin"),
        probe("support"),
    );
    assert!(pick.bottom() <= bin.top(), "{pick:?} over {bin:?}");
    assert!(bin.bottom() <= link.top());
    assert!(screen(&rig).contains_rect(link));
}

/// Demo data shows its dates as stored (no machine time zone), so
/// screenshots of the demo do not depend on the machine.
#[test]
fn demo_dates_ignore_the_time_zone() {
    let rig = Rig::new(800.0, 600.0);
    assert_eq!((rig.app.local_time_fn())(1_794_695_400), 1_794_695_400);
}

/// A delete confirmed after a rescan replaced the item (its id is now a
/// tombstone) runs nothing and says so, instead of reporting a success
/// while the file stays on disk.
#[test]
fn a_delete_confirmed_after_a_rescan_replaced_the_item_does_nothing() {
    let dir = std::env::temp_dir()
        .join("ringscan-dev")
        .join(format!("app-stale-confirm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let file = dir.join("sub").join("doomed.txt");
    std::fs::write(&file, b"keep me").unwrap();

    let ctx = Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = App::new(
        &cc,
        Options {
            path: Some(dir.clone()),
            ..Options::default()
        },
    );
    let wait = |app: &App| {
        let started = std::time::Instant::now();
        while app.scan.as_ref().is_some_and(|s| s.is_running()) {
            assert!(started.elapsed().as_secs() < 30, "scan did not finish");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    wait(&app);
    let (sub, id) = {
        let s = app.scan.as_ref().expect("a scan");
        let t = s.tree.read_recursive();
        let sub = t.find_child(t.root(), "sub").expect("sub");
        (sub, t.find_child(sub, "doomed.txt").expect("file"))
    };
    app.ask(crate::dialogs::ConfirmKind::Delete, vec![id]);
    let c = app.confirm.take().expect("confirmation dialog");
    assert!(app.confirmed_items_unchanged(&c));

    // The folder is rescanned while the dialog is open.
    app.scan.as_mut().unwrap().rescan(sub).unwrap();
    wait(&app);
    assert!(!app.scan.as_ref().unwrap().tree.read_recursive().is_live(id));
    assert!(!app.confirmed_items_unchanged(&c));

    let toasts = app.toasts.items.len();
    app.run_confirmed(&ctx, c);
    assert!(app.destructive.is_none(), "nothing was started");
    let last = app.toasts.items.last().expect("a toast");
    assert_eq!(app.toasts.items.len(), toasts + 1);
    assert!(last.error);
    assert_eq!(last.text, app.tr().s.toast_item_changed);
    assert!(file.exists());
    drop(app);
    std::fs::remove_dir_all(&dir).unwrap();
}
