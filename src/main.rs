//! grove — browse the git worktrees of a repo, annotated with open PR numbers.
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
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use app::{display_path, App, PrCell, Sort};
use load::Loader;

const USAGE: &str = "\
{name} — browse, search and prune the git worktrees of a repo

USAGE:
    {name} [OPTIONS] [DIRECTORY]

DIRECTORY defaults to the current directory. If it is inside a git repo, that
repo's worktrees are listed; otherwise every repo directly beneath it is scanned.

Interactive by default on a TTY: / opens a fuzzy filter, n creates a worktree,
space marks worktrees and d deletes them, enter opens a shell in the selected
worktree and returns to the picker when that shell exits (ctrl-d), s cycles the
sort order, o opens its PR, q quits.

OPTIONS:
    -q, --query Q   start with the filter box pre-filled; filters --plain and
                    --json output too
    -S, --sort S    order the list: name (default), recent, oldest
    -p, --pick      make enter print the selected path to stdout and exit,
                    instead of opening a shell; works when stdout is redirected
                        cd \"$({name} --pick)\"
        --plain     force tab-separated output:
                        path <TAB> branch <TAB> head <TAB> pr <TAB> flags
    -j, --json      JSON output
    -s, --status    include working-tree status in --plain/--json output
                    (the picker always loads it, lazily)
        --no-pr     skip the GitHub PR lookup
    -h, --help      print this help
    -V, --version   print the version, and the commit it was built from

EXIT CODES:
    0  listed, or the picker was left
    1  no worktrees found, or a fatal error
    2  bad usage
  130  --pick was cancelled, so nothing was printed
";

/// How to name the tool back to the user. Installed as `git-grove`, it is
/// reached as `git grove`, which is what the help should say — whichever of the
/// two was typed, since git execs it under its own name either way.
fn program() -> String {
    program_name(&env::args().next().unwrap_or_default())
}

fn program_name(arg0: &str) -> String {
    // argv[0] is not guaranteed to be anything; the normal way in is the one
    // worth naming when it tells us nothing.
    let Some(name) = Path::new(arg0).file_name() else {
        return "git grove".to_string();
    };
    let name = name.to_string_lossy();
    match name.strip_prefix("git-") {
        Some(sub) => format!("git {sub}"),
        None => name.into_owned(),
    }
}

fn usage() -> String {
    USAGE.replace("{name}", &program())
}

struct Opts {
    dir: PathBuf,
    json: bool,
    plain: bool,
    pick: bool,
    prs: bool,
    status: bool,
    query: String,
    sort: Sort,
}

