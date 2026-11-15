//! UI test harness: `ringscan.exe --script "cmd; cmd; ..."`.
//!
//! The script drives the real app through egui's input pipeline, so tests
//! exercise exactly what a user would: [`Harness::drive`] runs in
//! `App::raw_input_hook` before every frame, removes the OS's own mouse,
//! keyboard and focus events (the window may pop up under the user's
//! cursor) and injects the scripted ones instead. Commands that are not
//! input (screenshots, settings, state dumps) are handed to the app as
//! [`AppCmd`]s, which it applies at the start of the same frame. The app
//! keeps repainting continuously while a script runs.
//!
//! Commands (coordinates in egui points, i.e. pixels at 100 % scaling; a
//! pointer target can also be `@name`, a probe point the app publishes every
//! frame: `@dir`, `@file`, `@free`, `@rest`, `@center`, and `@ok` /
//! `@cancel` while a confirmation dialog is open):
//!
//! | command | effect |
//! |---|---|
//! | `wait <sec>` | let frames run |
//! | `wait-scan [timeout]` | until the scan finishes (default 120 s) |
//! | `size <w> <h>` | resize the window's inner area |
//! | `move <x> <y>` / `move @probe` | move the pointer |
//! | `click <x> <y>\|@probe [left\|right\|middle\|back]` | press + release |
//! | `dblclick <x> <y>\|@probe` | double click |
//! | `scroll <dy>` | mouse wheel, points (positive = up) |
//! | `key <Name> [ctrl] [shift] [alt]` | egui key name (`Escape`, `F5`, `Delete`, `A`, `ArrowLeft`, ...) |
//! | `type <text>` | text input |
//! | `shot <file.png>` | screenshot (waits until written) |
//! | `lang uk\|en\|auto`, `theme light\|dark\|system`, `colors folders\|types\|age` | settings |
//! | `zoom <path>` | focus a folder by path (or `/` for the root) |
//! | `select <path>` | select a node by path (then `key Delete` asks to recycle it) |
//! | `scan <path>` | what "Choose folder…" does with the folder (a tree in memory that has it, else a new scan) |
//! | `drive <X>` / `this-pc` | what a click on that sidebar card does |
//! | `tab <0..2>` | right panel tab |
//! | `set <key> <value>` | `min-arc <deg>`, `depth <n>`, `free on\|off`, `size disk\|logical`, `panels on\|off` |
//! | `log <file.json>` | dump state as JSON |
//! | `quit` | close the window |
//!
//! A script that is still running after [`MAX_RUNTIME`] quits on its own.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use eframe::egui::{
    Event, Key, Modifiers, MouseWheelSource, MouseWheelUnit, PointerButton, Pos2, RawInput,
    TouchPhase, Vec2, pos2, vec2,
};

/// Safety net: a script never runs longer than this (seconds).
pub const MAX_RUNTIME: f64 = 600.0;
/// A screenshot that does not arrive within this time (minimized window)
/// is reported in the log and skipped.
const SHOT_TIMEOUT: f64 = 10.0;

