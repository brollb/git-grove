//! git-worktrees — list git worktrees, annotated with open GitHub PR numbers.
//!
//! On a TTY it opens an interactive picker; piped, it prints tab-separated
//! lines (or JSON with `--json`).

mod app;
mod fuzzy;
mod gh;
mod git;
mod load;
mod ui;

use std::env;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use app::{display_path, App, PrCell};
use load::Loader;

const USAGE: &str = "\
git-worktrees — list git worktrees, with open PR numbers

USAGE:
    git-worktrees [OPTIONS] [DIRECTORY]

DIRECTORY defaults to the current directory. If it is inside a git repo, that
repo's worktrees are listed; otherwise every repo directly beneath it is scanned.

Interactive by default on a TTY: type to fuzzy-filter, arrows to move, enter
prints the selected worktree path, ctrl-o opens its PR, esc clears the filter
or quits.

OPTIONS:
    -c, --cd        open a shell in the worktree you select, and return to the
                    picker when that shell exits (ctrl-d)
    -q, --query Q   start with the filter box pre-filled; filters --plain and
                    --json output too
    -p, --pick      force the interactive picker, printing the selection to
                    stdout even when stdout is redirected
                        cd \"$(git-worktrees --pick)\"
        --plain     force tab-separated output:
                        path <TAB> branch <TAB> head <TAB> pr <TAB> flags
    -j, --json      JSON output
    -s, --status    include working-tree status in --plain/--json output
                    (the picker always loads it, lazily)
        --no-pr     skip the GitHub PR lookup
    -h, --help      print this help
    -V, --version   print the version

EXIT CODES:
    0  listed, or a worktree was selected
    1  no worktrees found, or a fatal error
    2  bad usage
  130  the picker was cancelled
";

struct Opts {
    dir: PathBuf,
    json: bool,
    plain: bool,
    pick: bool,
    cd: bool,
    prs: bool,
    status: bool,
    query: String,
}

fn parse_args() -> Result<Option<Opts>, String> {
    let mut opts = Opts {
        dir: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        json: false,
        plain: false,
        pick: false,
        cd: false,
        prs: true,
        status: false,
        query: String::new(),
    };
    let mut dir_set = false;
    let mut want_query = false;

    for arg in env::args().skip(1) {
        if want_query {
            opts.query = arg;
            want_query = false;
            continue;
        }
        if let Some(q) = arg
            .strip_prefix("--query=")
            .or_else(|| arg.strip_prefix("-q="))
        {
            opts.query = q.to_string();
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("git-worktrees {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-j" | "--json" => opts.json = true,
            "--plain" => opts.plain = true,
            "-p" | "--pick" => opts.pick = true,
            "-c" | "--cd" => opts.cd = true,
            "-q" | "--query" => want_query = true,
            "-s" | "--status" => opts.status = true,
            "--no-pr" | "--no-prs" => opts.prs = false,
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown option: {other}"));
            }
            other => {
                if dir_set {
                    return Err("expected at most one DIRECTORY".to_string());
                }
                opts.dir = PathBuf::from(other);
                dir_set = true;
            }
        }
    }
    if want_query {
        return Err("--query needs a value".to_string());
    }
    Ok(Some(opts))
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(Some(opts)) => opts,
        Ok(None) => return ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("git-worktrees: {err}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let dir = match std::fs::canonicalize(&opts.dir) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("git-worktrees: {}: {err}", opts.dir.display());
            return ExitCode::from(1);
        }
    };

    let repos = git::discover(&dir);
    if repos.is_empty() {
        eprintln!("git-worktrees: no git worktrees found in {}", dir.display());
        return ExitCode::from(1);
    }

    let interactive =
        (opts.pick || opts.cd || (std::io::stdout().is_terminal() && !opts.plain && !opts.json))
            && ui::tty_available();
    let color = interactive
        && env::var_os("NO_COLOR").is_none()
        && env::var("TERM").as_deref() != Ok("dumb");

    let mut app = App::new(repos, dir, opts.prs, color);
    let (mut loader, rx) = Loader::new(8);
    if opts.prs {
        for (i, repo) in app.repos.iter().enumerate() {
            loader.spawn_prs(i, repo.main_path.clone());
        }
    }

    if interactive {
        let mut state = ui::PickerState::with_query(opts.query.clone());
        if opts.cd {
            return shell_loop(&mut app, &mut loader, &rx, &mut state);
        }
        return match ui::run(&mut app, &mut loader, &rx, &mut state) {
            Ok(Some(path)) => {
                println!("{}", path.display());
                ExitCode::SUCCESS
            }
            Ok(None) => ExitCode::from(130),
            Err(err) => {
                eprintln!("git-worktrees: {err}");
                ExitCode::from(1)
            }
        };
    }

    // Non-interactive: everything has to be resolved before we can print.
    let mut outstanding = if opts.prs { app.repos.len() } else { 0 };
    if opts.status {
        for wt in &app.worktrees {
            loader.request_status(&wt.path);
        }
        outstanding += loader.pending();
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    while outstanding > 0 {
        let now = Instant::now();
        if now >= deadline {
            eprintln!("git-worktrees: timed out waiting for status/PR data");
            break;
        }
        match rx.recv_timeout(deadline - now) {
            Ok(msg) => {
                app.apply(msg);
                outstanding -= 1;
            }
            Err(_) => {
                eprintln!("git-worktrees: timed out waiting for status/PR data");
                break;
            }
        }
    }

    let selection: Vec<usize> = app
        .filter(&opts.query)
        .into_iter()
        .map(|(i, _)| i)
        .collect();
    if opts.json {
        print_json(&app, &selection, opts.status);
    } else {
        print_plain(&app, &selection, opts.status);
    }
    ExitCode::SUCCESS
}

