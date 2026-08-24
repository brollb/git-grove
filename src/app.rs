//! Shared state: the flattened worktree list plus whatever the background
//! loaders have filled in so far.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::gh::Pr;
use crate::git::{self, Repo, Status, Worktree};
use crate::load::Msg;

#[derive(Clone, Copy)]
pub enum PrCell<'a> {
    /// Still waiting on `gh`.
    Loading,
    /// No open PR for this branch.
    None,
    Open(&'a Pr),
    /// PR data could not be fetched (no `gh`, not a GitHub remote, ...).
    Unavailable(&'a str),
}

pub struct App {
    pub repos: Vec<Repo>,
    pub worktrees: Vec<Worktree>,
    pub statuses: HashMap<PathBuf, Status>,
    pub prs: HashMap<usize, Result<HashMap<String, Pr>, String>>,
    pub root: PathBuf,
    pub want_prs: bool,
    pub color: bool,
}

impl App {
    pub fn new(repos: Vec<Repo>, root: PathBuf, want_prs: bool, color: bool) -> App {
        let mut worktrees = Vec::new();
        for repo in &repos {
            let mut wts = repo.worktrees.clone();
            wts.sort_by_key(|w| (!w.main, w.branch_label().to_lowercase()));
            worktrees.extend(wts);
        }
        App {
            repos,
            worktrees,
            statuses: HashMap::new(),
            prs: HashMap::new(),
            root,
            want_prs,
            color,
        }
    }

    pub fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Status(path, status) => {
                self.statuses.insert(path, status);
            }
            Msg::Prs(repo, result) => {
                self.prs.insert(repo, result);
            }
        }
    }

    pub fn pr_for(&self, wt: &Worktree) -> PrCell<'_> {
        if !self.want_prs {
            return PrCell::None;
        }
        match self.prs.get(&wt.repo) {
            None => PrCell::Loading,
            Some(Err(why)) => PrCell::Unavailable(why),
            Some(Ok(by_branch)) => {
                let Some(branch) = &wt.branch else {
                    return PrCell::None;
                };
                for candidate in git::pr_candidates(branch) {
                    if let Some(pr) = by_branch.get(&candidate) {
                        return PrCell::Open(pr);
                    }
                }
                PrCell::None
            }
        }
    }
}

/// Path relative to `root` when it sits underneath it, else `~`-shortened.
///
/// The `.claude/worktrees/` segment is collapsed: it is identical on nearly
/// every row and would otherwise crowd out the part that differs.
pub fn display_path(path: &Path, root: Option<&Path>) -> String {
    let shortened = |s: String| s.replace(".claude/worktrees/", "\u{2026}/");

    if let Some(root) = root {
        if let Ok(rel) = path.strip_prefix(root) {
            let s = rel.display().to_string();
            return if s.is_empty() {
                ".".to_string()
            } else {
                shortened(s)
            };
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(rel) = path.strip_prefix(PathBuf::from(home)) {
            return shortened(format!("~/{}", rel.display()));
        }
    }
    shortened(path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_shown_relative_to_the_root() {
        let root = Path::new("/repo");
        assert_eq!(display_path(Path::new("/repo"), Some(root)), ".");
        assert_eq!(
            display_path(Path::new("/repo/.claude/worktrees/foo"), Some(root)),
            "\u{2026}/foo"
        );
        // Outside the root: left absolute rather than dressed up with `../`.
        assert_eq!(
            display_path(Path::new("/tmp/elsewhere"), Some(root)),
            "/tmp/elsewhere"
        );
    }
}
