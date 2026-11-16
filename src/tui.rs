//! `mcscan tui`: one full-screen session for the whole flow - scan (with
//! progress and time remaining), browse and filter the findings, select some,
//! preview an action as a dry run, apply it.
//!
//! Everything is reachable from the keyboard. With mouse support on, rows,
//! column headers, menu entries and the buttons along the bottom can be
//! clicked and lists scroll with the wheel.

use crate::apply::{self, Live, Ops, Status};
use crate::item::{norm_id, short_id};
use crate::nbt::{self, Tag};
use crate::preset::Preset;
use crate::progress::{human_duration, Eta};
use crate::region::Inflater;
use crate::results::{Header, Record};
use crate::scan::{self, human_bytes, human_count};
use crate::world;
use anyhow::{anyhow, bail, Context, Result};
use ratatui::backend::Backend;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::HashMap;
use std::io::{stdin, stdout, IsTerminal};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub type ScanFactory = Arc<dyn Fn() -> Result<scan::ScanOptions> + Send + Sync>;

pub struct Config {
    /// Builds the options for a (re)scan.
    pub make_scan: ScanFactory,
    /// Where scans are saved.
    pub results_path: String,
    pub world: PathBuf,
    /// Findings to start with; `None` scans first.
    pub loaded: Option<(Header, Vec<Record>)>,
    pub mouse: bool,
    pub theme: ThemeName,
    pub backup: bool,
    pub force: bool,
    pub threads: Option<usize>,
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Act {
    Fix,
    Delete,
    Clamp,
    Strip,
    SetCount,
    RemoveEnchant,
    SetEnchant,
    RemoveComponent,
    SetId,
    SetNbt,
    RemoveNbt,
}

/// (action, hotkey inside the menu, label, prompt if it needs a value)
const MENU: [(Act, char, &str, &str); 11] = [
    (Act::Fix, 'f', "Fix what the scan flagged", ""),
    (Act::Delete, 'd', "Delete the items", ""),
    (Act::Clamp, 'c', "Clamp enchantment levels to the vanilla maximum", ""),
    (Act::Strip, 's', "Strip all enchantments", ""),
    (Act::SetCount, 'n', "Set the stack size...", "Stack size (1-99)"),
    (Act::RemoveEnchant, 'r', "Remove an enchantment...", "Enchantment, e.g. sharpness"),
    (Act::SetEnchant, 'e', "Set an enchantment level...", "NAME=LEVEL, e.g. sharpness=5"),
    (Act::RemoveComponent, 'o', "Remove a component...", "Component, e.g. unbreakable"),
    (Act::SetId, 'i', "Change the item id...", "New item id, e.g. stone"),
    (Act::SetNbt, 't', "Set raw NBT...", "PATH=SNBT, e.g. components.minecraft:damage=0"),
    (Act::RemoveNbt, 'x', "Remove raw NBT...", "PATH, e.g. components.minecraft:lore"),
];

fn menu_entry(act: Act) -> &'static (Act, char, &'static str, &'static str) {
    MENU.iter().find(|m| m.0 == act).expect("every action is in the menu")
}

