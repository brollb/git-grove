//! Discovering and inspecting git worktrees by shelling out to `git`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A single worktree as reported by `git worktree list --porcelain`.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: String,
    /// Short branch name (`refs/heads/` stripped); `None` when detached or bare.
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub locked: bool,
    pub prunable: bool,
    /// Index into the discovered repo list.
    pub repo: usize,
    /// True for the repo's primary worktree (listed first by git).
    pub main: bool,
}

impl Worktree {
    pub fn branch_label(&self) -> String {
        match &self.branch {
            Some(b) => b.clone(),
            None if self.bare => "(bare)".to_string(),
            None => format!("(detached {})", short_sha(&self.head)),
        }
    }
}

/// Working-tree and upstream divergence for one worktree.
#[derive(Debug, Clone, Copy, Default)]
pub struct Status {
    pub modified: usize,
    pub untracked: usize,
    pub ahead: usize,
    pub behind: usize,
    /// The path is gone, or git refused to report on it.
    pub missing: bool,
    /// Unix seconds of the last activity in this worktree: the newest of the
    /// HEAD commit date, the worktree directory's own mtime, and the mtimes of
    /// the files git reports as changed. A clean worktree therefore reports its
    /// last commit; a dirty one reports the actual last edit.
    pub touched: Option<u64>,
}

impl Status {
    pub fn is_clean(&self) -> bool {
        self.modified == 0 && self.untracked == 0
    }
}

/// A repository plus every worktree attached to it.
#[derive(Debug, Clone)]
pub struct Repo {
    pub label: String,
    pub main_path: PathBuf,
    pub worktrees: Vec<Worktree>,
}

pub fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// Like [`git`], but keeps git's own complaint when the command fails.
fn git_result(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let reason = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("git failed")
        .trim()
        .trim_start_matches("fatal: ")
        .to_string();
    Err(reason)
}

/// Remove a worktree from disk and drop its administrative entry.
///
/// The branch is left alone: removing a worktree should not be able to lose
/// commits. `force` passes `--force` twice, which is what git wants for a
/// worktree that is locked as well as dirty.
pub fn remove_worktree(repo_main: &Path, path: &Path, force: bool) -> Result<(), String> {
    if !path.exists() {
        // The directory is already gone; just clear the stale entry.
        return git_result(repo_main, &["worktree", "prune"]).map(|_| ());
    }
    let path = path.to_string_lossy();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
        args.push("--force");
    }
    args.push(&path);
    git_result(repo_main, &args).map(|_| ())
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Absolute, canonical path of the repo's shared git dir. Identifies a repo
/// uniquely, so linked worktrees of the same repo collapse to one entry.
fn common_dir(dir: &Path) -> Option<PathBuf> {
    let raw = git(dir, &["rev-parse", "--git-common-dir"])?;
    let p = PathBuf::from(raw.trim());
    let p = if p.is_absolute() { p } else { dir.join(p) };
    Some(fs::canonicalize(&p).unwrap_or(p))
}

/// Worktrees visible from `dir`: the repo containing it, or — when `dir` is not
/// itself in a repo — every repo among its immediate children.
pub fn discover(root: &Path) -> Vec<Repo> {
    if common_dir(root).is_some() {
        return repo_at(root, 0).into_iter().collect();
    }

    let mut children: Vec<PathBuf> = match fs::read_dir(root) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(_) => return Vec::new(),
    };
    children.sort();

    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut repos = Vec::new();
    for child in children {
        let Some(cd) = common_dir(&child) else {
            continue;
        };
        if !seen.insert(cd) {
            continue; // already covered by a repo we listed
        }
        if let Some(repo) = repo_at(&child, repos.len()) {
            repos.push(repo);
        }
    }
    repos
}

fn repo_at(dir: &Path, idx: usize) -> Option<Repo> {
    let out = git(dir, &["worktree", "list", "--porcelain"])?;
    let worktrees = parse_porcelain(&out, idx);
    let main_path = worktrees.first()?.path.clone();
    let label = main_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| main_path.display().to_string());
    Some(Repo {
        label,
        main_path,
        worktrees,
    })
}