/// `--cd`: hand the selected worktree to a shell, and come back to the picker
/// when that shell exits, until the user quits the picker itself.
fn shell_loop(
    app: &mut App,
    loader: &mut load::Loader,
    rx: &std::sync::mpsc::Receiver<load::Msg>,
    state: &mut ui::PickerState,
) -> ExitCode {
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    loop {
        let path = match ui::run(app, loader, rx, state) {
            Ok(Some(path)) => path,
            Ok(None) => return ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("git-worktrees: {err}");
                return ExitCode::from(1);
            }
        };
        eprintln!("\x1b[90m\u{2192} {}\x1b[0m", path.display());
        if let Err(err) = Command::new(&shell).current_dir(&path).status() {
            eprintln!("git-worktrees: {shell} in {}: {err}", path.display());
        }
        // Whatever happened in there, the worktree's status is now suspect.
        app.statuses.remove(&path);
        loader.forget(&path);
    }
}

fn print_plain(app: &App, selection: &[usize], with_status: bool) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for wt in selection.iter().map(|&i| &app.worktrees[i]) {
        let pr = match app.pr_for(wt) {
            PrCell::Open(pr) => format!("#{}", pr.number),
            _ => "-".to_string(),
        };
        let mut flags = Vec::new();
        if wt.main {
            flags.push("main".to_string());
        }
        if wt.bare {
            flags.push("bare".to_string());
        }
        if wt.detached {
            flags.push("detached".to_string());
        }
        if wt.locked {
            flags.push("locked".to_string());
        }
        if wt.prunable {
            flags.push("prunable".to_string());
        }
        if matches!(app.pr_for(wt), PrCell::Open(pr) if pr.draft) {
            flags.push("draft".to_string());
        }
        if with_status {
            if let Some(s) = app.statuses.get(&wt.path) {
                if s.missing {
                    flags.push("missing".to_string());
                } else {
                    flags.push(if s.is_clean() { "clean" } else { "dirty" }.to_string());
                    if s.modified > 0 {
                        flags.push(format!("modified={}", s.modified));
                    }
                    if s.untracked > 0 {
                        flags.push(format!("untracked={}", s.untracked));
                    }
                    if s.ahead > 0 {
                        flags.push(format!("ahead={}", s.ahead));
                    }
                    if s.behind > 0 {
                        flags.push(format!("behind={}", s.behind));
                    }
                }
            }
        }
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}",
            wt.path.display(),
            wt.branch.clone().unwrap_or_else(|| "-".to_string()),
            git::short_sha(&wt.head),
            pr,
            if flags.is_empty() {
                "-".to_string()
            } else {
                flags.join(",")
            },
        );
    }
}

fn print_json(app: &App, selection: &[usize], with_status: bool) {
    let items: Vec<serde_json::Value> = selection
        .iter()
        .map(|&i| &app.worktrees[i])
        .map(|wt| {
            let mut obj = serde_json::json!({
                "repo": app.repos[wt.repo].label,
                "path": wt.path.display().to_string(),
                "display_path": display_path(&wt.path, Some(&app.root)),
                "branch": wt.branch,
                "head": wt.head,
                "main": wt.main,
                "bare": wt.bare,
                "detached": wt.detached,
                "locked": wt.locked,
                "prunable": wt.prunable,
                "pr": match app.pr_for(wt) {
                    PrCell::Open(pr) => serde_json::json!({
                        "number": pr.number,
                        "draft": pr.draft,
                        "title": pr.title,
                        "url": pr.url,
                    }),
                    _ => serde_json::Value::Null,
                },
            });
            if with_status {
                if let Some(s) = app.statuses.get(&wt.path) {
                    obj["status"] = serde_json::json!({
                        "missing": s.missing,
                        "clean": s.is_clean(),
                        "modified": s.modified,
                        "untracked": s.untracked,
                        "ahead": s.ahead,
                        "behind": s.behind,
                    });
                }
            }
            obj
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&items).unwrap_or_default()
    );
}
