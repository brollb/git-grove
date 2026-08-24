//! Interactive worktree picker, drawn on /dev/tty.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::{execute, terminal};

use crate::app::{App, PrCell, Sort};
use crate::fuzzy::Hits;
use crate::gh;
use crate::git::Status;
use crate::load::{Loader, Msg};

const RESET: &str = "\x1b[0m";
const CYAN: &str = "36";
const MAGENTA: &str = "35";
const YELLOW: &str = "33";
const GREEN: &str = "32";
const RED: &str = "31";
const DIM: &str = "90";
const BOLD: &str = "1";
/// Matched characters of the query.
const HL: &str = "1;36";

enum Row {
    Header(String),
    Item(usize, Hits),
}

/// The picker is modal: keys are commands until `/` opens the filter.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    Search,
}

/// A destructive action waiting on a keypress.
enum Prompt {
    /// Remove these worktrees?
    Delete(Vec<PathBuf>),
    /// git refused these; force them, losing whatever is in them?
    Force(Vec<PathBuf>),
}

/// Query and cursor, kept across picker visits so that returning from a
/// worktree shell (`--cd`) lands you back where you were.
#[derive(Default)]
pub struct PickerState {
    pub query: String,
    sel: usize,
    off: usize,
    /// Worktrees marked with space, by path so that marks survive the list
    /// being re-sorted, re-filtered, or having rows removed.
    marked: HashSet<PathBuf>,
}

impl PickerState {
    pub fn with_query(query: String) -> PickerState {
        PickerState {
            query,
            ..Default::default()
        }
    }
}

/// Restores the terminal however we leave the picker — including on panic.
struct Restore {
    tty: File,
}

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = execute!(
            self.tty,
            terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = terminal::disable_raw_mode();
    }
}

