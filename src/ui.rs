//! Interactive worktree picker, drawn on /dev/tty.

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
const RED: &str = "31";
const DIM: &str = "90";
const BOLD: &str = "1";
/// Matched characters of the query.
const HL: &str = "1;36";

enum Row {
    Header(String),
    Item(usize, Hits),
}

/// Query and cursor, kept across picker visits so that returning from a
/// worktree shell (`--cd`) lands you back where you were.
#[derive(Default)]
pub struct PickerState {
    pub query: String,
    sel: usize,
    off: usize,
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
            draw(&mut tty, app, &rows, &state.query, sel, off, view, width)?;
            dirty = false;
        }

        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    let alt = key.modifiers.contains(KeyModifiers::ALT);
                    let mut requery = false;
                    match key.code {
                        KeyCode::Char('c' | 'd') if ctrl => {
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
                        KeyCode::Char('s') if ctrl => {
                            app.sort = app.sort.next();
                            if app.sort != Sort::Name {
                                // Sorting by age needs every row dated, not
                                // just the ones on screen.
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
                        KeyCode::Char('o') if ctrl => {
                            if let Some(Row::Item(i, _)) = rows.get(sel) {
                                if let PrCell::Open(pr) = app.pr_for(&app.worktrees[*i]) {
                                    gh::open_url(&pr.url);
                                }
                            }
                        }
                        KeyCode::Up => sel = step(&rows, sel, -1),
                        KeyCode::Down => sel = step(&rows, sel, 1),
                        KeyCode::Char('p') if ctrl => sel = step(&rows, sel, -1),
                        KeyCode::Char('n') if ctrl => sel = step(&rows, sel, 1),
                        KeyCode::PageUp => sel = jump(&rows, sel, -(view as isize)),
                        KeyCode::PageDown => sel = jump(&rows, sel, view as isize),
                        KeyCode::Home => sel = first_item(&rows),
                        KeyCode::End => sel = last_item(&rows),
                        // Anything else printable extends the query.
                        KeyCode::Char(c) if !ctrl && !alt => {
                            state.query.push(c);
                            requery = true;
                        }
                        _ => {}
                    }
                    if requery {
                        rows = build_rows(app, &state.query);
                        sel = first_item(&rows);
                        off = 0;
                    }
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }

        let mut prs_arrived = false;
        let mut ages_arrived = false;
        while let Ok(msg) = rx.try_recv() {
            prs_arrived |= matches!(msg, Msg::Prs(..));
            ages_arrived |= matches!(msg, Msg::Age(..) | Msg::Status(..));
            app.apply(msg);
            dirty = true;
        }
        // PR numbers are searchable, so late-arriving PRs can change the match
        // set; newly dated rows can change a recency sort's order.
        let resort = ages_arrived && app.sort != Sort::Name;
        if resort || (prs_arrived && !state.query.trim().is_empty()) {
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
    let fixed = 2 + pr + 2 + status + 2 + age + 2 + 2; // marker + gaps
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

#[allow(clippy::too_many_arguments)]
fn draw(
    tty: &mut File,
    app: &App,
    rows: &[Row],
    query: &str,
    sel: usize,
    off: usize,
    view: usize,
    width: usize,
) -> io::Result<()> {
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
    line(&mut buf, &prompt_line(query, rows, total, width, color));
    line(
        &mut buf,
        &paint(
            DIM,
            &fit(
                &format!(
                    "  {}  {}  {}  {}  {}",
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
                let status_cell = fit(&status_text(status, wt.locked, wt.prunable), w.status);
                let age_cell = fit_right(&age_text(status, now), w.age);
                let branch = wt.branch_label();
                let path = crate::app::display_path(&wt.path, Some(&app.root));
                let marker = if selected { "\u{25b8} " } else { "  " };

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
    line(
        &mut buf,
        &paint(
            DIM,
            &fit(
                "  type to filter  ·  \u{2191}/\u{2193} move  ·  enter: select  ·  ctrl-s: sort  ·  ctrl-o: PR  ·  esc: clear/quit",
                width,
            ),
            color,
        ),
    );

    buf.push_str("\x1b[J"); // clear anything below
    tty.write_all(buf.as_bytes())?;
    tty.flush()
}

fn prompt_line(query: &str, rows: &[Row], total: usize, width: usize, color: bool) -> String {
    let shown = rows.iter().filter(|r| matches!(r, Row::Item(..))).count();
    let counts = format!("{shown}/{total} ");
    let left = if query.is_empty() {
        format!(" \u{203a} {}", paint(DIM, "type to filter", color))
    } else {
        format!(" \u{203a} {query}\u{258c}")
    };
    // `left` may carry escape codes, so pad against its printable width.
    let printable = if query.is_empty() {
        3 + "type to filter".chars().count()
    } else {
        3 + query.chars().count() + 1
    };
    let gap = width.saturating_sub(printable + counts.chars().count());
    format!("{left}{}{}", " ".repeat(gap), paint(DIM, &counts, color))
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

    #[test]
    fn prompt_line_is_exactly_one_row_wide() {
        let rows = vec![Row::Item(0, Hits::default())];
        for query in ["", "loops", "a b"] {
            let out = prompt_line(query, &rows, 42, 60, true);
            assert_eq!(visible(&out).chars().count(), 60, "query {query:?}");
        }
    }
}
