//! Interactive worktree picker, drawn on /dev/tty.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::{execute, terminal};

use crate::app::{App, PrCell};
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

enum Row {
    Header(String),
    Item(usize),
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

pub fn run(app: &mut App, loader: &mut Loader, rx: &Receiver<Msg>) -> io::Result<Option<PathBuf>> {
    let rows = build_rows(app);
    if !rows.iter().any(|r| matches!(r, Row::Item(_))) {
        return Ok(None);
    }

    let mut tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    let _restore = Restore {
        tty: tty.try_clone()?,
    };

    terminal::enable_raw_mode()?;
    execute!(tty, terminal::EnterAlternateScreen, crossterm::cursor::Hide)?;

    let mut sel = first_item(&rows);
    let mut off = 0usize;
    let mut dirty = true;

    loop {
        let (width, height) = size();
        let view = height.saturating_sub(6).max(1); // title + 2 header lines + 3 footer

        if sel < off {
            off = sel;
        } else if sel >= off + view {
            off = sel + 1 - view;
        }
        off = off.min(rows.len().saturating_sub(view));

        // Only the rows on screen pay for a `git status`.
        for row in rows.iter().skip(off).take(view) {
            if let Row::Item(i) = row {
                let path = &app.worktrees[*i].path;
                if !app.statuses.contains_key(path) {
                    loader.request_status(path);
                }
            }
        }

        if dirty {
            draw(&mut tty, app, &rows, sel, off, view, width)?;
            dirty = false;
        }

        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    match key.code {
                        KeyCode::Char('c') if ctrl => return Ok(None),
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                        KeyCode::Enter => {
                            if let Row::Item(i) = rows[sel] {
                                return Ok(Some(app.worktrees[i].path.clone()));
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
                        KeyCode::Char('o') => {
                            if let Row::Item(i) = rows[sel] {
                                if let PrCell::Open(pr) = app.pr_for(&app.worktrees[i]) {
                                    gh::open_url(&pr.url);
                                }
                            }
                        }
                        _ => {}
                    }
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }

        while let Ok(msg) = rx.try_recv() {
            app.apply(msg);
            dirty = true;
        }
    }
}

fn build_rows(app: &App) -> Vec<Row> {
    let mut rows = Vec::new();
    let multi = app.repos.len() > 1;
    let mut last_repo = usize::MAX;
    for (i, wt) in app.worktrees.iter().enumerate() {
        if multi && wt.repo != last_repo {
            rows.push(Row::Header(app.repos[wt.repo].label.clone()));
            last_repo = wt.repo;
        }
        rows.push(Row::Item(i));
    }
    rows
}

fn first_item(rows: &[Row]) -> usize {
    rows.iter()
        .position(|r| matches!(r, Row::Item(_)))
        .unwrap_or(0)
}

fn last_item(rows: &[Row]) -> usize {
    rows.iter()
        .rposition(|r| matches!(r, Row::Item(_)))
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
        if matches!(rows[i as usize], Row::Item(_)) {
            return i as usize;
        }
    }
}

/// Move roughly `n` rows, then settle on the nearest selectable row.
fn jump(rows: &[Row], from: usize, n: isize) -> usize {
    let target = (from as isize + n).clamp(0, rows.len() as isize - 1) as usize;
    if matches!(rows[target], Row::Item(_)) {
        return target;
    }
    let back = if n < 0 { 1 } else { -1 };
    let settled = step(rows, target, if n < 0 { -1 } else { 1 });
    if matches!(rows[settled], Row::Item(_)) && settled != target {
        settled
    } else {
        step(rows, target, back)
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
    path: usize,
}

fn widths(app: &App, total: usize) -> Widths {
    let pr = 7;
    let status = 14;
    let fixed = 2 + pr + 2 + status + 2 + 2; // marker + gaps
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
        path: flex.saturating_sub(branch).max(8),
    }
}

fn draw(
    tty: &mut File,
    app: &App,
    rows: &[Row],
    sel: usize,
    off: usize,
    view: usize,
    width: usize,
) -> io::Result<()> {
    let w = widths(app, width);
    let color = app.color;
    let mut buf = String::new();
    buf.push_str("\x1b[H"); // home

    let total = app.worktrees.len();
    let with_prs = app
        .worktrees
        .iter()
        .filter(|wt| matches!(app.pr_for(wt), PrCell::Open(_)))
        .count();
    let title = format!(
        " {} worktree{} in {}{}",
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
    line(&mut buf, "");
    line(
        &mut buf,
        &paint(
            DIM,
            &fit(
                &format!(
                    "  {}  {}  {}  {}",
                    fit("PR", w.pr),
                    fit("BRANCH", w.branch),
                    fit("STATUS", w.status),
                    "PATH"
                ),
                width,
            ),
            color,
        ),
    );

    for (idx, row) in rows.iter().enumerate().skip(off).take(view) {
        match row {
            Row::Header(label) => {
                line(
                    &mut buf,
                    &paint(BOLD, &fit(&format!(" {label}"), width), color),
                );
            }
            Row::Item(i) => {
                let wt = &app.worktrees[*i];
                let selected = idx == sel;
                let pr_cell = fit(&pr_text(app.pr_for(wt)), w.pr);
                let branch_cell = fit(&wt.branch_label(), w.branch);
                let status_cell = fit(
                    &status_text(app.statuses.get(&wt.path), wt.locked, wt.prunable),
                    w.status,
                );
                let path_cell =
                    fit_tail(&crate::app::display_path(&wt.path, Some(&app.root)), w.path);
                let marker = if selected { "\u{25b8} " } else { "  " };
                let plain = fit(
                    &format!("{marker}{pr_cell}  {branch_cell}  {status_cell}  {path_cell}"),
                    width,
                );
                if selected {
                    // Reverse video over the whole row, so per-cell colors
                    // (and their resets) can't punch holes in the highlight.
                    line(&mut buf, &paint("7", &plain, color));
                } else if color {
                    let styled = format!(
                        "{marker}{}  {}  {}  {}",
                        paint(pr_color(app.pr_for(wt)), &pr_cell, true),
                        branch_cell,
                        paint(
                            status_color(app.statuses.get(&wt.path), wt.prunable),
                            &status_cell,
                            true
                        ),
                        paint(DIM, &path_cell, true),
                    );
                    line(&mut buf, &styled);
                } else {
                    line(&mut buf, &plain);
                }
            }
        }
    }

    // Pad out to the footer.
    for _ in (off + view).min(rows.len())..(off + view) {
        line(&mut buf, "");
    }

    line(&mut buf, "");
    let detail = match rows.get(sel) {
        Some(Row::Item(i)) => detail_text(app, *i),
        _ => String::new(),
    };
    line(&mut buf, &fit(&detail, width));
    line(
        &mut buf,
        &paint(
            DIM,
            &fit(
                "  \u{2191}/\u{2193} move  ·  enter: print path  ·  o: open PR  ·  q: quit",
                width,
            ),
            color,
        ),
    );

    buf.push_str("\x1b[J"); // clear anything below
    tty.write_all(buf.as_bytes())?;
    tty.flush()
}

fn line(buf: &mut String, s: &str) {
    buf.push_str("\x1b[K");
    buf.push_str(s);
    buf.push_str("\r\n");
}

fn detail_text(app: &App, i: usize) -> String {
    let wt = &app.worktrees[i];
    match app.pr_for(wt) {
        PrCell::Open(pr) => format!(
            "  #{}{} {}",
            pr.number,
            if pr.draft { " (draft)" } else { "" },
            pr.title
        ),
        PrCell::Loading => "  looking up pull requests\u{2026}".to_string(),
        PrCell::Unavailable(why) => format!("  no PR data: {why}"),
        PrCell::None if app.want_prs => "  no open PR for this branch".to_string(),
        PrCell::None => format!("  {}", crate::git::short_sha(&wt.head)),
    }
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

pub fn tty_available() -> bool {
    Path::new("/dev/tty").exists() && File::open("/dev/tty").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn navigation_skips_repo_headers() {
        let rows = vec![
            Row::Header("a".into()),
            Row::Item(0),
            Row::Header("b".into()),
            Row::Item(1),
        ];
        assert_eq!(first_item(&rows), 1);
        assert_eq!(last_item(&rows), 3);
        assert_eq!(step(&rows, 1, 1), 3, "header between items is skipped");
        assert_eq!(step(&rows, 3, 1), 3, "stays put at the end");
        assert_eq!(step(&rows, 1, -1), 1, "stays put at the start");
        assert!(matches!(rows[jump(&rows, 1, 10)], Row::Item(_)));
        assert!(matches!(rows[jump(&rows, 3, -10)], Row::Item(_)));
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
}
