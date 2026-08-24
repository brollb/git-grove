//! Shared state: the flattened worktree list plus whatever the background
//! loaders have filled in so far.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::fuzzy::{self, Fields, Hits};
use crate::gh::Pr;
use crate::git::{self, Repo, Status, Worktree};
use crate::load::Msg;

/// How the list is ordered.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Sort {
    /// Repo order, main worktree first, then branch name — or by match score
    /// while a query is active.
    #[default]
    Name,
    /// Most recently touched first.
    Recent,
    /// Least recently touched first: what you would prune.
    Oldest,
}

impl Sort {
    pub fn parse(name: &str) -> Option<Sort> {
        match name {
            "name" => Some(Sort::Name),
            "recent" | "newest" => Some(Sort::Recent),
            "oldest" | "stale" => Some(Sort::Oldest),
            _ => None,
        }
    }

    pub fn next(self) -> Sort {
        match self {
            Sort::Name => Sort::Recent,
            Sort::Recent => Sort::Oldest,
            Sort::Oldest => Sort::Name,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::Name => "by name",
            Sort::Recent => "newest first",
            Sort::Oldest => "oldest first",
        }
    }
}

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
    /// Cheap age estimates, for rows whose full status has not loaded yet.
    pub ages: HashMap<PathBuf, Option<u64>>,
    pub sort: Sort,
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
            ages: HashMap::new(),
            sort: Sort::default(),
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
            Msg::Age(path, age) => {
                self.ages.insert(path, age);
            }
            Msg::Prs(repo, result) => {
                self.prs.insert(repo, result);
            }
            // Removals are batched by the picker, which drops the rows itself
            // once the whole batch has reported in.
            Msg::Removed(..) => {}
        }
    }

    /// Where to run `git worktree` commands affecting this worktree.
    pub fn repo_main(&self, wt: &Worktree) -> &Path {
        &self.repos[wt.repo].main_path
    }

    /// Drop worktrees that no longer exist, after removing them.
    pub fn forget_worktrees(&mut self, removed: &std::collections::HashSet<PathBuf>) {
        self.worktrees.retain(|wt| !removed.contains(&wt.path));
    }

    /// When this worktree was last touched: the full status if it has loaded,
    /// otherwise the cheap estimate.
    pub fn touched(&self, wt: &Worktree) -> Option<u64> {
        let from_status = self.statuses.get(&wt.path).and_then(|s| s.touched);
        from_status.max(self.ages.get(&wt.path).copied().flatten())
    }

    /// Worktrees whose age has not come back yet. A row that came back without
    /// a date — a worktree whose directory is gone — is answered, not pending,
    /// so the count reaches zero instead of sticking.
    pub fn undated(&self) -> usize {
        self.worktrees
            .iter()
            .filter(|wt| !self.ages.contains_key(&wt.path) && !self.statuses.contains_key(&wt.path))
            .count()
    }

    /// Worktrees matching `query`, best match first, then ordered by the
    /// current sort. A blank query and the default sort keep the natural
    /// order, so the unfiltered list still reads repo by repo.
    pub fn filter(&self, query: &str) -> Vec<(usize, Hits)> {
        if query.trim().is_empty() {
            let mut all: Vec<(usize, Hits)> = (0..self.worktrees.len())
                .map(|i| (i, Hits::default()))
                .collect();
            self.apply_sort(&mut all);
            return all;
        }
        // The repo name is only worth matching when there is more than one:
        // otherwise every row matches it and the query filters nothing.
        let match_repo = self.repos.len() > 1;
        let mut matched: Vec<(usize, Hits)> = self
            .worktrees
            .iter()
            .enumerate()
            .filter_map(|(i, wt)| {
                let branch = wt.branch_label();
                let path = display_path(&wt.path, Some(&self.root));
                let pr = match self.pr_for(wt) {
                    PrCell::Open(pr) => Some(format!("#{}", pr.number)),
                    _ => None,
                };
                let fields = Fields {
                    branch: &branch,
                    path: &path,
                    repo: if match_repo {
                        &self.repos[wt.repo].label
                    } else {
                        ""
                    },
                    pr,
                };
                fuzzy::match_fields(&fields, query).map(|hits| (i, hits))
            })
            .collect();
        // Stable, so equally-scored rows keep their natural order.
        matched.sort_by(|a, b| b.1.score.cmp(&a.1.score));
        // An explicit sort wins over match score; the score still breaks ties,
        // since this sort is stable too.
        self.apply_sort(&mut matched);
        matched
    }

    /// Order by age, newest or oldest first, with not-yet-dated rows last so
    /// the list does not jump around while they load.
    fn apply_sort(&self, rows: &mut [(usize, Hits)]) {
        match self.sort {
            Sort::Name => {}
            Sort::Recent => rows.sort_by_key(|(i, _)| {
                let touched = self.touched(&self.worktrees[*i]);
                (touched.is_none(), std::cmp::Reverse(touched.unwrap_or(0)))
            }),
            Sort::Oldest => rows.sort_by_key(|(i, _)| {
                let touched = self.touched(&self.worktrees[*i]);
                (touched.is_none(), touched.unwrap_or(0))
            }),
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
    use crate::git::Repo;

    fn worktree(path: &str, branch: &str) -> Worktree {
        Worktree {
            path: PathBuf::from(path),
            head: "0123456789abcdef".to_string(),
            branch: Some(branch.to_string()),
            bare: false,
            detached: false,
            locked: false,
            prunable: false,
            repo: 0,
            main: false,
        }
    }

    /// Three worktrees, dated oldest → newest, plus one that never dates.
    fn app_with_ages() -> App {
        let worktrees = vec![
            worktree("/r/old", "old-branch"),
            worktree("/r/new", "new-branch"),
            worktree("/r/mid", "mid-branch"),
            worktree("/r/gone", "gone-branch"),
        ];
        let repo = Repo {
            label: "r".to_string(),
            main_path: PathBuf::from("/r"),
            worktrees,
        };
        let mut app = App::new(vec![repo], PathBuf::from("/r"), false, false);
        app.ages.insert(PathBuf::from("/r/old"), Some(1_000));
        app.ages.insert(PathBuf::from("/r/mid"), Some(2_000));
        app.ages.insert(PathBuf::from("/r/new"), Some(3_000));
        app.ages.insert(PathBuf::from("/r/gone"), None);
        app
    }

    fn order(app: &App, query: &str) -> Vec<String> {
        app.filter(query)
            .into_iter()
            .map(|(i, _)| app.worktrees[i].branch.clone().unwrap())
            .collect()
    }

    #[test]
    fn recency_sorts_newest_first_and_undated_last() {
        let mut app = app_with_ages();
        app.sort = Sort::Recent;
        assert_eq!(
            order(&app, ""),
            ["new-branch", "mid-branch", "old-branch", "gone-branch"]
        );
    }

    #[test]
    fn oldest_sorts_the_other_way_but_still_parks_undated_last() {
        let mut app = app_with_ages();
        app.sort = Sort::Oldest;
        assert_eq!(
            order(&app, ""),
            ["old-branch", "mid-branch", "new-branch", "gone-branch"]
        );
    }

    #[test]
    fn sort_by_name_keeps_the_natural_order() {
        let app = app_with_ages();
        // App::new sorts by branch label within a repo.
        assert_eq!(
            order(&app, ""),
            ["gone-branch", "mid-branch", "new-branch", "old-branch"]
        );
    }

    #[test]
    fn an_age_sort_outranks_match_score() {
        let mut app = app_with_ages();
        // All four match "branch"; under a recency sort, age decides the order.
        app.sort = Sort::Recent;
        assert_eq!(
            order(&app, "branch"),
            ["new-branch", "mid-branch", "old-branch", "gone-branch"]
        );
        // ... and under the default sort, score decides it.
        app.sort = Sort::Name;
        assert_eq!(order(&app, "old"), ["old-branch"]);
    }

    #[test]
    fn a_worktree_that_came_back_undatable_is_not_counted_as_pending() {
        let mut app = app_with_ages();
        assert_eq!(
            app.undated(),
            0,
            "every row has an answer, one of them None"
        );
        app.ages.remove(&PathBuf::from("/r/mid"));
        assert_eq!(app.undated(), 1);
    }

    #[test]
    fn full_status_beats_the_cheap_estimate() {
        let mut app = app_with_ages();
        let path = PathBuf::from("/r/old");
        app.statuses.insert(
            path.clone(),
            Status {
                touched: Some(9_000),
                ..Default::default()
            },
        );
        let wt = app.worktrees.iter().find(|w| w.path == path).unwrap();
        assert_eq!(app.touched(wt), Some(9_000));
    }

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