fn parse_porcelain(out: &str, repo: usize) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    let mut cur: Option<Worktree> = None;

    for line in out.lines() {
        let (key, value) = match line.split_once(' ') {
            Some((k, v)) => (k, v),
            None => (line, ""),
        };
        match key {
            "worktree" => {
                if let Some(wt) = cur.take() {
                    worktrees.push(wt);
                }
                cur = Some(Worktree {
                    path: PathBuf::from(value),
                    head: String::new(),
                    branch: None,
                    bare: false,
                    detached: false,
                    locked: false,
                    prunable: false,
                    repo,
                    main: worktrees.is_empty(),
                });
            }
            _ => {
                let Some(wt) = cur.as_mut() else { continue };
                match key {
                    "HEAD" => wt.head = value.to_string(),
                    "branch" => {
                        wt.branch = Some(
                            value
                                .strip_prefix("refs/heads/")
                                .unwrap_or(value)
                                .to_string(),
                        )
                    }
                    "bare" => wt.bare = true,
                    "detached" => wt.detached = true,
                    "locked" => wt.locked = true,
                    "prunable" => wt.prunable = true,
                    _ => {}
                }
            }
        }
    }
    if let Some(wt) = cur.take() {
        worktrees.push(wt);
    }
    worktrees
}

/// Files a `git status --porcelain` line refers to. `XY path`, or
/// `XY orig -> new` for renames, with paths quoted if they need escaping.
fn porcelain_path(line: &str) -> Option<&str> {
    if line.len() < 4 {
        return None;
    }
    let rest = &line[3..]; // the status letters and their separator are ASCII
    let path = rest.rsplit(" -> ").next().unwrap_or(rest);
    let path = path.trim().trim_matches('"');
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

fn mtime_secs(path: &Path) -> Option<u64> {
    std::fs::symlink_metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Enough dirty files to date a worktree; stat'ing thousands would not make the
/// answer any truer.
const MAX_STATS: usize = 200;

/// A cheap lower bound on [`Status::touched`]: the HEAD commit date and the
/// worktree directory's own mtime, without the `git status` scan.
///
/// A full status costs ~0.1s per worktree in a large repo and contends badly
/// when run across hundreds of them; this costs a `git log -1` and a stat, so
/// the whole set can be dated in well under a second and sorted by age. The
/// full status refines the value upward when it arrives.
pub fn age(path: &Path) -> Option<u64> {
    if !path.exists() {
        return None;
    }
    let commit = git(path, &["log", "-1", "--format=%ct"]).and_then(|o| o.trim().parse().ok());
    mtime_secs(path).max(commit)
}

pub fn status(path: &Path) -> Status {
    let mut s = Status::default();
    if !path.exists() {
        s.missing = true;
        return s;
    }
    // `--no-optional-locks` keeps our own read from refreshing the index, which
    // would otherwise disturb the timestamps we are about to read.
    match git(path, &["--no-optional-locks", "status", "--porcelain"]) {
        Some(out) => {
            let mut stats = 0;
            for line in out.lines() {
                if line.starts_with("??") {
                    s.untracked += 1;
                } else if !line.trim().is_empty() {
                    s.modified += 1;
                } else {
                    continue;
                }
                if stats < MAX_STATS {
                    if let Some(rel) = porcelain_path(line) {
                        stats += 1;
                        s.touched = s.touched.max(mtime_secs(&path.join(rel)));
                    }
                }
            }
        }
        None => {
            s.missing = true;
            return s;
        }
    }
    s.touched = s.touched.max(mtime_secs(path));
    // Fails on a repo with no commits yet, which is fine.
    if let Some(out) = git(path, &["log", "-1", "--format=%ct"]) {
        s.touched = s.touched.max(out.trim().parse::<u64>().ok());
    }
    // Fails (harmlessly) when the branch has no upstream.
    if let Some(out) = git(
        path,
        &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
    ) {
        let mut counts = out.split_whitespace();
        s.behind = counts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        s.ahead = counts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    }
    s
}

/// Remote branch names a local branch might have been pushed as.
///
/// Claude Code worktree branches are prefixed `worktree-` and encode `/` as `+`,
/// but they are pushed with the prefix stripped and the slashes restored — so a
/// PR for `worktree-brollb+fix` lives on `brollb/fix`.
pub fn pr_candidates(branch: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |c: String| {
        if !out.contains(&c) {
            out.push(c);
        }
    };
    push(branch.to_string());
    if branch.contains('+') {
        push(branch.replace('+', "/"));
    }
    if let Some(rest) = branch.strip_prefix("worktree-") {
        push(rest.to_string());
        if rest.contains('+') {
            push(rest.replace('+', "/"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_entries_and_flags() {
        let out = "\
worktree /repo
HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
branch refs/heads/main

worktree /repo/.claude/worktrees/foo
HEAD bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
branch refs/heads/worktree-brollb+foo
locked

worktree /tmp/gone
HEAD cccccccccccccccccccccccccccccccccccccccc
detached
prunable gitdir file points to non-existent location
";
        let wts = parse_porcelain(out, 3);
        assert_eq!(wts.len(), 3);

        assert!(wts[0].main);
        assert_eq!(wts[0].branch.as_deref(), Some("main"));
        assert_eq!(wts[0].repo, 3);

        assert!(!wts[1].main);
        assert_eq!(wts[1].branch.as_deref(), Some("worktree-brollb+foo"));
        assert!(wts[1].locked);
        assert!(!wts[1].prunable);

        assert!(wts[2].detached);
        assert!(wts[2].prunable);
        assert_eq!(wts[2].branch, None);
        assert_eq!(wts[2].branch_label(), "(detached cccccccc)");
    }

    #[test]
    fn bare_worktree_is_labelled() {
        let out = "worktree /repo.git\nHEAD 0000000000000000000000000000000000000000\nbare\n";
        let wts = parse_porcelain(out, 0);
        assert!(wts[0].bare);
        assert_eq!(wts[0].branch_label(), "(bare)");
    }

    fn run(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A throwaway repo with one linked worktree, under the test's temp dir.
    fn scratch_repo(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("grove-test-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        run(&root, &["init", "-q", "."]);
        run(&root, &["config", "user.email", "t@example.com"]);
        run(&root, &["config", "user.name", "Test"]);
        run(&root, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let wt = root.join("wt");
        run(&root, &["worktree", "add", "-q", "wt", "-b", "feature"]);
        (root, wt)
    }

    #[test]
    fn removes_a_clean_worktree() {
        let (root, wt) = scratch_repo("clean");
        assert!(wt.exists());
        assert_eq!(remove_worktree(&root, &wt, false), Ok(()));
        assert!(!wt.exists(), "the directory should be gone");
        assert_eq!(list_worktrees_for_test(&root).len(), 1, "only main is left");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn refuses_a_dirty_worktree_until_forced() {
        let (root, wt) = scratch_repo("dirty");
        std::fs::write(wt.join("scratch.txt"), "work in progress").expect("write");

        let refused = remove_worktree(&root, &wt, false);
        assert!(
            refused.is_err(),
            "untracked work must not be deleted silently"
        );
        assert!(wt.exists(), "the worktree must survive a refused removal");

        assert_eq!(remove_worktree(&root, &wt, true), Ok(()));
        assert!(!wt.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_worktree_whose_directory_vanished_is_pruned() {
        let (root, wt) = scratch_repo("stale");
        std::fs::remove_dir_all(&wt).expect("rm");
        assert_eq!(
            list_worktrees_for_test(&root).len(),
            2,
            "entry is still registered"
        );

        assert_eq!(remove_worktree(&root, &wt, false), Ok(()));
        assert_eq!(
            list_worktrees_for_test(&root).len(),
            1,
            "stale entry pruned"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    fn list_worktrees_for_test(root: &Path) -> Vec<Worktree> {
        parse_porcelain(&git(root, &["worktree", "list", "--porcelain"]).unwrap(), 0)
    }

    #[test]
    fn reads_the_path_out_of_a_porcelain_line() {
        assert_eq!(porcelain_path(" M src/main.rs"), Some("src/main.rs"));
        assert_eq!(porcelain_path("?? notes.md"), Some("notes.md"));
        assert_eq!(porcelain_path("R  old.rs -> new.rs"), Some("new.rs"));
        assert_eq!(porcelain_path("A  \"odd name.rs\""), Some("odd name.rs"));
        assert_eq!(porcelain_path(""), None);
        assert_eq!(porcelain_path(" M "), None);
    }

    #[test]
    fn pr_candidates_cover_the_worktree_branch_convention() {
        // Pushed with the `worktree-` prefix stripped and `+` restored to `/`.
        assert_eq!(
            pr_candidates("worktree-brollb+fix-smoke-tests"),
            vec![
                "worktree-brollb+fix-smoke-tests",
                "worktree-brollb/fix-smoke-tests",
                "brollb+fix-smoke-tests",
                "brollb/fix-smoke-tests",
            ]
        );
        assert_eq!(pr_candidates("brollb/plain"), vec!["brollb/plain"]);
        assert_eq!(
            pr_candidates("worktree-solo"),
            vec!["worktree-solo", "solo"]
        );
    }
}