fn parse_args() -> Result<Option<Opts>, String> {
    let mut opts = Opts {
        dir: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        json: false,
        plain: false,
        pick: false,
        prs: true,
        status: false,
        query: String::new(),
        sort: Sort::default(),
    };
    let mut dir_set = false;
    let mut want_query = false;
    let mut want_sort = false;

    for arg in env::args().skip(1) {
        if want_query {
            opts.query = arg;
            want_query = false;
            continue;
        }
        if want_sort {
            opts.sort = Sort::parse(&arg).ok_or(format!("unknown sort: {arg}"))?;
            want_sort = false;
            continue;
        }
        if let Some(name) = arg
            .strip_prefix("--sort=")
            .or_else(|| arg.strip_prefix("-S="))
        {
            opts.sort = Sort::parse(name).ok_or(format!("unknown sort: {name}"))?;
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
                print!("{}", usage());
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("{} {}", program(), version());
                return Ok(None);
            }
            "-j" | "--json" => opts.json = true,
            "--plain" => opts.plain = true,
            "-p" | "--pick" => opts.pick = true,
            "-q" | "--query" => want_query = true,
            "-S" | "--sort" => want_sort = true,
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
    if want_sort {
        return Err("--sort needs one of: name, recent, oldest".to_string());
    }
    Ok(Some(opts))
}

/// The semver, plus the commit it was built from when there was one to name:
/// `0.1.0 (58cab72d)`, or `0.1.0 (58cab72d-dirty)` from a modified checkout.
fn version() -> String {
    match option_env!("GROVE_COMMIT") {
        Some(commit) => format!("{} ({commit})", env!("CARGO_PKG_VERSION")),
        None => env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(Some(opts)) => opts,
        Ok(None) => return ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{}: {err}\n\n{}", program(), usage());
            return ExitCode::from(2);
        }
    };

    let dir = match std::fs::canonicalize(&opts.dir) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("{}: {}: {err}", program(), opts.dir.display());
            return ExitCode::from(1);
        }
    };

    let repos = git::discover(&dir);
    if repos.is_empty() {
        eprintln!("{}: no git worktrees found in {}", program(), dir.display());
        return ExitCode::from(1);
    }

    let interactive = (opts.pick || (std::io::stdout().is_terminal() && !opts.plain && !opts.json))
        && ui::tty_available();
    let color = interactive
        && env::var_os("NO_COLOR").is_none()
        && env::var("TERM").as_deref() != Ok("dumb");

    let mut app = App::new(repos, dir, opts.prs, color);
    app.sort = opts.sort;
    let (mut loader, rx) = Loader::new(8);
    if opts.prs {
        for (i, repo) in app.repos.iter().enumerate() {
            loader.spawn_prs(i, repo.main_path.clone());
        }
    }

    if interactive {
        let mut state = ui::PickerState::with_query(opts.query.clone());
        if !opts.pick {
            return shell_loop(&mut app, &mut loader, &rx, &mut state);
        }
        state.picking = true;
        return match ui::run(&mut app, &mut loader, &rx, &mut state) {
            Ok(Some(path)) => {
                println!("{}", path.display());
                ExitCode::SUCCESS
            }
            Ok(None) => ExitCode::from(130),
            Err(err) => {
                eprintln!("{}: {err}", program());
                ExitCode::from(1)
            }
        };
    }

    // Non-interactive: everything has to be resolved before we can print.
    let mut outstanding = if opts.prs { app.repos.len() } else { 0 };
    if opts.status {
        for wt in &app.worktrees {
            if loader.request_status(&wt.path) {
                outstanding += 1;
            }
        }
    } else if opts.sort != Sort::Name {
        // Ordering by age needs every row dated; the cheap pass is enough, and
        // avoids a full status scan per worktree.
        for wt in &app.worktrees {
            if loader.request_age(&wt.path) {
                outstanding += 1;
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    while outstanding > 0 {
        let now = Instant::now();
        if now >= deadline {
            eprintln!("{}: timed out waiting for status/PR data", program());
            break;
        }
        match rx.recv_timeout(deadline - now) {
            Ok(msg) => {
                app.apply(msg);
                outstanding -= 1;
            }
            Err(_) => {
                eprintln!("{}: timed out waiting for status/PR data", program());
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

/// What enter does by default: hand the selected worktree to a shell, and come
/// back to the picker when that shell exits, until the user quits the picker
/// itself.
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
                eprintln!("{}: {err}", program());
                return ExitCode::from(1);
            }
        };
        eprintln!("\x1b[90m\u{2192} {}\x1b[0m", path.display());
        if let Err(err) = Command::new(&shell).current_dir(&path).status() {
            eprintln!("{}: {shell} in {}: {err}", program(), path.display());
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
                    if let Some(touched) = s.touched {
                        flags.push(format!("mtime={touched}"));
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
                        "last_modified": s.touched,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_git_subcommand_names_itself_the_way_it_is_reached() {
        assert_eq!(program_name("/usr/local/bin/git-grove"), "git grove");
        assert_eq!(program_name("git-grove"), "git grove");
        // Linked under a shorter name, it answers to that instead.
        assert_eq!(program_name("/usr/local/bin/grove"), "grove");
        // With no argv[0] to go on, the normal way in is the one to name.
        assert_eq!(program_name(""), "git grove");
    }

    #[test]
    fn the_help_text_carries_that_name_rather_than_a_literal() {
        assert!(
            !usage().contains("{name}"),
            "every placeholder is filled in"
        );
        assert!(usage().contains(&program()));
    }
}