pub fn run(
    app: &mut App,
    loader: &mut Loader,
    rx: &Receiver<Msg>,
    state: &mut PickerState,
) -> io::Result<Option<PathBuf>> {
    if app.worktrees.is_empty() {
        return Ok(None);
    }

    let mut tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    let _restore = Restore {
        tty: tty.try_clone()?,
    };

    terminal::enable_raw_mode()?;
    execute!(tty, terminal::EnterAlternateScreen, crossterm::cursor::Hide)?;

    if app.sort != Sort::Name {
        for wt in &app.worktrees {
            loader.request_age(&wt.path);
        }
    }
    let mut rows = build_rows(app, &state.query);
    let mut sel = state.sel.min(rows.len().saturating_sub(1));
    if !matches!(rows.get(sel), Some(Row::Item(..))) {
        sel = first_item(&rows);
    }
    let mut off = state.off;
    let mut dirty = true;
    let mut mode = Mode::Normal;
    let mut prompt: Option<Prompt> = None;
    let mut message: Option<String> = None;
    let mut batch = Batch::default();
    let selected;

    loop {
        let (width, height) = size();
        let view = height.saturating_sub(6).max(1); // title + prompt + header + 3 footer

        if sel < off {
            off = sel;
        } else if sel >= off + view {
            off = sel + 1 - view;
        }
        off = off.min(rows.len().saturating_sub(view));

        // Only the rows on screen pay for a `git status`.
        for row in rows.iter().skip(off).take(view) {
            if let Row::Item(i, _) = row {
                let path = &app.worktrees[*i].path;
                if !app.statuses.contains_key(path) {
                    loader.request_status(path);
                    loader.request_age(path);
                }
            }
        }

        if dirty {
            let progress = batch.progress();
            let frame = Frame {
                rows: &rows,
                query: &state.query,
                mode,
                sel,
                off,
                view,
                width,
                marked: &state.marked,
                removing: &batch.pending,
                progress: &progress,
                prompt: prompt.as_ref(),
                message: message.as_deref(),
            };
            draw(&mut tty, app, &frame)?;
            dirty = false;
        }

        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    let alt = key.modifiers.contains(KeyModifiers::ALT);
                    let mut requery = false;
                    // Any keypress clears the last result line.
                    message = None;

                    if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')) {
                        selected = None;
                        break;
                    }

                    // While worktrees are being deleted, keys that would leave
                    // the picker are held back: quitting would kill the removals
                    // mid-`rm`. ctrl-c above still gets out.
                    if batch.in_flight()
                        && matches!(
                            key.code,
                            KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('d') | KeyCode::Esc
                        )
                        && mode == Mode::Normal
                    {
                        message = Some("ctrl-c to quit anyway".to_string());
                        dirty = true;
                        continue;
                    }

                    if let Some(pending) = prompt.take() {
                        // A confirmation is up: only its own answers count, and
                        // anything else dismisses it without acting.
                        match (&pending, key.code) {
                            (Prompt::Delete(targets), KeyCode::Char('y' | 'Y'))
                            | (Prompt::Force(targets), KeyCode::Char('f' | 'F')) => {
                                // Hand every removal to a background thread and
                                // keep drawing: an `rm -rf` per worktree adds up
                                // to minutes across a big batch.
                                batch = Batch::starting(targets.len());
                                batch.force = matches!(pending, Prompt::Force(_));
                                for target in targets {
                                    let Some(wt) = app.worktrees.iter().find(|w| &w.path == target)
                                    else {
                                        batch.total -= 1;
                                        continue;
                                    };
                                    if let Some(reason) = cannot_remove(wt) {
                                        // Never dispatched, so it is not part of
                                        // the progress count, and forcing it
                                        // would not help either.
                                        batch.total -= 1;
                                        batch.refused.push((target.clone(), reason));
                                        continue;
                                    }
                                    batch.pending.insert(target.clone());
                                    loader.remove(app.repo_main(wt), target.clone(), batch.force);
                                }
                                if batch.pending.is_empty() {
                                    finish_batch(
                                        app,
                                        loader,
                                        state,
                                        &mut batch,
                                        &mut message,
                                        &mut prompt,
                                    );
                                    requery = true;
                                }
                            }
                            _ => message = Some("  cancelled".to_string()),
                        }
                    } else if mode == Mode::Search {
                        match key.code {
                            KeyCode::Enter | KeyCode::Esc => mode = Mode::Normal,
                            KeyCode::Char('u') if ctrl => {
                                state.query.clear();
                                requery = true;
                            }
                            KeyCode::Char('w') if ctrl => {
                                let trimmed = state.query.trim_end();
                                let cut = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);
                                state.query.truncate(cut);
                                requery = true;
                            }
                            KeyCode::Backspace => {
                                state.query.pop();
                                requery = true;
                            }
                            KeyCode::Up => sel = step(&rows, sel, -1),
                            KeyCode::Down => sel = step(&rows, sel, 1),
                            KeyCode::Char('p') if ctrl => sel = step(&rows, sel, -1),
                            KeyCode::Char('n') if ctrl => sel = step(&rows, sel, 1),
                            KeyCode::Char(c) if !ctrl && !alt => {
                                state.query.push(c);
                                requery = true;
                            }
                            _ => {}
                        }
                    } else {
                        match key.code {
                            KeyCode::Char('/') => mode = Mode::Search,
                            KeyCode::Char('q') => {
                                selected = None;
                                break;
                            }
                            KeyCode::Esc => {
                                if state.query.is_empty() {
                                    selected = None;
                                    break;
                                }
                                state.query.clear();
                                requery = true;
                            }
                            KeyCode::Enter => {
                                if let Some(Row::Item(i, _)) = rows.get(sel) {
                                    selected = Some(app.worktrees[*i].path.clone());
                                    break;
                                }
                            }
                            KeyCode::Char(' ') => {
                                if let Some(Row::Item(i, _)) = rows.get(sel) {
                                    let path = app.worktrees[*i].path.clone();
                                    if !state.marked.remove(&path) {
                                        state.marked.insert(path);
                                    }
                                    // Fall down a row, so a run of worktrees
                                    // can be marked with repeated taps.
                                    sel = step(&rows, sel, 1);
                                }
                            }
                            KeyCode::Char('a') => {
                                let visible: Vec<PathBuf> = rows
                                    .iter()
                                    .filter_map(|r| match r {
                                        Row::Item(i, _) => Some(app.worktrees[*i].path.clone()),
                                        _ => None,
                                    })
                                    .collect();
                                // All already marked → unmark; otherwise mark the rest.
                                if visible.iter().all(|p| state.marked.contains(p)) {
                                    for path in &visible {
                                        state.marked.remove(path);
                                    }
                                } else {
                                    state.marked.extend(visible);
                                }
                            }
                            KeyCode::Char('d') => {
                                let targets = delete_targets(app, &state.marked, &rows, sel);
                                if targets.is_empty() {
                                    message = Some("  nothing to delete".to_string());
                                } else {
                                    prompt = Some(Prompt::Delete(targets));
                                }
                            }
                            KeyCode::Char('s') => {
                                app.sort = app.sort.next();
                                if app.sort != Sort::Name {
                                    for wt in &app.worktrees {
                                        loader.request_age(&wt.path);
                                    }
                                }
                                // Rebuilding resets the cursor to the top, which
                                // is the point of asking for newest/oldest first.
                                // (A re-sort from data arriving in the background
                                // keeps your place instead — see below.)
                                requery = true;
                            }
                            KeyCode::Char('o') => {
                                if let Some(Row::Item(i, _)) = rows.get(sel) {
                                    if let PrCell::Open(pr) = app.pr_for(&app.worktrees[*i]) {
                                        gh::open_url(&pr.url);
                                    }
                                }
                            }
                            KeyCode::Up | KeyCode::Char('k') => sel = step(&rows, sel, -1),
                            KeyCode::Down | KeyCode::Char('j') => sel = step(&rows, sel, 1),
                            KeyCode::Char('p') if ctrl => sel = step(&rows, sel, -1),
                            KeyCode::Char('n') if ctrl => sel = step(&rows, sel, 1),
                            KeyCode::PageUp => sel = jump(&rows, sel, -(view as isize)),
                            KeyCode::PageDown => sel = jump(&rows, sel, view as isize),
                            KeyCode::Home | KeyCode::Char('g') => sel = first_item(&rows),
                            KeyCode::End | KeyCode::Char('G') => sel = last_item(&rows),
                            _ => {}
                        }
                    }

                    if requery {
                        let keep = match rows.get(sel) {
                            Some(Row::Item(i, _)) => app.worktrees.get(*i).map(|w| w.path.clone()),
                            _ => None,
                        };
                        rows = build_rows(app, &state.query);
                        // After a delete the cursor should stay put; after a
                        // query or sort change it belongs at the top.
                        sel = if message.is_some() {
                            keep.and_then(|path| {
                                rows.iter().position(
                                    |r| matches!(r, Row::Item(i, _) if app.worktrees[*i].path == path),
                                )
                            })
                            .unwrap_or_else(|| first_item(&rows))
                        } else {
                            off = 0;
                            first_item(&rows)
                        };
                        sel = sel.min(rows.len().saturating_sub(1));
                    }
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }

        let mut prs_arrived = false;
        let mut ages_arrived = false;
        let mut removal_arrived = false;
        while let Ok(msg) = rx.try_recv() {
            prs_arrived |= matches!(msg, Msg::Prs(..));
            ages_arrived |= matches!(msg, Msg::Age(..) | Msg::Status(..));
            if let Msg::Removed(path, result) = msg {
                batch.pending.remove(&path);
                match result {
                    Ok(()) => batch.removed.push(path),
                    Err(reason) => batch.failed.push((path, reason)),
                }
                removal_arrived = true;
            } else {
                app.apply(msg);
            }
            dirty = true;
        }
        // Rows are dropped once the whole batch has reported, so the list does
        // not reshuffle under the cursor on every individual removal.
        let batch_done = removal_arrived && !batch.in_flight();
        if batch_done {
            finish_batch(app, loader, state, &mut batch, &mut message, &mut prompt);
        }
        // PR numbers are searchable, so late-arriving PRs can change the match
        // set; newly dated rows can change a recency sort's order.
        let resort = ages_arrived && app.sort != Sort::Name;
        if batch_done || resort || (prs_arrived && !state.query.trim().is_empty()) {
            let keep = match rows.get(sel) {
                Some(Row::Item(i, _)) => Some(*i),
                _ => None,
            };
            rows = build_rows(app, &state.query);
            // Follow the selected worktree to wherever the new order put it.
            sel = keep
                .and_then(|k| {
                    rows.iter()
                        .position(|r| matches!(r, Row::Item(i, _) if *i == k))
                })
                .unwrap_or_else(|| first_item(&rows));
        }
    }

    state.sel = sel;
    state.off = off;
    Ok(selected)
}