/// Where a pointer command goes.
#[derive(Clone, Debug, PartialEq)]
pub enum At {
    Point(Pos2),
    /// A named probe point published by the app (see the module docs).
    Probe(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    Wait(f64),
    WaitScan(f64),
    Size(Vec2),
    Move(At),
    Click(At, PointerButton),
    DblClick(At),
    Scroll(f32),
    Key(Key, Modifiers),
    Type(String),
    Shot(PathBuf),
    Lang(String),
    Theme(String),
    Colors(String),
    Zoom(String),
    Select(String),
    ScanPath(PathBuf),
    Drive(char),
    ThisPc,
    Tab(usize),
    Set(String, String),
    Log(PathBuf),
    Quit,
}

/// Non-input commands, applied by the app.
#[derive(Clone, Debug, PartialEq)]
pub enum AppCmd {
    Resize(Vec2),
    Shot(PathBuf),
    Lang(String),
    Theme(String),
    Colors(String),
    Zoom(String),
    Select(String),
    ScanPath(PathBuf),
    Drive(char),
    ThisPc,
    Tab(usize),
    Set(String, String),
    Log(PathBuf),
    Quit,
}

pub struct Harness {
    cmds: Vec<Cmd>,
    idx: usize,
    /// Sub-step of a multi-frame command (click = press, release).
    step: u32,
    wait_until: Option<f64>,
    /// Screenshots requested but not yet written.
    pub shots_pending: Arc<AtomicUsize>,
    awaiting_shots: bool,
    shot_wait_since: Option<f64>,
    out: Vec<AppCmd>,
    started: Option<f64>,
    pub errors: Vec<String>,
    pub pointer: Option<Pos2>,
    /// Probe points of the last frame, set by the app.
    pub probes: HashMap<String, Pos2>,
}

fn parse_f32(s: Option<&str>, what: &str) -> Result<f32, String> {
    s.ok_or_else(|| format!("missing {what}"))?
        .parse::<f32>()
        .map_err(|e| format!("bad {what}: {e}"))
}

fn key_by_name(name: &str) -> Option<Key> {
    let alias = match name.to_ascii_lowercase().as_str() {
        "esc" => "Escape",
        "del" => "Delete",
        "left" => "ArrowLeft",
        "right" => "ArrowRight",
        "up" => "ArrowUp",
        "down" => "ArrowDown",
        "return" => "Enter",
        _ => name,
    };
    Key::from_name(alias).or_else(|| {
        // Single letters are case-insensitive.
        let up = alias.to_ascii_uppercase();
        Key::from_name(&up)
    })
}

/// Parses a pointer target (`x y` or `@probe`) from the start of `tokens`;
/// returns it and the number of tokens used.
fn parse_at(tokens: &[&str]) -> Result<(At, usize), String> {
    match tokens.first() {
        Some(t) if t.starts_with('@') && t.len() > 1 => Ok((At::Probe(t[1..].to_string()), 1)),
        _ => {
            let x = parse_f32(tokens.first().copied(), "x")?;
            let y = parse_f32(tokens.get(1).copied(), "y")?;
            Ok((At::Point(pos2(x, y)), 2))
        }
    }
}

/// Parses one command; `rest` is the text after the command name.
fn parse_cmd(line: &str) -> Result<Cmd, String> {
    let line = line.trim();
    let (name, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let mut args = rest.split_whitespace();
    let mut f = |what: &str| parse_f32(args.next(), what);
    Ok(match name.to_ascii_lowercase().as_str() {
        "wait" => Cmd::Wait(f64::from(f("seconds")?)),
        "wait-scan" => Cmd::WaitScan(if rest.is_empty() {
            120.0
        } else {
            f64::from(f("timeout")?)
        }),
        "size" => Cmd::Size(vec2(f("width")?, f("height")?)),
        "move" => Cmd::Move(parse_at(&tokens)?.0),
        "click" => {
            let (p, used) = parse_at(&tokens)?;
            let button = match tokens.get(used).copied().unwrap_or("left") {
                "left" => PointerButton::Primary,
                "right" => PointerButton::Secondary,
                "middle" => PointerButton::Middle,
                "back" => PointerButton::Extra1,
                "forward" => PointerButton::Extra2,
                b => return Err(format!("unknown button {b}")),
            };
            Cmd::Click(p, button)
        }
        "dblclick" => Cmd::DblClick(parse_at(&tokens)?.0),
        "scroll" => Cmd::Scroll(f("dy")?),
        "key" => {
            let mut it = rest.split_whitespace();
            let kname = it.next().ok_or("missing key name")?;
            let key = key_by_name(kname).ok_or_else(|| format!("unknown key {kname}"))?;
            let mut m = Modifiers::NONE;
            for modifier in it {
                match modifier.to_ascii_lowercase().as_str() {
                    "ctrl" => {
                        m.ctrl = true;
                        m.command = true;
                    }
                    "shift" => m.shift = true,
                    "alt" => m.alt = true,
                    other => return Err(format!("unknown modifier {other}")),
                }
            }
            Cmd::Key(key, m)
        }
        "type" => Cmd::Type(rest.to_string()),
        "shot" if !rest.is_empty() => Cmd::Shot(PathBuf::from(rest)),
        "lang" => Cmd::Lang(rest.to_ascii_lowercase()),
        "theme" => Cmd::Theme(rest.to_ascii_lowercase()),
        "colors" => Cmd::Colors(rest.to_ascii_lowercase()),
        "zoom" if !rest.is_empty() => Cmd::Zoom(rest.to_string()),
        "select" if !rest.is_empty() => Cmd::Select(rest.to_string()),
        "scan" if !rest.is_empty() => Cmd::ScanPath(PathBuf::from(rest)),
        "drive" => {
            let mut cs = rest.chars();
            match (cs.next(), cs.next(), cs.next()) {
                (Some(l), None | Some(':'), None) if l.is_ascii_alphabetic() => {
                    Cmd::Drive(l.to_ascii_uppercase())
                }
                _ => return Err(format!("bad drive letter: {rest}")),
            }
        }
        "this-pc" => Cmd::ThisPc,
        "tab" => Cmd::Tab(rest.parse().map_err(|e| format!("bad tab: {e}"))?),
        "log" if !rest.is_empty() => Cmd::Log(PathBuf::from(rest)),
        "set" => {
            let (k, v) = rest
                .split_once(char::is_whitespace)
                .ok_or("set needs a key and a value")?;
            Cmd::Set(k.to_ascii_lowercase(), v.trim().to_ascii_lowercase())
        }
        "quit" | "exit" => Cmd::Quit,
        _ => return Err(format!("unknown command: {line}")),
    })
}

/// Real user input that the harness replaces with scripted input.
fn is_user_input(e: &Event) -> bool {
    matches!(
        e,
        Event::PointerMoved(_)
            | Event::MouseMoved(_)
            | Event::PointerButton { .. }
            | Event::PointerGone
            | Event::MouseWheel { .. }
            | Event::Key { .. }
            | Event::Text(_)
            | Event::Copy
            | Event::Cut
            | Event::Paste(_)
            | Event::ModifiersChanged(_)
            | Event::WindowFocused(_)
            | Event::Zoom(_)
            | Event::Touch { .. }
    )
}

impl Harness {
    pub fn parse(script: &str) -> Result<Harness, String> {
        let cmds = script
            .split([';', '\n'])
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| parse_cmd(l).map_err(|e| format!("{e} (in `{l}`)")))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Harness {
            cmds,
            idx: 0,
            step: 0,
            wait_until: None,
            shots_pending: Arc::new(AtomicUsize::new(0)),
            awaiting_shots: false,
            shot_wait_since: None,
            out: Vec::new(),
            started: None,
            errors: Vec::new(),
            pointer: None,
            probes: HashMap::new(),
        })
    }

