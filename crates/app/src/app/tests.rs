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

/// Auto (the default): the ring count follows the chart's size and color
/// mode, the model is rebuilt only when the count changes, a change
/// animates from the old rings to the new ones, an animation keeps its
/// rings until it ends (and a resize meanwhile then animates too), and a
/// fixed count ignores the window.
#[test]
fn ring_count_follows_the_window_and_is_cached_by_count() {
    use crate::chart;
    use crate::settings::{ColorMode, RingMode};

    let mut rig = Rig::new(1280.0, 640.0);
    let rings = |rig: &Rig| rig.app.model.key.params.max_depth;
    let serial = |rig: &Rig| rig.app.model.serial;
    let auto =
        |rig: &Rig, mode| chart::auto_rings(rig.app.chart_rect, chart::auto_ring_min_width(mode));
    // Animations run on the app's own clock. A zoom that ends with a new
    // ring count is followed by the ring animation.
    let settle = |rig: &mut Rig| {
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_secs_f32(chart::ZOOM_SECS + 0.05));
            rig.frames(1);
            if rig.app.transition.is_none() {
                return;
            }
        }
        panic!("still animating");
    };
    assert_eq!(rig.app.settings.ring_mode(), RingMode::Auto);
    let small = rings(&rig);
    assert_eq!(small, auto(&rig, ColorMode::Folders));
    assert!((7..=9).contains(&small), "{small} rings at 1280x640");
    // The first count (the chart's size was not known yet) did not animate.
    assert!(rig.app.transition.is_none());

    // A few points more or less: same count, same model.
    let s = serial(&rig);
    for h in [643.0, 637.0, 641.0] {
        rig.size = vec2(1280.0, h);
        rig.frames(2);
        assert_eq!((rings(&rig), serial(&rig)), (small, s), "height {h}");
    }

    // Full HD: more rings, applied in the frame that saw the new size, and
    // animated (from the old chart, which was on screen).
    rig.size = vec2(1920.0, 1080.0);
    rig.frames(1);
    let big = rings(&rig);
    assert!(big >= small + 3, "{big} rings at 1920x1080");
    assert_eq!(big, auto(&rig, ColorMode::Folders));
    assert_ne!(serial(&rig), s);
    let t = rig.app.transition.as_ref().expect("animated");
    assert_eq!(
        (t.outer.key.params.max_depth, t.inner.key.params.max_depth),
        (small, big)
    );
    // While it runs the count stays; the resize applies when it ends,
    // animated again.
    rig.size = vec2(1280.0, 640.0);
    rig.frames(1);
    assert_eq!(rings(&rig), big, "re-laid out mid-animation");
    std::thread::sleep(std::time::Duration::from_secs_f32(chart::ZOOM_SECS + 0.05));
    rig.frames(1);
    assert_eq!(rings(&rig), small);
    let t = rig.app.transition.as_ref().expect("animated");
    assert!(!t.inward && t.inner.key.params.max_depth == big);
    settle(&mut rig);
    assert_eq!(rings(&rig), small);

    // A zoom keeps its two models' rings while it animates; the resize
    // applies when it ends.
    let drive = {
        let t = rig.app.scan.as_ref().unwrap().tree.read();
        t.children(t.root()).find(|&c| t.node(c).is_dir()).unwrap()
    };
    rig.app.actions.push(crate::actions::Action::Zoom(drive));
    rig.frames(1);
    assert!(rig.app.transition.is_some());
    rig.size = vec2(1920.0, 1080.0);
    rig.frames(1);
    assert!(rig.app.transition.is_some(), "the test ran too slowly");
    assert_eq!(rings(&rig), small, "re-laid out mid-animation");
    settle(&mut rig);
    assert_eq!(rings(&rig), big);
    assert_eq!(rig.app.focus, drive);

    // File types and age: more, thinner rings (colors carry the
    // information there).
    rig.app.settings.color_mode = ColorMode::Types;
    rig.frames(1);
    settle(&mut rig);
    let types = rings(&rig);
    assert_eq!(types, auto(&rig, ColorMode::Types));
    assert!(types > big, "{types} rings by type, {big} by folder");
    rig.app.settings.color_mode = ColorMode::Folders;
    rig.frames(1);
    settle(&mut rig);
    assert_eq!(rings(&rig), big);

    // Fixed: the window does not matter.
    rig.app.settings.set_ring_mode(RingMode::Fixed(12));
    rig.frames(1);
    settle(&mut rig);
    assert_eq!(rings(&rig), 12);
    rig.size = vec2(1280.0, 640.0);
    rig.frames(1);
    assert_eq!(rings(&rig), 12);
    assert!(rig.app.transition.is_none());
    rig.app.settings.set_ring_mode(RingMode::Auto);
    rig.frames(1);
    settle(&mut rig);
    assert_eq!(rings(&rig), small);
}

/// At the window's minimum size Auto draws few rings, wide enough for names.
#[test]
fn the_smallest_window_has_room_for_names() {
    let mut rig = Rig::new(760.0, 520.0);
    assert_eq!(rig.app.model.key.params.max_depth, 3);
    let geo = crate::chart::Geometry::of(rig.app.chart_rect, &rig.app.model);
    assert_eq!(geo.bounds.len(), 4);
    rig.frames(2);
    assert!(rig.app.label_count >= 4, "{} labels", rig.app.label_count);
}