/// What `d` acts on: everything marked, or the row under the cursor when
/// nothing is marked.
fn delete_targets(app: &App, marked: &HashSet<PathBuf>, rows: &[Row], sel: usize) -> Vec<PathBuf> {
    if !marked.is_empty() {
        // Follow list order rather than the set's, so messages read sensibly.
        return app
            .worktrees
            .iter()
            .filter(|wt| marked.contains(&wt.path))
            .map(|wt| wt.path.clone())
            .collect();
    }
    match rows.get(sel) {
        Some(Row::Item(i, _)) => vec![app.worktrees[*i].path.clone()],
        _ => Vec::new(),
    }
}

/// One round of deletions, in flight on the background threads.
#[derive(Default)]
struct Batch {
    /// Removals dispatched and not yet reported back.
    pending: HashSet<PathBuf>,
    removed: Vec<PathBuf>,
    /// git refused these; forcing may get past it.
    failed: Vec<(PathBuf, String)>,
    /// We refused these ourselves; forcing would not change the answer.
    refused: Vec<(PathBuf, String)>,
    /// How many were dispatched, for the progress line.
    total: usize,
    /// Whether this round was already the forced one.
    force: bool,
}

impl Batch {
    fn starting(total: usize) -> Batch {
        Batch {
            total,
            ..Default::default()
        }
    }

    fn in_flight(&self) -> bool {
        !self.pending.is_empty()
    }

    fn progress(&self) -> String {
        format!(
            "  removing {} of {}\u{2026}",
            self.total - self.pending.len(),
            self.total
        )
    }

    fn outcome(&self) -> Removal {
        let mut failed = self.failed.clone();
        failed.extend(self.refused.iter().cloned());
        Removal {
            removed: self.removed.clone(),
            failed,
        }
    }
}

/// Apply a finished batch: drop the rows that are gone, report, and offer to
/// force whatever git refused.
fn finish_batch(
    app: &mut App,
    loader: &mut Loader,
    state: &mut PickerState,
    batch: &mut Batch,
    message: &mut Option<String>,
    prompt: &mut Option<Prompt>,
) {
    let removed: HashSet<PathBuf> = batch.removed.iter().cloned().collect();
    app.forget_worktrees(&removed);
    state.marked.retain(|p| !removed.contains(p));
    for path in &removed {
        loader.forget(path);
    }
    *message = Some(batch.outcome().summary());
    if !batch.failed.is_empty() && !batch.force {
        *prompt = Some(Prompt::Force(
            batch.failed.iter().map(|(p, _)| p.clone()).collect(),
        ));
    }
    *batch = Batch::default();
}

#[derive(Default, Clone)]
struct Removal {
    removed: Vec<PathBuf>,
    failed: Vec<(PathBuf, String)>,
}

impl Removal {
    fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.removed.is_empty() {
            parts.push(format!(
                "removed {} worktree{}",
                self.removed.len(),
                if self.removed.len() == 1 { "" } else { "s" }
            ));
        }
        if let Some((_, reason)) = self.failed.first() {
            parts.push(format!("{} refused: {reason}", self.failed.len()));
        }
        if parts.is_empty() {
            parts.push("nothing removed".to_string());
        }
        format!("  {}", parts.join("  \u{b7}  "))
    }
}

/// Why a worktree cannot be removed at all, decided before any git call.
fn cannot_remove(wt: &crate::git::Worktree) -> Option<String> {
    wt.main
        .then(|| "a repo's main worktree cannot be removed".to_string())
}

fn build_rows(app: &App, query: &str) -> Vec<Row> {
    let matches = app.filter(query);

    // Filtered results are ranked across repos, so repo headers would be
    // meaningless; the path column carries the repo name in that case anyway.
    if !query.trim().is_empty() {
        return matches
            .into_iter()
            .map(|(i, hits)| Row::Item(i, hits))
            .collect();
    }

    let mut rows = Vec::new();
    let multi = app.repos.len() > 1;
    let mut last_repo = usize::MAX;
    for (i, hits) in matches {
        let wt = &app.worktrees[i];
        if multi && wt.repo != last_repo {
            rows.push(Row::Header(app.repos[wt.repo].label.clone()));
            last_repo = wt.repo;
        }
        rows.push(Row::Item(i, hits));
    }
    rows
}

fn first_item(rows: &[Row]) -> usize {
    rows.iter()
        .position(|r| matches!(r, Row::Item(..)))
        .unwrap_or(0)
}

fn last_item(rows: &[Row]) -> usize {
    rows.iter()
        .rposition(|r| matches!(r, Row::Item(..)))
        .unwrap_or(0)
}

/// Move to the next selectable row in `dir`, staying put at the ends.
fn step(rows: &[Row], from: usize, dir: isize) -> usize {
    let mut i = from as isize;
    loop {
        i += dir;
        if i < 0 || i as usize >= rows.len() {
            return from;
        }
        if matches!(rows[i as usize], Row::Item(..)) {
            return i as usize;
        }
    }
}