fn build_ops(act: Act, text: &str) -> Result<Ops> {
    let text = text.trim();
    let mut ops = Ops::default();
    let needs_value = !menu_entry(act).3.is_empty();
    if needs_value && text.is_empty() {
        bail!("enter a value");
    }
    match act {
        Act::Fix => ops.fix = true,
        Act::Delete => ops.delete = true,
        Act::Clamp => ops.clamp_enchants = true,
        Act::Strip => ops.strip_enchants = true,
        Act::SetCount => {
            let n: i64 = text.parse().context("not a number")?;
            crate::check_count(n)?;
            ops.set_count = Some(n);
        }
        Act::RemoveEnchant => ops.remove_enchant.push(norm_id(text).into_owned()),
        Act::SetEnchant => ops.set_enchant.push(crate::parse_set_enchant(text)?),
        Act::RemoveComponent => ops.remove_component.push(crate::component_arg(text)),
        Act::SetId => ops.set_id = Some(norm_id(text).into_owned()),
        Act::SetNbt => ops.set.push(crate::parse_set(text)?),
        Act::RemoveNbt => ops.remove.push(nbt::parse_path(text)?),
    }
    Ok(ops)
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum SortKey {
    Num,
    Item,
    Where,
    Why,
}

enum RowState {
    Fresh,
    /// Changed or deleted in this session; the text says which.
    Done(String),
    Skipped(String),
}

struct ScanJob {
    stats: Arc<scan::Stats>,
    eta: Eta,
    rx: Receiver<Result<scan::ScanOutput>>,
}

struct ApplyJob {
    progress: Arc<apply::Progress>,
    eta: Eta,
    rx: Receiver<Result<apply::Report>>,
    ops: Ops,
    targets: Vec<usize>,
    dry: bool,
}

enum PagerKind {
    Info,
    /// A dry run; Enter applies it for real if it would do anything.
    DryRun { ops: Ops, targets: Vec<usize>, actionable: bool },
    /// The world was modified.
    Done,
}

enum Mode {
    Scanning(ScanJob),
    Browse,
    Filter,
    Menu { cursor: usize },
    Input { act: Act, text: String, error: Option<String> },
    Working(ApplyJob),
    Pager { title: String, lines: Vec<String>, scroll: usize, kind: PagerKind },
}

/// Something on screen that reacts to a click.
#[derive(Clone, Copy)]
enum Hit {
    Key(KeyCode),
    Menu(usize),
    Sort(SortKey),
}

struct Button {
    key: &'static str,
    label: String,
    code: KeyCode,
}

impl Button {
    fn new(key: &'static str, label: impl Into<String>, code: KeyCode) -> Self {
        Button { key, label: label.into(), code }
    }

    fn width(&self) -> u16 {
        (self.key.chars().count() + self.label.chars().count() + 3) as u16
    }
}

/// Lay buttons out left to right, wrapping. Returns each one's (x, row) and
/// the number of rows used.
fn flow(buttons: &[Button], width: u16) -> (Vec<(u16, u16)>, u16) {
    let (mut x, mut row) = (0u16, 0u16);
    let mut pos = Vec::with_capacity(buttons.len());
    for b in buttons {
        let w = b.width();
        if x > 0 && x + w > width {
            row += 1;
            x = 0;
        }
        pos.push((x, row));
        x += w + 2;
    }
    (pos, row + 1)
}

/// Pad or cut `s` to exactly `w` columns.
fn fit(s: &str, w: usize) -> String {
    let clean = |c: char| if c.is_control() { ' ' } else { c };
    let n = s.chars().count();
    if n <= w {
        let mut out: String = s.chars().map(clean).collect();
        out.extend(std::iter::repeat(' ').take(w - n));
        out
    } else if w == 0 {
        String::new()
    } else {
        let mut out: String = s.chars().take(w - 1).map(clean).collect();
        out.push('…');
        out
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

fn inside(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

struct Cols {
    num: usize,
    item: usize,
    loc: usize,
    why: usize,
}

const CHECK_W: usize = 4; // "[x] "

// ---------------------------------------------------------------------------
// Themes
// ---------------------------------------------------------------------------

/// Colour scheme of the full-screen interface.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, clap::ValueEnum)]
pub enum ThemeName {
    /// The terminal's own background and text colours
    #[default]
    Default,
    /// Midnight Commander: blue panel, cyan bars, grey dialogs
    #[value(alias = "midnight-commander")]
    Mc,
}

impl ThemeName {
    fn next(self) -> Self {
        match self {
            ThemeName::Default => ThemeName::Mc,
            ThemeName::Mc => ThemeName::Default,
        }
    }

    fn flag(self) -> &'static str {
        match self {
            ThemeName::Default => "default",
            ThemeName::Mc => "mc",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ThemeName::Default => "default",
            ThemeName::Mc => "Midnight Commander",
        }
    }

    fn theme(self) -> Theme {
        let s = Style::new();
        match self {
            ThemeName::Default => {
                let p = Palette {
                    base: s,
                    label: s.fg(Color::Cyan),
                    dim: s.fg(Color::DarkGray),
                    error: s.fg(Color::Red),
                    accent: s.fg(Color::Yellow),
                    hotkey: s.fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    border: s.fg(Color::Cyan),
                    gauge: s.fg(Color::Cyan).bg(Color::DarkGray),
                    cursor: None,
                };
                Theme {
                    main: Palette { border: s, ..p },
                    popup: p,
                    title: s.fg(Color::Black).bg(Color::Cyan),
                    key: s.fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
                    key_label: s,
                    head: s.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    mark: s.fg(Color::Yellow).add_modifier(Modifier::BOLD),
                }
            }
            ThemeName::Mc => {
                let cursor = Some(s.fg(Color::Black).bg(Color::Cyan));
                Theme {
                    main: Palette {
                        base: s.fg(Color::Gray).bg(Color::Blue),
                        label: s.fg(Color::LightCyan),
                        dim: s.fg(Color::Cyan),
                        error: s.fg(Color::LightRed),
                        accent: s.fg(Color::Yellow),
                        hotkey: s.fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        border: s.fg(Color::Gray),
                        gauge: s.fg(Color::Cyan).bg(Color::Black),
                        cursor,
                    },
                    popup: Palette {
                        base: s.fg(Color::Black).bg(Color::Gray),
                        label: s.fg(Color::Blue),
                        dim: s.fg(Color::DarkGray),
                        error: s.fg(Color::Red),
                        accent: s.fg(Color::Blue),
                        hotkey: s.fg(Color::Blue).add_modifier(Modifier::BOLD),
                        border: s.fg(Color::Black),
                        gauge: s.fg(Color::Blue).bg(Color::White),
                        cursor,
                    },
                    title: s.fg(Color::Black).bg(Color::Cyan),
                    key: s.fg(Color::White).bg(Color::Black),
                    key_label: s.fg(Color::Black).bg(Color::Cyan),
                    head: s.fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    mark: s.fg(Color::Yellow).add_modifier(Modifier::BOLD),
                }
            }
        }
    }
}

/// Colours for one surface: the main screen, or a popup on top of it.
#[derive(Clone, Copy)]
struct Palette {
    /// Background and ordinary text; empty leaves the terminal's own.
    base: Style,
    label: Style,
    dim: Style,
    error: Style,
    accent: Style,
    hotkey: Style,
    border: Style,
    gauge: Style,
    /// The highlighted row; `None` reverses whatever the row looks like.
    cursor: Option<Style>,
}

impl Palette {
    fn cursor(&self, row: Style) -> Style {
        match self.cursor {
            Some(c) => row.patch(c),
            None => row.add_modifier(Modifier::REVERSED),
        }
    }
}

#[derive(Clone, Copy)]
struct Theme {
    main: Palette,
    popup: Palette,
    title: Style,
    key: Style,
    key_label: Style,
    head: Style,
    /// A ticked row's checkbox.
    mark: Style,
}

struct App {
    cfg: Config,
    header: Option<Header>,
    world: PathBuf,
    records: Arc<Vec<Record>>,
    /// Lower-cased text of each record, for the filter.
    hay: Vec<String>,
    state: Vec<RowState>,
    selected: Vec<bool>,
    /// Record indices that pass the filter, in display order.
    view: Vec<usize>,
    cursor: usize,
    offset: usize,
    filter: String,
    sort: SortKey,
    sort_desc: bool,
    mode: Mode,
    mouse: bool,
    theme: ThemeName,
    /// Whether to really switch the terminal's mouse reporting (off in tests).
    control_terminal: bool,
    hits: Vec<(Rect, Hit)>,
    table_area: Rect,
    pager_height: usize,
    last_click: Option<(Instant, usize)>,
    status: String,
    /// What the last scan looked into (shown under the list).
    coverage: Vec<String>,
    /// The world was changed since the findings were scanned.
    stale: bool,
    quit: bool,
    fatal: Option<anyhow::Error>,
}

impl App {
    fn new(mut cfg: Config) -> Self {
        let loaded = cfg.loaded.take();
        let mut app = App {
            world: cfg.world.clone(),
            mouse: cfg.mouse,
            theme: cfg.theme,
            cfg,
            header: None,
            records: Arc::new(Vec::new()),
            hay: Vec::new(),
            state: Vec::new(),
            selected: Vec::new(),
            view: Vec::new(),
            cursor: 0,
            offset: 0,
            filter: String::new(),
            sort: SortKey::Num,
            sort_desc: false,
            mode: Mode::Browse,
            control_terminal: false,
            hits: Vec::new(),
            table_area: Rect::default(),
            pager_height: 10,
            last_click: None,
            status: String::new(),
            coverage: Vec::new(),
            stale: false,
            quit: false,
            fatal: None,
        };
        match loaded {
            Some((header, records)) => {
                app.status = format!("Loaded {} finding(s) from {}", human_count(records.len() as u64), app.cfg.results_path);
                app.set_records(header, records);
            }
            None => app.start_scan(),
        }
        app
    }

    fn set_records(&mut self, header: Header, records: Vec<Record>) {
        self.world = PathBuf::from(&header.world);
        self.header = Some(header);
        self.hay = records
            .iter()
            .map(|r| format!("{} {} {} {} {}", r.id, r.item_text(), r.location(), r.why(), r.file).to_lowercase())
            .collect();
        self.state = records.iter().map(|_| RowState::Fresh).collect();
        self.selected = vec![false; records.len()];
        self.records = Arc::new(records);
        self.cursor = 0;
        self.offset = 0;
        self.stale = false;
        self.refilter();
    }

    /// Recompute `view` from the filter and sort order, keeping the cursor on
    /// the same finding where possible.
    fn refilter(&mut self) {
        let current = self.view.get(self.cursor).copied();
        let terms: Vec<String> = self.filter.to_lowercase().split_whitespace().map(str::to_string).collect();
        let hay = &self.hay;
        let mut view: Vec<usize> = (0..self.records.len()).filter(|&i| terms.iter().all(|t| hay[i].contains(t.as_str()))).collect();
        let recs = &self.records;
        match self.sort {
            SortKey::Num => view.sort_by_key(|&i| recs[i].n),
            SortKey::Item => view.sort_by_cached_key(|&i| (short_id(&recs[i].id).to_string(), recs[i].n)),
            SortKey::Where => view.sort_by_cached_key(|&i| (recs[i].location(), recs[i].n)),
            SortKey::Why => view.sort_by_cached_key(|&i| (recs[i].why(), recs[i].n)),
        }
        if self.sort_desc {
            view.reverse();
        }
        self.view = view;
        self.cursor = current.and_then(|c| self.view.iter().position(|&i| i == c)).unwrap_or(0);
    }

    fn set_sort(&mut self, k: SortKey) {
        if self.sort == k {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort = k;
            self.sort_desc = false;
        }
        self.refilter();
    }

    fn selected_count(&self) -> usize {
        self.selected.iter().filter(|s| **s).count()
    }

    /// What an action applies to: the selection, or the row under the cursor
    /// if nothing is selected. Rows already changed in this session are left out.
    fn targets(&self) -> Vec<usize> {
        let fresh = |i: &usize| !matches!(self.state[*i], RowState::Done(_));
        if self.selected.iter().any(|s| *s) {
            (0..self.records.len()).filter(|&i| self.selected[i]).filter(fresh).collect()
        } else {
            self.view.get(self.cursor).copied().into_iter().filter(fresh).collect()
        }
    }

    fn move_cursor(&mut self, delta: i64) {
        if self.view.is_empty() {
            self.cursor = 0;
            return;
        }
        let max = self.view.len() as i64 - 1;
        self.cursor = (self.cursor as i64 + delta).clamp(0, max) as usize;
    }

    fn toggle(&mut self, row: usize) {
        if let Some(&i) = self.view.get(row) {
            self.selected[i] = !self.selected[i];
        }
    }

    fn set_mouse(&mut self, on: bool) {
        if self.control_terminal {
            let ok = if on { execute!(stdout(), EnableMouseCapture) } else { execute!(stdout(), DisableMouseCapture) };
            self.mouse = on && ok.is_ok();
        } else {
            self.mouse = on;
        }
    }

    // -- background jobs ----------------------------------------------------

    fn start_scan(&mut self) {
        let stats = Arc::new(scan::Stats::default());
        let (tx, rx) = mpsc::channel();
        let (make, shared) = (self.cfg.make_scan.clone(), stats.clone());
        std::thread::spawn(move || {
            let result = (|| {
                let opts = make()?;
                let plan = scan::plan(&opts)?;
                scan::execute(&plan, &opts, &shared)
            })();
            let _ = tx.send(result);
        });
        self.status.clear();
        self.mode = Mode::Scanning(ScanJob { stats, eta: Eta::new(), rx });
    }

    fn finish_scan(&mut self, result: Result<scan::ScanOutput>) {
        self.mode = Mode::Browse;
        match result {
            Ok(out) => {
                let mut status = format!("{} - {} finding(s)", out.speed_line(), human_count(out.records.len() as u64));
                match scan::save(&self.cfg.results_path, &out.header, &out.records) {
                    Ok(()) => status.push_str(&format!(", saved to {}", self.cfg.results_path)),
                    Err(e) => status.push_str(&format!(" - NOT saved: {e:#}")),
                }
                self.coverage = out.coverage_lines();
                self.coverage.extend(world::sibling_worlds(&PathBuf::from(&out.header.world)).iter().map(|p| {
                    format!("note: {} is a separate world folder and is not part of this scan - scan it as well", p.display())
                }));
                if out.errors > 0 {
                    status.push_str(&format!(" - {} unreadable chunk(s)/file(s) skipped", human_count(out.errors)));
                }
                if world::lock_holder(&PathBuf::from(&out.header.world)).is_some() {
                    status.push_str(" - world is open in a running server: stop it before applying changes");
                }
                self.status = status;
                self.set_records(out.header, out.records);
            }
            Err(e) if self.header.is_none() => {
                // Nothing to fall back to: leave the TUI and report.
                self.fatal = Some(e);
                self.quit = true;
            }
            Err(e) => self.status = format!("Rescan failed: {e:#}"),
        }
    }

    /// Run `ops` on `targets` in the background, as a dry run or for real.
    fn start_apply(&mut self, ops: Ops, targets: Vec<usize>, dry: bool) {
        let progress = Arc::new(apply::Progress::default());
        let (tx, rx) = mpsc::channel();
        let records = self.records.clone();
        let spec = self.header.as_ref().and_then(|h| h.preset.clone()).unwrap_or_else(|| "illegal".to_string());
        let mut opts = apply::ApplyOptions {
            world: self.world.clone(),
            ops: ops.clone(),
            preset: None,
            dry_run: dry,
            backup: self.cfg.backup,
            force: self.cfg.force,
            yes: true,
            interactive: false,
            threads: self.cfg.threads,
        };
        let (shared, picked) = (progress.clone(), targets.clone());
        std::thread::spawn(move || {
            let result = (|| {
                if opts.ops.needs_preset() {
                    let mut p = Preset::load(&spec)?;
                    // The scan decided which rules count; fix repairs what it recorded.
                    p.enable_all();
                    opts.preset = Some(p);
                }
                let selected: Vec<&Record> = picked.iter().map(|&i| &records[i]).collect();
                apply::execute(&opts, &selected, &shared)
            })();
            let _ = tx.send(result);
        });
        self.mode = Mode::Working(ApplyJob { progress, eta: Eta::new(), rx, ops, targets, dry });
    }

    fn finish_apply(&mut self, job: ApplyJob, result: Result<apply::Report>) {
        let report = match result {
            Ok(r) => r,
            Err(e) => {
                self.message("Could not apply", vec![format!("{e:#}")]);
                return;
            }
        };
        let by_n: HashMap<u64, usize> = self.records.iter().enumerate().map(|(i, r)| (r.n, i)).collect();
        let (changed, deleted, _, skips) = report.counts();
        let mut lines = vec![
            format!(
                "Action: {} - {} item stack(s) in {} file(s)",
                job.ops.describe(),
                human_count(job.targets.len() as u64),
                human_count(report.files as u64)
            ),
            report.summary(),
        ];
        if let Some(dir) = &report.backup_dir {
            lines.push(format!("Originals of every modified file are in {}", dir.display()));
        }
        for w in &report.warnings {
            lines.push(format!("warning: {w}"));
        }
        if skips > 0 {
            lines.push("Skipped items no longer match the scan; rescan (r) to pick them up.".to_string());
        }
        lines.push(String::new());
        for o in &report.outcomes {
            if let Some(&i) = by_n.get(&o.n) {
                lines.push(o.line(&self.records[i], report.dry_run));
            }
        }

        if job.dry {
            let actionable = changed + deleted > 0;
            self.mode = Mode::Pager {
                title: "Preview (dry run) - nothing has been written yet".to_string(),
                lines,
                scroll: 0,
                kind: PagerKind::DryRun { ops: job.ops, targets: job.targets, actionable },
            };
            return;
        }
        for o in &report.outcomes {
            let Some(&i) = by_n.get(&o.n) else { continue };
            self.selected[i] = false;
            match &o.status {
                Status::Deleted => self.state[i] = RowState::Done("deleted".to_string()),
                Status::Changed => self.state[i] = RowState::Done(format!("changed: {}", o.notes.join("; "))),
                Status::Skipped(why) => self.state[i] = RowState::Skipped(why.clone()),
                Status::Unchanged => {}
            }
        }
        if changed + deleted > 0 {
            self.stale = true;
        }
        self.status = report.summary();
        self.mode = Mode::Pager { title: "Applied".to_string(), lines, scroll: 0, kind: PagerKind::Done };
    }

    fn tick(&mut self) {
        fn poll<T>(rx: &Receiver<Result<T>>) -> Option<Result<T>> {
            match rx.try_recv() {
                Ok(r) => Some(r),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(anyhow!("the worker thread stopped unexpectedly"))),
            }
        }
        match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::Scanning(mut job) => {
                job.eta.update(job.stats.bytes_done.load(Ordering::Relaxed));
                match poll(&job.rx) {
                    Some(result) => self.finish_scan(result),
                    None => self.mode = Mode::Scanning(job),
                }
            }
            Mode::Working(mut job) => {
                job.eta.update(job.progress.files_done.load(Ordering::Relaxed));
                match poll(&job.rx) {
                    Some(result) => self.finish_apply(job, result),
                    None => self.mode = Mode::Working(job),
                }
            }
            other => self.mode = other,
        }
    }

    fn busy(&self) -> bool {
        matches!(self.mode, Mode::Scanning(_) | Mode::Working(_))
    }

    // -- popups -------------------------------------------------------------

    fn message(&mut self, title: &str, lines: Vec<String>) {
        self.mode = Mode::Pager { title: title.to_string(), lines, scroll: 0, kind: PagerKind::Info };
    }

    fn help(&mut self) {
        let text = "\
Moving
  Up/Down, j/k        one finding            PgUp/PgDn   one page
  Home/End, g/G       first / last           mouse wheel scroll

Selecting
  Space               select / unselect the current finding
  a / n / i           select all shown / none / invert shown
  click [ ] or right-click a row   select it

Looking
  Enter, double-click full details and the item's current NBT on disk
  /                   filter: words that must all appear (item, place, rule, file)
  s / S               next sort column / reverse    (or click a column header)

Acting  (on the selection, or on the current finding if nothing is selected)
  f                   fix what the scan flagged
  d                   delete
  e                   more: clamp/strip enchantments, set count, set/remove NBT ...
  Every action first runs as a dry run and shows exactly what it would do;
  nothing is written until you confirm that preview.

Other
  c                   what the last scan looked into (blocks, entities, players)
  r                   rescan the world
  m                   mouse support on/off (off lets the terminal select text)
  t                   switch theme: default / Midnight Commander
  ?                   this help              q, Ctrl-C   quit

Changes are refused while a game or server has the world open, items that
changed since the scan are skipped, and modified files are backed up to
<world>/mcscan-backups/ first.";
        self.message("Keys", text.lines().map(str::to_string).collect());
    }

    fn details(&mut self) {
        let Some(&i) = self.view.get(self.cursor) else { return };
        let rec = &self.records[i];
        let mut lines = vec![
            format!("#{}  {}", rec.n, rec.item_text()),
            format!("where:  {}", rec.location()),
            format!("file:   {}{}", rec.file, rec.chunk.map(|[x, z]| format!("   chunk {x},{z}")).unwrap_or_default()),
            format!("path:   {}", nbt::path_to_string(&rec.path)),
        ];
        if !rec.info.is_empty() {
            lines.push(format!("info:   {}", rec.info));
        }
        for r in &rec.reasons {
            lines.push(format!("why:    {}: {}", r.rule, r.detail));
        }
        match &self.state[i] {
            RowState::Fresh => {}
            RowState::Done(s) => lines.push(format!("status: {s} in this session")),
            RowState::Skipped(s) => lines.push(format!("status: skipped - {s}")),
        }
        lines.push(String::new());
        match apply::load_item(&self.world, rec, &mut Inflater::default(), &mut Vec::new()) {
            Err(e) => lines.push(format!("Could not read the item from disk: {e:#}")),
            Ok(Live::ChunkGone) => lines.push("The chunk no longer exists on disk.".to_string()),
            Ok(Live::ItemGone) => lines.push("The item is no longer there on disk.".to_string()),
            Ok(Live::Item { item, unchanged }) => {
                lines.push(
                    if unchanged { "Current NBT on disk:" } else { "Current NBT on disk (changed since the scan):" }.to_string(),
                );
                let mut s = String::new();
                apply::pretty_snbt(&Tag::Compound(item), 0, &mut s);
                lines.extend(s.lines().map(str::to_string));
            }
        }
        let title = format!("Finding #{}", rec.n);
        self.message(&title, lines);
    }

    // -- actions ------------------------------------------------------------

    fn choose(&mut self, act: Act) {
        if self.targets().is_empty() {
            self.mode = Mode::Browse;
            self.status = "Nothing to act on.".to_string();
        } else if menu_entry(act).3.is_empty() {
            self.begin(act, "");
        } else {
            self.mode = Mode::Input { act, text: String::new(), error: None };
        }
    }

    /// Start the dry run of an action. On a bad value, re-open the prompt.
    fn begin(&mut self, act: Act, text: &str) {
        match build_ops(act, text) {
            Ok(ops) => {
                let targets = self.targets();
                self.start_apply(ops, targets, true);
            }
            Err(e) => self.mode = Mode::Input { act, text: text.to_string(), error: Some(format!("{e:#}")) },
        }
    }

    fn apply_for_real(&mut self, ops: Ops, targets: Vec<usize>) {
        if !self.cfg.force {
            if let Some(pid) = world::lock_holder(&self.world) {
                self.message(
                    "World is in use",
                    vec![
                        format!("This world is open in a running game/server (pid {pid})."),
                        String::new(),
                        "Stop it first: the server would overwrite the changes and can corrupt".to_string(),
                        "chunks it has loaded. Nothing was written.".to_string(),
                    ],
                );
                return;
            }
        }
        self.start_apply(ops, targets, false);
    }

    // -- input --------------------------------------------------------------

    fn on_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            if let Mode::Scanning(job) = &self.mode {
                job.stats.cancel.store(true, Ordering::Relaxed);
            }
            self.quit = true;
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::Scanning(job) => {
                if matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) {
                    job.stats.cancel.store(true, Ordering::Relaxed);
                    self.status = "Cancelling...".to_string();
                }
                self.mode = Mode::Scanning(job);
            }
            // Never interrupted: it may be in the middle of rewriting files.
            Mode::Working(job) => self.mode = Mode::Working(job),
            Mode::Browse => self.browse_key(k),
            Mode::Filter => match k.code {
                KeyCode::Enter => {}
                KeyCode::Esc => {
                    self.filter.clear();
                    self.refilter();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.refilter();
                    self.mode = Mode::Filter;
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.refilter();
                    self.mode = Mode::Filter;
                }
                _ => self.mode = Mode::Filter,
            },
            Mode::Menu { cursor } => match k.code {
                KeyCode::Esc | KeyCode::Char('q') => {}
                KeyCode::Up | KeyCode::Char('k') => self.mode = Mode::Menu { cursor: cursor.saturating_sub(1) },
                KeyCode::Down | KeyCode::Char('j') => self.mode = Mode::Menu { cursor: (cursor + 1).min(MENU.len() - 1) },
                KeyCode::Enter => self.choose(MENU[cursor].0),
                KeyCode::Char(c) => match MENU.iter().find(|m| m.1 == c) {
                    Some(m) => self.choose(m.0),
                    None => self.mode = Mode::Menu { cursor },
                },
                _ => self.mode = Mode::Menu { cursor },
            },
            Mode::Input { act, mut text, error } => match k.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.begin(act, &text),
                KeyCode::Backspace => {
                    text.pop();
                    self.mode = Mode::Input { act, text, error: None };
                }
                KeyCode::Char(c) => {
                    text.push(c);
                    self.mode = Mode::Input { act, text, error: None };
                }
                _ => self.mode = Mode::Input { act, text, error },
            },
            Mode::Pager { title, lines, scroll, kind } => {
                let last = lines.len().saturating_sub(1);
                let page = self.pager_height.max(1);
                let scrolled = match k.code {
                    KeyCode::Up | KeyCode::Char('k') => Some(scroll.saturating_sub(1)),
                    KeyCode::Down | KeyCode::Char('j') => Some((scroll + 1).min(last)),
                    KeyCode::PageUp => Some(scroll.saturating_sub(page)),
                    KeyCode::PageDown | KeyCode::Char(' ') => Some((scroll + page).min(last)),
                    KeyCode::Home | KeyCode::Char('g') => Some(0),
                    KeyCode::End | KeyCode::Char('G') => Some(last),
                    _ => None,
                };
                if let Some(scroll) = scrolled {
                    self.mode = Mode::Pager { title, lines, scroll, kind };
                    return;
                }
                match (k.code, kind) {
                    (KeyCode::Esc | KeyCode::Char('q'), _) => {}
                    (KeyCode::Enter | KeyCode::Char('y'), PagerKind::DryRun { ops, targets, actionable: true }) => {
                        self.apply_for_real(ops, targets)
                    }
                    (KeyCode::Enter, PagerKind::Info | PagerKind::Done) => {}
                    (KeyCode::Char('r'), PagerKind::Done) => self.start_scan(),
                    (_, kind) => self.mode = Mode::Pager { title, lines, scroll, kind },
                }
            }
        }
    }

    fn browse_key(&mut self, k: KeyEvent) {
        let page = (self.table_area.height as i64).max(1);
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.refilter();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-page),
            KeyCode::PageDown => self.move_cursor(page),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = self.view.len().saturating_sub(1),
            KeyCode::Char(' ') => {
                self.toggle(self.cursor);
                self.move_cursor(1);
            }
            KeyCode::Char('a') => {
                for &i in &self.view {
                    self.selected[i] = true;
                }
            }
            KeyCode::Char('n') => self.selected.iter_mut().for_each(|s| *s = false),
            KeyCode::Char('i') => {
                for &i in &self.view {
                    self.selected[i] = !self.selected[i];
                }
            }
            KeyCode::Char('f') => self.choose(Act::Fix),
            KeyCode::Char('d') => self.choose(Act::Delete),
            KeyCode::Char('e') => {
                if self.targets().is_empty() {
                    self.status = "Nothing to act on.".to_string();
                } else {
                    self.mode = Mode::Menu { cursor: 0 };
                }
            }
            KeyCode::Enter => self.details(),
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('s') => {
                let next = match self.sort {
                    SortKey::Num => SortKey::Item,
                    SortKey::Item => SortKey::Where,
                    SortKey::Where => SortKey::Why,
                    SortKey::Why => SortKey::Num,
                };
                self.set_sort(next);
            }
            KeyCode::Char('S') => self.set_sort(self.sort),
            KeyCode::Char('r') => self.start_scan(),
            KeyCode::Char('c') => {
                let lines = if self.coverage.is_empty() {
                    vec!["These findings were loaded from a file; rescan (r) to see what a scan looks into.".to_string()]
                } else {
                    self.coverage.clone()
                };
                self.message("What the scan looked into", lines);
            }
            KeyCode::Char('m') => {
                self.set_mouse(!self.mouse);
                self.status = if self.mouse {
                    "Mouse support on.".to_string()
                } else {
                    "Mouse support off - the terminal's own text selection works again.".to_string()
                };
            }
            KeyCode::Char('t') => {
                self.theme = self.theme.next();
                self.status = format!("Theme: {} (start with --theme {} to keep it).", self.theme.label(), self.theme.flag());
            }
            KeyCode::Char('?') | KeyCode::F(1) => self.help(),
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let wheel = match m.kind {
            MouseEventKind::ScrollUp => -3i64,
            MouseEventKind::ScrollDown => 3,
            _ => 0,
        };
        if wheel != 0 {
            match &mut self.mode {
                Mode::Browse | Mode::Filter => self.move_cursor(wheel),
                Mode::Pager { lines, scroll, .. } => {
                    *scroll = (*scroll as i64 + wheel).clamp(0, lines.len().saturating_sub(1) as i64) as usize;
                }
                Mode::Menu { cursor } => {
                    *cursor = (*cursor as i64 + wheel.signum()).clamp(0, MENU.len() as i64 - 1) as usize;
                }
                _ => {}
            }
            return;
        }
        let button = match m.kind {
            MouseEventKind::Down(b) => b,
            _ => return,
        };
        let (x, y) = (m.column, m.row);
        if button == MouseButton::Left {
            // Later entries are drawn on top of earlier ones.
            if let Some(&(_, hit)) = self.hits.iter().rev().find(|(r, _)| inside(*r, x, y)) {
                match hit {
                    Hit::Key(code) => self.on_key(key(code)),
                    Hit::Menu(i) => self.choose(MENU[i].0),
                    Hit::Sort(k) => self.set_sort(k),
                }
                return;
            }
        }
        if !matches!(self.mode, Mode::Browse | Mode::Filter) || !inside(self.table_area, x, y) {
            return;
        }
        let row = self.offset + (y - self.table_area.y) as usize;
        if row >= self.view.len() {
            return;
        }
        self.mode = Mode::Browse;
        self.cursor = row;
        match button {
            MouseButton::Right => self.toggle(row),
            MouseButton::Left if (x - self.table_area.x) < CHECK_W as u16 => self.toggle(row),
            MouseButton::Left => {
                let now = Instant::now();
                let double = matches!(self.last_click, Some((t, r)) if r == row && now.duration_since(t) < Duration::from_millis(450));
                self.last_click = if double { None } else { Some((now, row)) };
                if double {
                    self.details();
                }
            }
            _ => {}
        }
    }

    // -- drawing ------------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        self.hits.clear();
        let area = f.area();
        f.render_widget(Block::new().style(self.theme.theme().main.base), area);
        let mode = std::mem::replace(&mut self.mode, Mode::Browse);
        match &mode {
            Mode::Scanning(job) => self.draw_scan(f, area, job),
            Mode::Browse => self.draw_browse(f, area, true, false),
            Mode::Filter => self.draw_browse(f, area, true, true),
            Mode::Menu { cursor } => {
                self.draw_browse(f, area, false, false);
                self.draw_menu(f, area, *cursor);
            }
            Mode::Input { act, text, error } => {
                self.draw_browse(f, area, false, false);
                self.draw_input(f, area, *act, text, error.as_deref());
            }
            Mode::Working(job) => {
                self.draw_browse(f, area, false, false);
                self.draw_working(f, area, job);
            }
            Mode::Pager { title, lines, scroll, kind } => {
                self.draw_browse(f, area, false, false);
                self.draw_pager(f, area, title, lines, *scroll, kind);
            }
        }
        self.mode = mode;
    }

    fn draw_buttons(&mut self, f: &mut Frame, area: Rect, buttons: &[Button], active: bool) {
        let t = self.theme.theme();
        let (pos, _) = flow(buttons, area.width);
        for (b, (x, row)) in buttons.iter().zip(pos) {
            if row >= area.height || x >= area.width {
                break;
            }
            let rect = Rect::new(area.x + x, area.y + row, b.width().min(area.width - x), 1);
            let line = Line::from(vec![Span::styled(format!(" {} ", b.key), t.key), Span::styled(format!(" {}", b.label), t.key_label)]);
            f.render_widget(Paragraph::new(line), rect);
            if active {
                self.hits.push((rect, Hit::Key(b.code)));
            }
        }
    }

    fn browse_buttons(&self) -> Vec<Button> {
        let c = KeyCode::Char;
        vec![
            Button::new("Space", "Select", c(' ')),
            Button::new("a", "All", c('a')),
            Button::new("n", "None", c('n')),
            Button::new("i", "Invert", c('i')),
            Button::new("f", "Fix", c('f')),
            Button::new("d", "Delete", c('d')),
            Button::new("e", "More actions", c('e')),
            Button::new("Enter", "Details", KeyCode::Enter),
            Button::new("/", "Filter", c('/')),
            Button::new("s", "Sort", c('s')),
            Button::new("r", "Rescan", c('r')),
            Button::new("m", if self.mouse { "Mouse: on" } else { "Mouse: off" }, c('m')),
            Button::new("t", "Theme", c('t')),
            Button::new("?", "Help", c('?')),
            Button::new("q", "Quit", c('q')),
        ]
    }

    fn cols(&self, width: usize) -> Cols {
        let num = format!("#{}", self.records.len()).len().max(3);
        let rest = width.saturating_sub(CHECK_W + num + 1 + 2 + 2);
        let item = (rest * 28 / 100).clamp(8, 36).min(rest);
        let loc = ((rest - item) * 45 / 100).min(64);
        Cols { num, item, loc, why: rest - item - loc }
    }

    fn draw_browse(&mut self, f: &mut Frame, area: Rect, active: bool, editing_filter: bool) {
        let t = self.theme.theme();
        let p = t.main;
        let buttons = self.browse_buttons();
        let (_, bar_rows) = flow(&buttons, area.width);
        let detail_rows = if area.height >= 20 { 6 } else { 0 };
        let [title, head, table, detail, status, bar] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(detail_rows),
            Constraint::Length(1),
            Constraint::Length(bar_rows),
        ])
        .areas(area);

        // Title bar.
        let mut text = format!(
            " mcscan │ {} │ {} finding(s)",
            self.world.display(),
            human_count(self.records.len() as u64)
        );
        if self.view.len() != self.records.len() {
            text.push_str(&format!(" │ {} shown", human_count(self.view.len() as u64)));
        }
        text.push_str(&format!(" │ {} selected", human_count(self.selected_count() as u64)));
        if self.stale {
            text.push_str(" │ world changed: press r to rescan");
        }
        f.render_widget(Paragraph::new(fit(&text, title.width as usize)).style(t.title), title);

        // Column headers (click to sort).
        let c = self.cols(area.width as usize);
        let mark = |k: SortKey| match (self.sort == k, self.sort_desc) {
            (false, _) => "",
            (true, false) => " ▲",
            (true, true) => " ▼",
        };
        let heads = [
            (SortKey::Num, "#", CHECK_W, c.num),
            (SortKey::Item, "ITEM", CHECK_W + c.num + 1, c.item),
            (SortKey::Where, "WHERE", CHECK_W + c.num + 1 + c.item + 2, c.loc),
            (SortKey::Why, "WHY", CHECK_W + c.num + 1 + c.item + 2 + c.loc + 2, c.why),
        ];
        for (k, name, x, w) in heads {
            if w == 0 || x >= head.width as usize {
                continue;
            }
            let rect = Rect::new(head.x + x as u16, head.y, (w as u16).min(head.width - x as u16), 1);
            f.render_widget(Paragraph::new(fit(&format!("{name}{}", mark(k)), w)).style(t.head), rect);
            if active {
                self.hits.push((rect, Hit::Sort(k)));
            }
        }

        // Rows: only the visible window is formatted, so huge lists stay fast.
        self.table_area = table;
        let h = table.height as usize;
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if h > 0 && self.cursor >= self.offset + h {
            self.offset = self.cursor + 1 - h;
        }
        self.offset = self.offset.min(self.view.len().saturating_sub(1));
        if self.view.is_empty() {
            let msg = if self.records.is_empty() {
                "No findings - this world is clean for the rules and filters used."
            } else {
                "Nothing matches the filter (Esc clears it)."
            };
            let mut text = format!("\n  {msg}\n");
            for line in &self.coverage {
                text.push_str(&format!("\n  {line}"));
            }
            f.render_widget(Paragraph::new(text).style(p.dim).wrap(Wrap { trim: false }), table);
        } else {
            let mut lines = Vec::with_capacity(h);
            for (row, &i) in self.view.iter().enumerate().skip(self.offset).take(h) {
                let r = &self.records[i];
                let (why, style) = match &self.state[i] {
                    RowState::Fresh => (r.why(), Style::new()),
                    RowState::Done(s) => (s.clone(), p.dim),
                    RowState::Skipped(s) => (format!("skipped: {s}"), p.error),
                };
                let style = if row == self.cursor { p.cursor(style) } else { style };
                let (check, check_style) = if self.selected[i] {
                    ("[x] ", style.patch(t.mark))
                } else {
                    ("[ ] ", style)
                };
                let text = format!(
                    "{} {}  {}  {}",
                    fit(&format!("#{}", r.n), c.num),
                    fit(&r.item_text(), c.item),
                    fit(&r.location(), c.loc),
                    fit(&why, c.why)
                );
                lines.push(Line::from(vec![Span::styled(check, check_style), Span::styled(text, style)]));
            }
            f.render_widget(Paragraph::new(lines), table);
        }

        // Details of the current finding.
        if detail_rows > 0 {
            let block = Block::new().borders(Borders::TOP).title(" Details ").border_style(p.border);
            let inner = block.inner(detail);
            f.render_widget(block, detail);
            if let Some(&i) = self.view.get(self.cursor) {
                let r = &self.records[i];
                let label = p.label;
                let mut lines = vec![
                    Line::from(vec![
                        Span::styled(format!("#{} ", r.n), label),
                        Span::styled(r.item_text(), Style::new().add_modifier(Modifier::BOLD)),
                        Span::raw(if r.info.is_empty() { String::new() } else { format!("   {}", r.info) }),
                    ]),
                    Line::from(vec![Span::styled("where ", label), Span::raw(r.location())]),
                    Line::from(vec![
                        Span::styled("file  ", label),
                        Span::raw(format!(
                            "{}{}   {}",
                            r.file,
                            r.chunk.map(|[x, z]| format!("  chunk {x},{z}")).unwrap_or_default(),
                            nbt::path_to_string(&r.path)
                        )),
                    ]),
                ];
                if !r.reasons.is_empty() {
                    lines.push(Line::from(vec![Span::styled("why   ", label), Span::raw(r.why())]));
                }
                f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
            }
        }

        // Status / filter line.
        let line = if editing_filter {
            Line::from(vec![
                Span::styled("Filter: ", p.accent.add_modifier(Modifier::BOLD)),
                Span::raw(self.filter.clone()),
                Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
                Span::styled("   Enter keeps it, Esc clears it", p.dim),
            ])
        } else if !self.filter.is_empty() {
            Line::from(vec![
                Span::styled(format!("Filter: {}", self.filter), p.accent),
                Span::styled("  (Esc clears)   ", p.dim),
                Span::raw(self.status.clone()),
            ])
        } else {
            Line::from(self.status.clone())
        };
        f.render_widget(Paragraph::new(line), status);

        self.draw_buttons(f, bar, &buttons, active);
    }

    fn popup(&self, f: &mut Frame, rect: Rect, title: &str) -> Rect {
        let p = self.theme.theme().popup;
        let block = Block::bordered().title(format!(" {title} ")).style(p.base).border_style(p.border);
        let inner = block.inner(rect);
        f.render_widget(Clear, rect);
        f.render_widget(block, rect);
        // One column of padding left and right.
        Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), inner.height)
    }

    fn draw_scan(&mut self, f: &mut Frame, area: Rect, job: &ScanJob) {
        let p = self.theme.theme().popup;
        let s = &job.stats;
        let load = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        let total = load(&s.total_bytes);
        let done = load(&s.bytes_done).min(total);
        let ratio = if total == 0 { 0.0 } else { done as f64 / total as f64 };
        let inner = self.popup(f, centered(area, 76, 14), "mcscan - scanning");
        let [world_row, _, gauge, _, stats, _, status, bar] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(5),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        f.render_widget(Paragraph::new(fit(&self.cfg.world.display().to_string(), world_row.width as usize)), world_row);
        f.render_widget(
            Gauge::default()
                .gauge_style(p.gauge)
                .ratio(ratio.clamp(0.0, 1.0))
                .label(format!("{:.0}%", ratio * 100.0))
                .use_unicode(true),
            gauge,
        );
        let lines = if total == 0 {
            vec![Line::from("Looking for world files...")]
        } else {
            let label = p.label;
            let row = |a: &str, av: String, b: &str, bv: String| {
                Line::from(vec![
                    Span::styled(format!("{a:<13}"), label),
                    Span::raw(format!("{av:<24}")),
                    Span::styled(format!("{b:<13}"), label),
                    Span::raw(bv),
                ])
            };
            vec![
                row(
                    "Files",
                    format!("{} / {}", human_count(load(&s.files_done)), human_count(load(&s.total_files))),
                    "Chunks",
                    human_count(load(&s.chunks)),
                ),
                row(
                    "Data",
                    format!("{} / {}", human_bytes(done), human_bytes(total)),
                    "Item stacks",
                    human_count(load(&s.items)),
                ),
                row("Speed", format!("{}/s", human_bytes(job.eta.rate() as u64)), "Matches", human_count(load(&s.matches))),
                Line::default(),
                row("Elapsed", human_duration(job.eta.elapsed()), "Time left", job.eta.text(total)),
            ]
        };
        f.render_widget(Paragraph::new(lines), stats);
        f.render_widget(Paragraph::new(self.status.clone()).style(p.accent), status);
        self.draw_buttons(f, bar, &[Button::new("Esc", "Cancel", KeyCode::Esc)], true);
    }

    fn draw_working(&mut self, f: &mut Frame, area: Rect, job: &ApplyJob) {
        let p = self.theme.theme().popup;
        let total = job.progress.files_total.load(Ordering::Relaxed);
        let done = job.progress.files_done.load(Ordering::Relaxed).min(total);
        let ratio = if total == 0 { 0.0 } else { done as f64 / total as f64 };
        let title = if job.dry { "Working out what would change (dry run)" } else { "Applying changes to the world" };
        let inner = self.popup(f, centered(area, 70, 8), title);
        let [what, _, gauge, _, line] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .areas(inner);
        let what_text = format!("{} - {} item stack(s)", job.ops.describe(), human_count(job.targets.len() as u64));
        f.render_widget(Paragraph::new(fit(&what_text, what.width as usize)), what);
        f.render_widget(
            Gauge::default()
                .gauge_style(p.gauge)
                .ratio(ratio.clamp(0.0, 1.0))
                .label(format!("{:.0}%", ratio * 100.0))
                .use_unicode(true),
            gauge,
        );
        let text = format!(
            "Files {} / {}     Elapsed {}     Time left {}",
            human_count(done),
            human_count(total),
            human_duration(job.eta.elapsed()),
            job.eta.text(total)
        );
        f.render_widget(Paragraph::new(text), line);
    }

    fn draw_menu(&mut self, f: &mut Frame, area: Rect, cursor: usize) {
        let p = self.theme.theme().popup;
        let n = self.targets().len();
        let title = format!("Action for {} item stack(s)", human_count(n as u64));
        let inner = self.popup(f, centered(area, 64, MENU.len() as u16 + 4), &title);
        for (i, (_, hotkey, label, _)) in MENU.iter().enumerate() {
            if i as u16 >= inner.height {
                break;
            }
            let rect = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
            let style = if i == cursor { p.cursor(Style::new()) } else { Style::new() };
            let line = Line::from(vec![
                Span::styled(format!(" {hotkey} "), style.patch(p.hotkey)),
                Span::styled(fit(&format!(" {label}"), (inner.width as usize).saturating_sub(3)), style),
            ]);
            f.render_widget(Paragraph::new(line), rect);
            self.hits.push((rect, Hit::Menu(i)));
        }
        if inner.height > MENU.len() as u16 + 1 {
            let bar = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
            self.draw_buttons(
                f,
                bar,
                &[Button::new("Enter", "Preview", KeyCode::Enter), Button::new("Esc", "Cancel", KeyCode::Esc)],
                true,
            );
        }
    }

    fn draw_input(&mut self, f: &mut Frame, area: Rect, act: Act, text: &str, error: Option<&str>) {
        let p = self.theme.theme().popup;
        let (_, _, label, prompt) = menu_entry(act);
        let inner = self.popup(f, centered(area, 72, 8), label.trim_end_matches('.'));
        let [prompt_row, input, _, err, _, bar] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(inner);
        f.render_widget(Paragraph::new(*prompt).style(p.dim), prompt_row);
        // Keep the end of a long value in view.
        let room = (input.width as usize).saturating_sub(3);
        let skip = text.chars().count().saturating_sub(room);
        let shown: String = text.chars().skip(skip).collect();
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("> ", p.accent),
                Span::raw(shown),
                Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
            ])),
            input,
        );
        if let Some(e) = error {
            f.render_widget(Paragraph::new(fit(e, err.width as usize)).style(p.error), err);
        }
        self.draw_buttons(
            f,
            bar,
            &[Button::new("Enter", "Preview", KeyCode::Enter), Button::new("Esc", "Cancel", KeyCode::Esc)],
            true,
        );
    }

    fn draw_pager(&mut self, f: &mut Frame, area: Rect, title: &str, lines: &[String], scroll: usize, kind: &PagerKind) {
        let p = self.theme.theme().popup;
        let widest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
        let w = (widest + 4).clamp(50, area.width.saturating_sub(4).max(50));
        let h = (lines.len() as u16 + 4).clamp(7, area.height.saturating_sub(2).max(7));
        let inner = self.popup(f, centered(area, w, h), title);
        let [body, _, bar] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]).areas(inner);
        self.pager_height = body.height as usize;
        let shown: Vec<Line> = lines
            .iter()
            .skip(scroll)
            .take(body.height as usize)
            .map(|l| {
                let style = if l.contains(" SKIPPED ") || l.starts_with("warning:") {
                    p.error
                } else if l.starts_with("DRY RUN") || l.starts_with("Done:") {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                };
                Line::styled(l.clone(), style)
            })
            .collect();
        f.render_widget(Paragraph::new(shown).wrap(Wrap { trim: false }), body);

        let mut buttons = match kind {
            PagerKind::Info => vec![Button::new("Esc", "Close", KeyCode::Esc)],
            PagerKind::DryRun { actionable: true, .. } => vec![
                Button::new("Enter", "Apply this to the world", KeyCode::Enter),
                Button::new("Esc", "Cancel", KeyCode::Esc),
            ],
            PagerKind::DryRun { .. } => vec![Button::new("Esc", "Close - nothing to apply", KeyCode::Esc)],
            PagerKind::Done => {
                vec![Button::new("Esc", "Close", KeyCode::Esc), Button::new("r", "Rescan", KeyCode::Char('r'))]
            }
        };
        if lines.len() > body.height as usize {
            buttons.push(Button::new("PgDn", "More", KeyCode::PageDown));
            buttons.push(Button::new("PgUp", "Back", KeyCode::PageUp));
        }
        self.draw_buttons(f, bar, &buttons, true);
    }

    fn event_loop<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        while !self.quit {
            self.tick();
            terminal.draw(|f| self.draw(f))?;
            let wait = Duration::from_millis(if self.busy() { 100 } else { 500 });
            if !event::poll(wait)? {
                continue;
            }
            // Handle everything that is queued before drawing again.
            loop {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => self.on_key(k),
                    Event::Mouse(m) => self.on_mouse(m),
                    _ => {}
                }
                if self.quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        Ok(())
    }
}