    pub fn finished(&self) -> bool {
        self.idx >= self.cmds.len()
    }

    /// Commands for the app, collected by [`Self::drive`] this frame.
    pub fn take_app_cmds(&mut self) -> Vec<AppCmd> {
        std::mem::take(&mut self.out)
    }

    /// Screen point of a pointer target. A probe is resolved once per
    /// command (when its first step runs) and then kept, so a multi-frame
    /// click stays where it started even if the chart moves.
    fn resolve(&mut self, at: &At) -> Option<Pos2> {
        match at {
            At::Point(p) => Some(*p),
            At::Probe(_) if self.step > 0 => self.pointer,
            At::Probe(name) => {
                let p = self.probes.get(name).copied();
                if p.is_none() {
                    self.errors.push(format!("probe @{name} not available"));
                }
                p
            }
        }
    }

    fn next(&mut self) {
        self.idx += 1;
        self.step = 0;
        self.wait_until = None;
    }

    /// Replaces the frame's user input with scripted input. Call once per
    /// frame from `raw_input_hook`; `now` is the frame time in seconds.
    pub fn drive(&mut self, raw: &mut RawInput, now: f64, scan_done: bool) {
        raw.events.retain(|e| !is_user_input(e));
        raw.focused = true;
        let started = *self.started.get_or_insert(now);
        if now - started > MAX_RUNTIME && !self.finished() {
            self.errors.push(format!("script exceeded {MAX_RUNTIME} s"));
            self.out.push(AppCmd::Quit);
            self.idx = self.cmds.len();
            return;
        }
        if self.awaiting_shots {
            let since = *self.shot_wait_since.get_or_insert(now);
            if self.shots_pending.load(Ordering::Acquire) > 0 && now - since < SHOT_TIMEOUT {
                return;
            }
            if now - since >= SHOT_TIMEOUT {
                self.errors
                    .push(format!("screenshot not delivered within {SHOT_TIMEOUT} s"));
            }
            self.awaiting_shots = false;
            self.shot_wait_since = None;
        }
        // Run commands until one needs to wait for the next frame.
        while let Some(cmd) = self.cmds.get(self.idx).cloned() {
            match cmd {
                Cmd::Wait(s) => {
                    let until = *self.wait_until.get_or_insert(now + s);
                    if now < until {
                        return;
                    }
                    self.next();
                }
                Cmd::WaitScan(t) => {
                    let until = *self.wait_until.get_or_insert(now + t);
                    if !scan_done && now < until {
                        return;
                    }
                    self.next();
                }
                Cmd::Move(at) => {
                    let Some(p) = self.resolve(&at) else {
                        self.next();
                        continue;
                    };
                    raw.events.push(Event::PointerMoved(p));
                    self.pointer = Some(p);
                    self.next();
                    return;
                }
                Cmd::Click(at, button) => {
                    let Some(p) = self.resolve(&at) else {
                        self.next();
                        continue;
                    };
                    let pressed = self.step == 0;
                    if pressed {
                        raw.events.push(Event::PointerMoved(p));
                        self.pointer = Some(p);
                    }
                    raw.events.push(Event::PointerButton {
                        pos: p,
                        button,
                        pressed,
                        modifiers: Modifiers::NONE,
                    });
                    if pressed {
                        self.step = 1;
                    } else {
                        self.next();
                    }
                    return;
                }
                Cmd::DblClick(at) => {
                    let Some(p) = self.resolve(&at) else {
                        self.next();
                        continue;
                    };
                    if self.step == 0 {
                        raw.events.push(Event::PointerMoved(p));
                        self.pointer = Some(p);
                    }
                    raw.events.push(Event::PointerButton {
                        pos: p,
                        button: PointerButton::Primary,
                        pressed: self.step.is_multiple_of(2),
                        modifiers: Modifiers::NONE,
                    });
                    self.step += 1;
                    if self.step == 4 {
                        self.next();
                    }
                    return;
                }
                Cmd::Scroll(dy) => {
                    raw.events.push(Event::MouseWheel {
                        unit: MouseWheelUnit::Point,
                        delta: vec2(0.0, dy),
                        phase: TouchPhase::Move,
                        source: MouseWheelSource::Wheel,
                        modifiers: Modifiers::NONE,
                    });
                    self.next();
                    return;
                }
                Cmd::Key(key, m) => {
                    let pressed = self.step == 0;
                    if pressed && m != Modifiers::NONE {
                        raw.events.push(Event::ModifiersChanged(m));
                    }
                    // winit turns Ctrl+C/X into clipboard events; mirror it.
                    if pressed && m.command && !m.shift && !m.alt {
                        match key {
                            Key::C => raw.events.push(Event::Copy),
                            Key::X => raw.events.push(Event::Cut),
                            _ => {}
                        }
                    }
                    raw.events.push(Event::Key {
                        key,
                        physical_key: Some(key),
                        pressed,
                        repeat: false,
                        modifiers: m,
                    });
                    if pressed {
                        self.step = 1;
                    } else {
                        if m != Modifiers::NONE {
                            raw.events.push(Event::ModifiersChanged(Modifiers::NONE));
                        }
                        self.next();
                    }
                    return;
                }
                Cmd::Type(text) => {
                    raw.events.push(Event::Text(text));
                    self.next();
                    return;
                }
                Cmd::Size(v) => {
                    self.out.push(AppCmd::Resize(v));
                    self.next();
                }
                Cmd::Shot(p) => {
                    self.shots_pending.fetch_add(1, Ordering::AcqRel);
                    self.out.push(AppCmd::Shot(p));
                    self.awaiting_shots = true;
                    self.next();
                    return;
                }
                Cmd::Lang(s) => {
                    self.out.push(AppCmd::Lang(s));
                    self.next();
                }
                Cmd::Theme(s) => {
                    self.out.push(AppCmd::Theme(s));
                    self.next();
                }
                Cmd::Colors(s) => {
                    self.out.push(AppCmd::Colors(s));
                    self.next();
                }
                Cmd::Zoom(s) => {
                    self.out.push(AppCmd::Zoom(s));
                    self.next();
                }
                Cmd::Select(s) => {
                    self.out.push(AppCmd::Select(s));
                    self.next();
                    return;
                }
                // One per frame, like the clicks they stand for.
                Cmd::ScanPath(p) => {
                    self.out.push(AppCmd::ScanPath(p));
                    self.next();
                    return;
                }
                Cmd::Drive(l) => {
                    self.out.push(AppCmd::Drive(l));
                    self.next();
                    return;
                }
                Cmd::ThisPc => {
                    self.out.push(AppCmd::ThisPc);
                    self.next();
                    return;
                }
                Cmd::Tab(t) => {
                    self.out.push(AppCmd::Tab(t));
                    self.next();
                }
                Cmd::Set(k, v) => {
                    self.out.push(AppCmd::Set(k, v));
                    self.next();
                }
                Cmd::Log(p) => {
                    self.out.push(AppCmd::Log(p));
                    self.next();
                    return;
                }
                Cmd::Quit => {
                    self.out.push(AppCmd::Quit);
                    self.idx = self.cmds.len();
                    return;
                }
            }
        }
    }
}