/// Move roughly `n` rows, then settle on the nearest selectable row.
fn jump(rows: &[Row], from: usize, n: isize) -> usize {
    if rows.is_empty() {
        return 0;
    }
    let target = (from as isize + n).clamp(0, rows.len() as isize - 1) as usize;
    if matches!(rows[target], Row::Item(..)) {
        return target;
    }
    let settled = step(rows, target, if n < 0 { -1 } else { 1 });
    if matches!(rows[settled], Row::Item(..)) && settled != target {
        settled
    } else {
        step(rows, target, if n < 0 { 1 } else { -1 })
    }
}

fn size() -> (usize, usize) {
    match terminal::size() {
        Ok((w, h)) if w >= 40 && h >= 8 => (w as usize, h as usize),
        Ok((w, h)) => ((w as usize).max(40), (h as usize).max(8)),
        Err(_) => (100, 30),
    }
}

struct Widths {
    pr: usize,
    branch: usize,
    status: usize,
    age: usize,
    path: usize,
}

fn widths(app: &App, total: usize) -> Widths {
    let pr = 7;
    let status = 14;
    let age = 5;
    let fixed = 3 + pr + 2 + status + 2 + age + 2 + 2; // gutter + gaps
    let flex = total.saturating_sub(fixed).max(20);
    let longest = app
        .worktrees
        .iter()
        .map(|w| w.branch_label().chars().count())
        .max()
        .unwrap_or(20);
    let branch = longest.clamp(12, (flex * 3 / 5).max(12));
    Widths {
        pr,
        branch,
        status,
        age,
        path: flex.saturating_sub(branch).max(8),
    }
}

struct Frame<'a> {
    rows: &'a [Row],
    query: &'a str,
    mode: Mode,
    sel: usize,
    off: usize,
    view: usize,
    width: usize,
    marked: &'a HashSet<PathBuf>,
    removing: &'a HashSet<PathBuf>,
    progress: &'a str,
    prompt: Option<&'a Prompt>,
    message: Option<&'a str>,
}

fn draw(tty: &mut File, app: &App, f: &Frame) -> io::Result<()> {
    let (rows, sel, off, view, width) = (f.rows, f.sel, f.off, f.view, f.width);
    let w = widths(app, width);
    let color = app.color;
    let now = unix_now();
    let mut buf = String::new();
    buf.push_str("\x1b[H"); // home

    let total = app.worktrees.len();
    let with_prs = app
        .worktrees
        .iter()
        .filter(|wt| matches!(app.pr_for(wt), PrCell::Open(_)))
        .count();
    let sort_note = if app.sort == Sort::Name {
        String::new()
    } else {
        let undated = app.undated();
        if undated > 0 {
            format!("  ·  {}, dating {undated}\u{2026}", app.sort.label())
        } else {
            format!("  ·  {}", app.sort.label())
        }
    };
    let title = format!(
        " {} worktree{} in {}{}{sort_note}",
        total,
        if total == 1 { "" } else { "s" },
        crate::app::display_path(&app.root, None),
        if app.want_prs {
            format!("  ·  {with_prs} with an open PR")
        } else {
            String::new()
        },
    );
    line(&mut buf, &paint(BOLD, &fit(&title, width), color));
    line(&mut buf, &search_line(f, rows, total, width, color));
    line(
        &mut buf,
        &paint(
            DIM,
            &fit(
                &format!(
                    "   {}  {}  {}  {}  {}",
                    fit("PR", w.pr),
                    fit("BRANCH", w.branch),
                    fit("STATUS", w.status),
                    fit_right("AGE", w.age),
                    "PATH"
                ),
                width,
            ),
            color,
        ),
    );

    let mut drawn = 0;
    for (idx, row) in rows.iter().enumerate().skip(off).take(view) {
        drawn += 1;
        match row {
            Row::Header(label) => {
                line(
                    &mut buf,
                    &paint(BOLD, &fit(&format!(" {label}"), width), color),
                );
            }
            Row::Item(i, hits) => {
                let wt = &app.worktrees[*i];
                let selected = idx == sel;
                let pr_cell = fit(&pr_text(app.pr_for(wt)), w.pr);
                let status = app.statuses.get(&wt.path);
                let status_cell = if f.removing.contains(&wt.path) {
                    fit("removing\u{2026}", w.status)
                } else {
                    fit(&status_text(status, wt.locked, wt.prunable), w.status)
                };
                let age_cell = fit_right(&age_text(status, now), w.age);
                let branch = wt.branch_label();
                let path = crate::app::display_path(&wt.path, Some(&app.root));
                let marked = f.marked.contains(&wt.path);
                let marker = format!(
                    "{}{} ",
                    if selected { "\u{25b8}" } else { " " },
                    if marked { "\u{25cf}" } else { " " }
                );

                if selected {
                    // Reverse video over the whole row, so per-cell colors (and
                    // their resets) can't punch holes in the highlight.
                    let plain = fit(
                        &format!(
                            "{marker}{pr_cell}  {}  {status_cell}  {age_cell}  {}",
                            fit(&branch, w.branch),
                            fit_tail(&path, w.path)
                        ),
                        width,
                    );
                    line(&mut buf, &paint("7", &plain, color));
                } else if color {
                    let marker = if marked {
                        format!("{} ", paint(GREEN, marker.trim_end(), true))
                    } else {
                        marker.clone()
                    };
                    let styled = format!(
                        "{marker}{}  {}  {}  {}  {}",
                        paint(pr_color(app.pr_for(wt)), &pr_cell, true),
                        fit_hl(&branch, w.branch, &hits.branch, false, ""),
                        paint(status_color(status, wt.prunable), &status_cell, true),
                        paint(DIM, &age_cell, true),
                        fit_hl(&path, w.path, &hits.path, true, DIM),
                    );
                    line(&mut buf, &styled);
                } else {
                    let plain = fit(
                        &format!(
                            "{marker}{pr_cell}  {}  {status_cell}  {age_cell}  {}",
                            fit(&branch, w.branch),
                            fit_tail(&path, w.path)
                        ),
                        width,
                    );
                    line(&mut buf, &plain);
                }
            }
        }
    }

    if drawn == 0 {
        line(&mut buf, "");
        line(
            &mut buf,
            &paint(DIM, &fit("  no worktree matches that query", width), color),
        );
        drawn = 2;
    }
    for _ in drawn..view {
        line(&mut buf, "");
    }

    line(&mut buf, "");
    let detail = match rows.get(sel) {
        Some(Row::Item(i, _)) => detail_text(app, *i, now),
        _ => String::new(),
    };
    line(&mut buf, &fit(&detail, width));
    let footer = if !f.removing.is_empty() {
        // Progress keeps ticking, but anything the picker needs to say (such as
        // why `q` did nothing) rides along with it rather than replacing it.
        let text = match f.message {
            Some(message) => format!("{}  \u{b7} {}", f.progress.trim_end(), message.trim()),
            None => f.progress.to_string(),
        };
        paint(YELLOW, &fit(&text, width), color)
    } else {
        match (f.prompt, f.message) {
            (Some(prompt), _) => paint(YELLOW, &fit(&prompt_text(prompt), width), color),
            (None, Some(message)) => fit(message, width),
            (None, None) => paint(DIM, &fit(hints(f.mode), width), color),
        }
    };
    line(&mut buf, &footer);

    buf.push_str("\x1b[J"); // clear anything below
    tty.write_all(buf.as_bytes())?;
    tty.flush()
}