pub fn run(cfg: Config) -> Result<()> {
    if !stdout().is_terminal() || !stdin().is_terminal() {
        bail!("the TUI needs an interactive terminal; use `scan`, `list` and `apply` in scripts");
    }
    let want_mouse = cfg.mouse;
    let mut app = App::new(cfg);
    app.control_terminal = true;

    let mut terminal = ratatui::init();
    // ratatui's hook restores the screen on a panic; mouse reporting is ours to undo.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        previous(info);
    }));
    app.set_mouse(want_mouse);
    let result = app.event_loop(&mut terminal);
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();

    result?;
    if let Some(e) = app.fatal {
        return Err(e);
    }
    if app.header.is_some() && std::path::Path::new(&app.cfg.results_path).is_file() {
        println!("Findings are in {}", app.cfg.results_path);
        if app.stale {
            println!("The world was changed after that scan; scan again before using the file with `mcscan apply`.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn record(n: u64, id: &str, rule: &str, slot: i32) -> Record {
        serde_json::from_value(serde_json::json!({
            "n": n, "id": id, "count": 1,
            "reasons": [{"rule": rule, "detail": "detail text"}],
            "source": "block_entity", "holder": "minecraft:chest", "dim": "minecraft:overworld",
            "pos": [1, 2, 3], "slot": slot, "file": "region/r.0.0.mca", "chunk": [0, 0],
            "path": ["block_entities", 0, "Items", slot], "hash": "00"
        }))
        .unwrap()
    }

    fn app() -> App {
        let records = vec![
            record(1, "minecraft:bedrock", "unobtainable_item", 0),
            record(2, "minecraft:diamond_sword", "enchant_over_max", 1),
            record(3, "minecraft:zombie_spawn_egg", "unobtainable_item", 2),
            record(4, "minecraft:stick", "enchant_wrong_item", 3),
        ];
        let header = Header {
            mcscan: crate::results::FORMAT_VERSION,
            world: "/nonexistent/world".into(),
            preset: Some("illegal".into()),
            filters: vec![],
            scanned_at: 0,
            items_scanned: 10,
            matches: 4,
        };
        App::new(Config {
            make_scan: Arc::new(|| bail!("no scanning in tests")),
            results_path: "unused.jsonl".into(),
            world: PathBuf::from("/nonexistent/world"),
            loaded: Some((header, records)),
            mouse: true,
            theme: ThemeName::Default,
            backup: true,
            force: false,
            threads: Some(1),
        })
    }

    /// Draw one frame and return the screen as text lines.
    fn screen(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()).to_string()).collect::<String>())
            .collect()
    }

    fn click(app: &mut App, x: u16, y: u16, button: MouseButton) {
        app.on_mouse(MouseEvent { kind: MouseEventKind::Down(button), column: x, row: y, modifiers: KeyModifiers::NONE });
    }

    fn find(lines: &[String], needle: &str) -> (u16, u16) {
        for (y, l) in lines.iter().enumerate() {
            if let Some(byte) = l.find(needle) {
                return (l[..byte].chars().count() as u16, y as u16);
            }
        }
        panic!("{needle:?} is not on screen:\n{}", lines.join("\n"));
    }

    fn ids(app: &App) -> Vec<u64> {
        app.view.iter().map(|&i| app.records[i].n).collect()
    }

    #[test]
    fn text_fitting() {
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("a\nb", 3), "a b");
        assert_eq!(fit("abc", 0), "");
    }

    #[test]
    fn buttons_wrap() {
        let b = |l: &str| Button::new("x", l, KeyCode::Char('x'));
        let buttons = [b("one"), b("two"), b("three")];
        // widths 7, 7, 9 with gaps of 2
        assert_eq!(flow(&buttons, 80), (vec![(0, 0), (9, 0), (18, 0)], 1));
        assert_eq!(flow(&buttons, 17), (vec![(0, 0), (9, 0), (0, 1)], 2));
    }

    #[test]
    fn renders_findings_and_buttons() {
        let mut app = app();
        let lines = screen(&mut app, 120, 30);
        let all = lines.join("\n");
        for needle in ["4 finding(s)", "0 selected", "bedrock", "zombie_spawn_egg", "chest @ overworld 1 2 3 slot 0", "Details", "Fix", "Mouse: on", "Quit"] {
            assert!(all.contains(needle), "{needle:?} missing:\n{all}");
        }
    }

    #[test]
    fn midnight_commander_theme() {
        let style_at = |app: &mut App, x: u16, y: u16| {
            let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            let c = terminal.backend().buffer().cell((x, y)).unwrap().clone();
            (c.fg, c.bg, c.modifier)
        };
        let mut app = app();
        // The default theme leaves the terminal's colours alone and reverses the cursor row.
        let (_, bg, modifier) = style_at(&mut app, 119, 4);
        assert_eq!(bg, Color::Reset);
        assert!(!modifier.contains(Modifier::REVERSED));
        assert!(style_at(&mut app, 10, 2).2.contains(Modifier::REVERSED));

        app.on_key(key(KeyCode::Char('t')));
        assert_eq!(app.theme, ThemeName::Mc);
        assert!(app.status.contains("Midnight Commander"), "{}", app.status);
        // Blue panel, black-on-cyan cursor bar, yellow tick on a selected row.
        assert_eq!(style_at(&mut app, 119, 4), (Color::Gray, Color::Blue, Modifier::empty()));
        assert_eq!(style_at(&mut app, 10, 2), (Color::Black, Color::Cyan, Modifier::empty()));
        app.on_key(key(KeyCode::Char(' ')));
        app.on_key(key(KeyCode::Down));
        let (fg, bg, _) = style_at(&mut app, 1, 2);
        assert_eq!((fg, bg), (Color::Yellow, Color::Blue));
        // Popups are grey dialogs.
        app.on_key(key(KeyCode::Char('?')));
        let lines = screen(&mut app, 120, 30);
        let (x, y) = find(&lines, "Moving");
        assert_eq!(style_at(&mut app, x, y).1, Color::Gray);
        app.on_key(key(KeyCode::Esc));

        app.on_key(key(KeyCode::Char('t')));
        assert_eq!(app.theme, ThemeName::Default);
        assert_eq!(style_at(&mut app, 119, 4).1, Color::Reset);
    }

    #[test]
    fn filter_and_sort() {
        let mut app = app();
        app.on_key(key(KeyCode::Char('/')));
        for c in "UNOBTAIN egg".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(ids(&app), vec![3]);
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Browse));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(ids(&app), vec![1, 2, 3, 4]);
        app.on_key(key(KeyCode::Char('s'))); // by item
        assert_eq!(ids(&app), vec![1, 2, 4, 3]);
        app.on_key(key(KeyCode::Char('S'))); // reversed
        assert_eq!(ids(&app), vec![3, 4, 2, 1]);
    }

    #[test]
    fn keyboard_selection() {
        let mut app = app();
        app.on_key(key(KeyCode::Char(' ')));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(app.selected, vec![true, false, true, false]);
        assert_eq!(app.targets(), vec![0, 2]);
        app.on_key(key(KeyCode::Char('i')));
        assert_eq!(app.selected, vec![false, true, false, true]);
        app.on_key(key(KeyCode::Char('n')));
        // Nothing selected: the action applies to the row under the cursor.
        assert_eq!(app.targets(), vec![app.view[app.cursor]]);
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(app.selected_count(), 4);
    }

    #[test]
    fn mouse_clicks() {
        let mut app = app();
        let lines = screen(&mut app, 120, 30);

        // A row: left click moves the cursor, the checkbox and right click select.
        let (_, y) = find(&lines, "zombie_spawn_egg");
        click(&mut app, 30, y, MouseButton::Left);
        assert_eq!(app.cursor, 2);
        assert_eq!(app.selected_count(), 0);
        click(&mut app, 1, y, MouseButton::Left);
        assert_eq!(app.selected, vec![false, false, true, false]);
        click(&mut app, 30, y - 1, MouseButton::Right);
        assert_eq!(app.selected, vec![false, true, true, false]);

        // A column header sorts; clicking it again reverses.
        let (x, y) = find(&lines, "ITEM");
        click(&mut app, x, y, MouseButton::Left);
        assert_eq!(ids(&app), vec![1, 2, 4, 3]);
        click(&mut app, x, y, MouseButton::Left);
        assert_eq!(ids(&app), vec![3, 4, 2, 1]);

        // A button behaves like its key.
        let (x, y) = find(&lines, "None");
        click(&mut app, x, y, MouseButton::Left);
        assert_eq!(app.selected_count(), 0);
        let (x, y) = find(&lines, "More actions");
        click(&mut app, x, y, MouseButton::Left);
        assert!(matches!(app.mode, Mode::Menu { .. }));

        // A menu entry that needs a value opens the prompt.
        let lines = screen(&mut app, 120, 30);
        let (x, y) = find(&lines, "Set the stack size");
        click(&mut app, x, y, MouseButton::Left);
        assert!(matches!(app.mode, Mode::Input { act: Act::SetCount, .. }));
        // Buttons of the list underneath are not clickable through a popup.
        let lines = screen(&mut app, 120, 30);
        let (x, y) = find(&lines, "Quit");
        click(&mut app, x, y, MouseButton::Left);
        assert!(!app.quit);
        let (x, y) = find(&lines, "Cancel");
        click(&mut app, x, y, MouseButton::Left);
        assert!(matches!(app.mode, Mode::Browse));

        // The wheel moves through the list.
        app.cursor = 0;
        app.on_mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column: 5, row: 5, modifiers: KeyModifiers::NONE });
        assert_eq!(app.cursor, 3);
    }

    #[test]
    fn double_click_opens_details() {
        let mut app = app();
        let lines = screen(&mut app, 120, 30);
        let (_, y) = find(&lines, "diamond_sword");
        click(&mut app, 30, y, MouseButton::Left);
        assert!(matches!(app.mode, Mode::Browse));
        click(&mut app, 30, y, MouseButton::Left);
        let Mode::Pager { title, lines, .. } = &app.mode else { panic!("no details popup") };
        assert_eq!(title, "Finding #2");
        assert!(lines.iter().any(|l| l.contains("enchant_over_max")), "{lines:?}");
    }

    #[test]
    fn bad_values_keep_the_prompt_open() {
        let mut app = app();
        app.on_key(key(KeyCode::Char('e')));
        app.on_key(key(KeyCode::Char('n')));
        for c in "500".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        let Mode::Input { error: Some(e), text, .. } = &app.mode else { panic!("prompt closed") };
        assert_eq!(text, "500");
        assert!(e.contains("between 1 and 99"), "{e}");
        let lines = screen(&mut app, 120, 30).join("\n");
        assert!(lines.contains("between 1 and 99"), "{lines}");
    }

    #[test]
    fn actions_from_text() {
        assert!(build_ops(Act::Fix, "").unwrap().fix);
        assert!(build_ops(Act::Delete, "").unwrap().delete);
        assert_eq!(build_ops(Act::SetCount, " 16 ").unwrap().set_count, Some(16));
        assert_eq!(build_ops(Act::SetEnchant, "sharpness=5").unwrap().set_enchant, vec![("minecraft:sharpness".to_string(), 5)]);
        assert_eq!(build_ops(Act::RemoveComponent, "unbreakable").unwrap().remove_component, vec!["minecraft:unbreakable"]);
        assert!(build_ops(Act::SetCount, "").is_err());
        assert!(build_ops(Act::SetEnchant, "sharpness").is_err());
        assert!(build_ops(Act::SetNbt, "components.minecraft:damage=0").is_ok());
    }

    #[test]
    fn small_terminals_do_not_panic() {
        let mut app = app();
        for (w, h) in [(1, 1), (10, 3), (20, 8), (40, 12), (200, 60)] {
            screen(&mut app, w, h);
            app.help();
            screen(&mut app, w, h);
            app.mode = Mode::Menu { cursor: 0 };
            screen(&mut app, w, h);
            app.mode = Mode::Input { act: Act::SetCount, text: "x".repeat(300), error: Some("bad".into()) };
            screen(&mut app, w, h);
            app.mode = Mode::Browse;
        }
    }
}