/// Saves a screenshot as PNG (RGBA8). Runs on the caller's thread.
pub fn save_png(path: &std::path::Path, img: &eframe::egui::ColorImage) -> std::io::Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::File::create(path)?;
    let w = std::io::BufWriter::new(file);
    let mut enc = png::Encoder::new(w, img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header().map_err(std::io::Error::other)?;
    let mut data = Vec::with_capacity(img.pixels.len() * 4);
    for px in &img.pixels {
        // Screenshots are opaque; unmultiply defensively anyway.
        let [r, g, b, a] = px.to_srgba_unmultiplied();
        data.extend_from_slice(&[r, g, b, a]);
    }
    writer
        .write_image_data(&data)
        .map_err(std::io::Error::other)?;
    writer.finish().map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        let h = Harness::parse(
            "wait 1.5; size 1024 700; click 10 20 right; key F5; key c ctrl; type hello world; \
             shot C:\\tmp\\a b.png; zoom C:\\Program Files; tab 2; quit",
        )
        .unwrap();
        assert_eq!(h.cmds.len(), 10);
        assert_eq!(
            h.cmds[2],
            Cmd::Click(At::Point(pos2(10.0, 20.0)), PointerButton::Secondary)
        );
        assert_eq!(h.cmds[5], Cmd::Type("hello world".into()));
        assert_eq!(h.cmds[6], Cmd::Shot(PathBuf::from("C:\\tmp\\a b.png")));
        let h = Harness::parse("click @dir right; move @file; dblclick @ok; click @x").unwrap();
        assert_eq!(
            h.cmds[0],
            Cmd::Click(At::Probe("dir".into()), PointerButton::Secondary)
        );
        assert_eq!(h.cmds[1], Cmd::Move(At::Probe("file".into())));
        assert_eq!(h.cmds[2], Cmd::DblClick(At::Probe("ok".into())));
        assert_eq!(
            h.cmds[3],
            Cmd::Click(At::Probe("x".into()), PointerButton::Primary)
        );
        let h = Harness::parse("scan C:\\Windows\\Web; drive c; drive D:; this-pc").unwrap();
        assert_eq!(
            h.cmds,
            vec![
                Cmd::ScanPath(PathBuf::from("C:\\Windows\\Web")),
                Cmd::Drive('C'),
                Cmd::Drive('D'),
                Cmd::ThisPc
            ]
        );
        assert!(Harness::parse("drive CD").is_err());
        assert!(Harness::parse("drive 1").is_err());
        assert!(Harness::parse("bogus 1").is_err());
        assert!(Harness::parse("key NoSuchKey").is_err());
    }
}