/// The line under the title: the filter, or an invitation to open it.
fn search_line(f: &Frame, rows: &[Row], total: usize, width: usize, color: bool) -> String {
    let shown = rows.iter().filter(|r| matches!(r, Row::Item(..))).count();
    // The mark count lives here rather than in the title, which is the first
    // thing to be truncated when the path is long.
    let counts = if f.marked.is_empty() {
        format!("{shown}/{total} ")
    } else {
        format!("{} marked  \u{b7}  {shown}/{total} ", f.marked.len())
    };
    let (left, printable) = match (f.mode, f.query.is_empty()) {
        (Mode::Search, _) => (
            format!(" /{}\u{258c}", f.query),
            2 + f.query.chars().count() + 1,
        ),
        (Mode::Normal, false) => (
            format!(" {}", paint(DIM, &format!("/{}", f.query), color)),
            2 + f.query.chars().count(),
        ),
        (Mode::Normal, true) => (
            format!(" {}", paint(DIM, "press / to search", color)),
            1 + "press / to search".chars().count(),
        ),
    };
    let gap = width.saturating_sub(printable + counts.chars().count());
    format!("{left}{}{}", " ".repeat(gap), paint(DIM, &counts, color))
}

fn prompt_text(prompt: &Prompt) -> String {
    match prompt {
        Prompt::Delete(targets) => format!(
            "  delete {} worktree{} from disk?  [y] yes   [any other key] cancel",
            targets.len(),
            if targets.len() == 1 { "" } else { "s" }
        ),
        Prompt::Force(targets) => format!(
            "  force-remove {} worktree{}, losing uncommitted work in {}?  [f] yes   [any other key] cancel",
            targets.len(),
            if targets.len() == 1 { "" } else { "s" },
            if targets.len() == 1 { "it" } else { "them" }
        ),
    }
}

fn hints(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => {
            "  /: search  \u{b7}  space: mark  \u{b7}  d: delete  \u{b7}  enter: select  \u{b7}  s: sort  \u{b7}  o: PR  \u{b7}  q: quit"
        }
        Mode::Search => {
            "  type to filter  \u{b7}  enter/esc: back to the list  \u{b7}  \u{2191}/\u{2193}: move"
        }
    }
}

fn line(buf: &mut String, s: &str) {
    buf.push_str("\x1b[K");
    buf.push_str(s);
    buf.push_str("\r\n");
}

fn detail_text(app: &App, i: usize, now: u64) -> String {
    let wt = &app.worktrees[i];
    let mut parts = Vec::new();
    if let Some(touched) = app.statuses.get(&wt.path).and_then(|s| s.touched) {
        parts.push(format!("modified {}", age_phrase(now, touched)));
    }
    parts.push(match app.pr_for(wt) {
        PrCell::Open(pr) => format!(
            "#{}{} {}",
            pr.number,
            if pr.draft { " (draft)" } else { "" },
            pr.title
        ),
        PrCell::Loading => "looking up pull requests\u{2026}".to_string(),
        PrCell::Unavailable(why) => format!("no PR data: {why}"),
        PrCell::None if app.want_prs => "no open PR for this branch".to_string(),
        PrCell::None => crate::git::short_sha(&wt.head),
    });
    format!("  {}", parts.join("  \u{b7}  "))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn age_text(status: Option<&Status>, now: u64) -> String {
    match status {
        None => "\u{2026}".to_string(),
        Some(s) => match s.touched {
            Some(touched) => rel_age(now, touched),
            None => "-".to_string(),
        },
    }
}

/// Compact age for the column: `now`, `45m`, `3h`, `6d`, `3w`, `7mo`, `2y`.
fn rel_age(now: u64, then: u64) -> String {
    if then >= now {
        return "now".to_string();
    }
    let mins = (now - then) / 60;
    if mins < 1 {
        return "now".to_string();
    }
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d");
    }
    if days < 60 {
        return format!("{}w", days / 7);
    }
    if days < 730 {
        return format!("{}mo", days / 30);
    }
    format!("{}y", days / 365)
}

/// Spelled-out age for the detail line: `just now`, `5 minutes ago`, ...
fn age_phrase(now: u64, then: u64) -> String {
    if then >= now || now - then < 60 {
        return "just now".to_string();
    }
    let secs = now - then;
    for (unit, name) in [
        (60 * 60 * 24 * 365, "year"),
        (60 * 60 * 24 * 30, "month"),
        (60 * 60 * 24 * 7, "week"),
        (60 * 60 * 24, "day"),
        (60 * 60, "hour"),
        (60, "minute"),
    ] {
        let n = secs / unit;
        if n >= 1 {
            return format!("{n} {name}{} ago", if n == 1 { "" } else { "s" });
        }
    }
    "just now".to_string()
}

fn pr_text(cell: PrCell) -> String {
    match cell {
        PrCell::Open(pr) => format!("#{}", pr.number),
        PrCell::Loading => "\u{2026}".to_string(),
        _ => "-".to_string(),
    }
}

fn pr_color(cell: PrCell) -> &'static str {
    match cell {
        PrCell::Open(pr) if pr.draft => MAGENTA,
        PrCell::Open(_) => CYAN,
        _ => DIM,
    }
}

fn status_text(status: Option<&Status>, locked: bool, prunable: bool) -> String {
    if prunable {
        return "prunable".to_string();
    }
    let Some(s) = status else {
        return "\u{2026}".to_string();
    };
    if s.missing {
        return "missing".to_string();
    }
    let mut parts = Vec::new();
    if s.ahead > 0 {
        parts.push(format!("\u{2191}{}", s.ahead));
    }
    if s.behind > 0 {
        parts.push(format!("\u{2193}{}", s.behind));
    }
    if s.modified > 0 {
        parts.push(format!("\u{25cf}{}", s.modified));
    }
    if s.untracked > 0 {
        parts.push(format!("?{}", s.untracked));
    }
    if locked {
        parts.push("locked".to_string());
    }
    if parts.is_empty() {
        "clean".to_string()
    } else {
        parts.join(" ")
    }
}

fn status_color(status: Option<&Status>, prunable: bool) -> &'static str {
    if prunable {
        return RED;
    }
    match status {
        Some(s) if s.missing => RED,
        Some(s) if !s.is_clean() => YELLOW,
        Some(_) => DIM,
        None => DIM,
    }
}

fn paint(code: &str, s: &str, on: bool) -> String {
    if on {
        format!("\x1b[{code}m{s}{RESET}")
    } else {
        s.to_string()
    }
}

/// Pad to `w`, or truncate with a trailing ellipsis.
fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n == w {
        s.to_string()
    } else if n < w {
        format!("{s}{}", " ".repeat(w - n))
    } else if w == 0 {
        String::new()
    } else {
        let mut out: String = s.chars().take(w - 1).collect();
        out.push('\u{2026}');
        out
    }
}

/// Like `fit`, but keeps the end of the string — paths are more recognizable
/// by their tail.
fn fit_tail(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        return fit(s, w);
    }
    if w == 0 {
        return String::new();
    }
    let mut out = String::from("\u{2026}");
    out.extend(s.chars().skip(n - (w - 1)));
    out
}

/// Pad on the left instead of the right — the age column reads better aligned
/// to its numbers.
fn fit_right(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        fit(s, w)
    } else {
        format!("{}{s}", " ".repeat(w - n))
    }
}

/// `fit`/`fit_tail` with the query's matched characters highlighted, over a
/// `base` style that is restored after each highlighted run.
fn fit_hl(s: &str, w: usize, hits: &[usize], tail: bool, base: &str) -> String {
    if hits.is_empty() {
        let fitted = if tail { fit_tail(s, w) } else { fit(s, w) };
        return paint(base, &fitted, !base.is_empty());
    }
    if w == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    // Mirror the window fit/fit_tail would have shown, so hit offsets line up.
    let (start, end, prefix, suffix) = if n <= w {
        (0, n, "", "")
    } else if tail {
        (n - (w - 1), n, "\u{2026}", "")
    } else {
        (0, w - 1, "", "\u{2026}")
    };

    let restore = if base.is_empty() {
        RESET.to_string()
    } else {
        format!("\x1b[{base}m")
    };
    let mut out = String::new();
    if !base.is_empty() {
        out.push_str(&restore);
    }
    out.push_str(prefix);
    let mut lit = false;
    for (i, ch) in chars.iter().enumerate().take(end).skip(start) {
        let hit = hits.binary_search(&i).is_ok();
        if hit && !lit {
            out.push_str(&format!("\x1b[{HL}m"));
            lit = true;
        } else if !hit && lit {
            out.push_str(&restore);
            lit = false;
        }
        out.push(*ch);
    }
    if lit || !base.is_empty() {
        out.push_str(RESET);
    }
    out.push_str(suffix);

    let shown = (end - start) + prefix.chars().count() + suffix.chars().count();
    if shown < w {
        out.push_str(&" ".repeat(w - shown));
    }
    out
}

pub fn tty_available() -> bool {
    Path::new("/dev/tty").exists() && File::open("/dev/tty").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip SGR escapes, leaving what the terminal actually shows.
    fn visible(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn fit_pads_and_truncates_to_exact_width() {
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(fit("abcd", 4), "abcd");
        assert_eq!(fit("abcdef", 4), "abc\u{2026}");
        assert_eq!(fit("abc", 0), "");
        // Multi-byte characters count as one column, not three.
        assert_eq!(fit("\u{2191}2", 4).chars().count(), 4);
    }

    #[test]
    fn fit_tail_keeps_the_end_of_a_path() {
        assert_eq!(fit_tail("/a/b/c", 8), "/a/b/c  ");
        assert_eq!(fit_tail("/very/long/path", 6), "\u{2026}/path");
        assert_eq!(fit_tail("/very/long/path", 0), "");
    }

    #[test]
    fn highlighting_does_not_change_the_visible_text() {
        for (s, w, hits, tail) in [
            ("brollb/loops", 20, vec![7, 8, 9, 10, 11], false),
            ("brollb/loops", 6, vec![0, 1], false),
            ("/a/very/long/path", 8, vec![13, 14, 15, 16], true),
            ("exact", 5, vec![0, 4], false),
        ] {
            let hl = fit_hl(s, w, &hits, tail, DIM);
            let plain = if tail { fit_tail(s, w) } else { fit(s, w) };
            assert_eq!(visible(&hl), plain, "{s:?} at width {w}");
            assert_eq!(visible(&hl).chars().count(), w);
        }
    }

    #[test]
    fn highlighting_wraps_only_the_matched_run() {
        let out = fit_hl("abcd", 4, &[1, 2], false, "");
        assert!(out.contains(&format!("\x1b[{HL}mbc")), "got {out:?}");
        assert_eq!(visible(&out), "abcd");
    }

    #[test]
    fn navigation_skips_repo_headers() {
        let rows = vec![
            Row::Header("a".into()),
            Row::Item(0, Hits::default()),
            Row::Header("b".into()),
            Row::Item(1, Hits::default()),
        ];
        assert_eq!(first_item(&rows), 1);
        assert_eq!(last_item(&rows), 3);
        assert_eq!(step(&rows, 1, 1), 3, "header between items is skipped");
        assert_eq!(step(&rows, 3, 1), 3, "stays put at the end");
        assert_eq!(step(&rows, 1, -1), 1, "stays put at the start");
        assert!(matches!(rows[jump(&rows, 1, 10)], Row::Item(..)));
        assert!(matches!(rows[jump(&rows, 3, -10)], Row::Item(..)));
    }

    #[test]
    fn status_cell_summarizes_divergence_and_flags() {
        let mut s = Status::default();
        assert_eq!(status_text(Some(&s), false, false), "clean");
        assert_eq!(status_text(None, false, false), "\u{2026}");
        s.ahead = 2;
        s.modified = 3;
        assert_eq!(status_text(Some(&s), false, false), "\u{2191}2 \u{25cf}3");
        assert_eq!(
            status_text(Some(&s), true, false),
            "\u{2191}2 \u{25cf}3 locked"
        );
        assert_eq!(status_text(Some(&s), false, true), "prunable");
        assert_eq!(
            status_text(
                Some(&Status {
                    missing: true,
                    ..Default::default()
                }),
                false,
                false
            ),
            "missing"
        );
    }

    #[test]
    fn fit_right_aligns_within_the_column() {
        assert_eq!(fit_right("3d", 5), "   3d");
        assert_eq!(fit_right("12mo", 5), " 12mo");
        assert_eq!(fit_right("toolong", 5), "tool\u{2026}");
    }

    #[test]
    fn rel_age_picks_the_largest_fitting_unit() {
        const MIN: u64 = 60;
        const HOUR: u64 = 60 * MIN;
        const DAY: u64 = 24 * HOUR;
        let now = 1_000_000_000;
        assert_eq!(rel_age(now, now), "now");
        assert_eq!(rel_age(now, now - 30), "now");
        assert_eq!(rel_age(now, now - 5 * MIN), "5m");
        assert_eq!(rel_age(now, now - 59 * MIN), "59m");
        assert_eq!(rel_age(now, now - 3 * HOUR), "3h");
        assert_eq!(rel_age(now, now - 6 * DAY), "6d");
        assert_eq!(rel_age(now, now - 20 * DAY), "2w");
        assert_eq!(rel_age(now, now - 200 * DAY), "6mo");
        assert_eq!(rel_age(now, now - 1000 * DAY), "2y");
        // A worktree touched by a clock ahead of ours must not underflow.
        assert_eq!(rel_age(now, now + 5000), "now");
        // Every result has to fit the column.
        for ago in [0, 90, 4000, 100_000, 9_000_000, 400_000_000] {
            assert!(rel_age(now, now - ago).chars().count() <= 5, "{ago}");
        }
    }

    #[test]
    fn age_phrase_pluralizes() {
        let now = 1_000_000_000;
        assert_eq!(age_phrase(now, now - 10), "just now");
        assert_eq!(age_phrase(now, now - 60), "1 minute ago");
        assert_eq!(age_phrase(now, now - 120), "2 minutes ago");
        assert_eq!(age_phrase(now, now - 86_400), "1 day ago");
        assert_eq!(age_phrase(now, now - 3 * 86_400), "3 days ago");
        assert_eq!(age_phrase(now, now + 99), "just now");
    }

    #[test]
    fn age_cell_shows_loading_and_unknown_distinctly() {
        let now = 1_000_000_000;
        assert_eq!(age_text(None, now), "\u{2026}");
        assert_eq!(age_text(Some(&Status::default()), now), "-");
        let touched = Status {
            touched: Some(now - 7200),
            ..Default::default()
        };
        assert_eq!(age_text(Some(&touched), now), "2h");
    }

    fn frame<'a>(query: &'a str, mode: Mode, marked: &'a HashSet<PathBuf>) -> Frame<'a> {
        static NONE: std::sync::OnceLock<HashSet<PathBuf>> = std::sync::OnceLock::new();
        Frame {
            rows: &[],
            query,
            mode,
            sel: 0,
            off: 0,
            view: 10,
            width: 60,
            marked,
            removing: NONE.get_or_init(HashSet::new),
            progress: "",
            prompt: None,
            message: None,
        }
    }

    #[test]
    fn search_line_is_exactly_one_row_wide_in_every_mode() {
        let rows = vec![Row::Item(0, Hits::default())];
        let marked = HashSet::new();
        for mode in [Mode::Normal, Mode::Search] {
            for query in ["", "loops", "a b"] {
                let f = frame(query, mode, &marked);
                let out = search_line(&f, &rows, 42, 60, true);
                assert_eq!(visible(&out).chars().count(), 60, "{query:?}");
            }
        }
    }

    #[test]
    fn search_line_says_how_to_start_searching() {
        let marked = HashSet::new();
        let rows = vec![Row::Item(0, Hits::default())];
        let idle = visible(&search_line(
            &frame("", Mode::Normal, &marked),
            &rows,
            1,
            60,
            false,
        ));
        assert!(idle.contains("press / to search"), "{idle:?}");
        // A filter that is applied but not being edited still shows itself.
        let applied = visible(&search_line(
            &frame("loops", Mode::Normal, &marked),
            &rows,
            1,
            60,
            false,
        ));
        assert!(applied.contains("/loops"), "{applied:?}");
        assert!(!applied.contains('\u{258c}'), "no caret when not editing");
        let editing = visible(&search_line(
            &frame("loops", Mode::Search, &marked),
            &rows,
            1,
            60,
            false,
        ));
        assert!(editing.contains("/loops\u{258c}"), "{editing:?}");
    }

    fn app_with(paths: &[&str]) -> App {
        use crate::git::{Repo, Worktree};
        let worktrees: Vec<Worktree> = paths
            .iter()
            .enumerate()
            .map(|(n, p)| Worktree {
                path: PathBuf::from(p),
                head: "abc".to_string(),
                branch: Some(format!("b{n}")),
                bare: false,
                detached: false,
                locked: false,
                prunable: false,
                repo: 0,
                main: n == 0,
            })
            .collect();
        let repo = Repo {
            label: "r".to_string(),
            main_path: PathBuf::from("/r"),
            worktrees,
        };
        App::new(vec![repo], PathBuf::from("/r"), false, false)
    }

    #[test]
    fn delete_acts_on_the_marks_or_else_the_cursor() {
        let app = app_with(&["/r", "/r/a", "/r/b"]);
        let rows = build_rows(&app, "");
        let cursor_on = |sel: usize| match &rows[sel] {
            Row::Item(i, _) => app.worktrees[*i].path.clone(),
            _ => panic!("not an item"),
        };

        // Nothing marked: just the row under the cursor.
        let empty = HashSet::new();
        assert_eq!(delete_targets(&app, &empty, &rows, 1), vec![cursor_on(1)]);

        // Marked rows win, and come back in list order regardless of the
        // order they were marked in.
        let marked: HashSet<PathBuf> = [PathBuf::from("/r/b"), PathBuf::from("/r/a")]
            .into_iter()
            .collect();
        assert_eq!(
            delete_targets(&app, &marked, &rows, 0),
            vec![PathBuf::from("/r/a"), PathBuf::from("/r/b")],
            "the cursor row is ignored when marks exist"
        );
    }

    #[test]
    fn the_main_worktree_is_refused_before_git_is_asked() {
        let app = app_with(&["/r", "/r/a"]);
        let main = app.worktrees.iter().find(|w| w.main).unwrap();
        let linked = app.worktrees.iter().find(|w| !w.main).unwrap();
        assert!(cannot_remove(main).unwrap().contains("main worktree"));
        assert_eq!(cannot_remove(linked), None);
    }

    #[test]
    fn a_batch_tracks_progress_and_finishes_only_when_all_report() {
        let mut batch = Batch::starting(3);
        for p in ["/a", "/b", "/c"] {
            batch.pending.insert(PathBuf::from(p));
        }
        assert!(batch.in_flight());
        assert_eq!(batch.progress(), "  removing 0 of 3\u{2026}");

        batch.pending.remove(&PathBuf::from("/a"));
        batch.removed.push(PathBuf::from("/a"));
        assert_eq!(batch.progress(), "  removing 1 of 3\u{2026}");
        assert!(batch.in_flight(), "two are still running");

        batch.pending.remove(&PathBuf::from("/b"));
        batch
            .failed
            .push((PathBuf::from("/b"), "dirty".to_string()));
        batch.pending.remove(&PathBuf::from("/c"));
        batch.removed.push(PathBuf::from("/c"));
        assert!(!batch.in_flight());

        let summary = batch.outcome().summary();
        assert!(summary.contains("removed 2 worktrees"), "{summary:?}");
        assert!(summary.contains("1 refused: dirty"), "{summary:?}");
    }

    #[test]
    fn a_worktree_we_refuse_ourselves_is_not_offered_for_forcing() {
        // Marking everything includes the main worktree, which we never
        // dispatch: it should not inflate the progress count, and offering to
        // force it would just fail again.
        let mut batch = Batch::starting(2);
        batch.total -= 1;
        batch.refused.push((
            PathBuf::from("/r"),
            "a repo's main worktree cannot be removed".to_string(),
        ));
        batch.pending.insert(PathBuf::from("/r/a"));
        assert_eq!(batch.progress(), "  removing 0 of 1\u{2026}");

        batch.pending.remove(&PathBuf::from("/r/a"));
        batch.removed.push(PathBuf::from("/r/a"));
        assert!(batch.failed.is_empty(), "nothing for the force prompt");
        let summary = batch.outcome().summary();
        assert!(summary.contains("removed 1 worktree"), "{summary:?}");
        assert!(summary.contains("main worktree"), "{summary:?}");
    }

    #[test]
    fn confirmations_name_what_will_happen() {
        let one = prompt_text(&Prompt::Delete(vec![PathBuf::from("/a")]));
        assert!(one.contains("delete 1 worktree from disk?"), "{one:?}");
        let many = prompt_text(&Prompt::Delete(vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
        ]));
        assert!(many.contains("delete 2 worktrees"), "{many:?}");
        // Forcing has to spell out that work is lost.
        let forced = prompt_text(&Prompt::Force(vec![PathBuf::from("/a")]));
        assert!(forced.contains("losing uncommitted work"), "{forced:?}");
        assert!(forced.contains("[f]"), "{forced:?}");
    }

    #[test]
    fn removal_summary_reports_both_halves() {
        let mut outcome = Removal::default();
        assert!(outcome.summary().contains("nothing removed"));
        outcome.removed.push(PathBuf::from("/a"));
        assert!(outcome.summary().contains("removed 1 worktree"));
        outcome.removed.push(PathBuf::from("/b"));
        assert!(outcome.summary().contains("removed 2 worktrees"));
        outcome
            .failed
            .push((PathBuf::from("/c"), "contains modified files".to_string()));
        let summary = outcome.summary();
        assert!(summary.contains("removed 2 worktrees"), "{summary:?}");
        assert!(
            summary.contains("1 refused: contains modified files"),
            "{summary:?}"
        );
    }
}
