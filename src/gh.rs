//! Open pull requests for a repo, via the `gh` CLI.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Pr {
    pub number: u64,
    pub draft: bool,
    pub title: String,
    pub url: String,
    /// CI and review state, filled in by a second, narrower query once the PR
    /// list is in — see [`fetch_details`].
    pub ci: Option<Ci>,
    /// Already merged: found for a branch that has no open PR.
    pub merged: bool,
    /// The commit the PR last pointed at. Only looked up for merged PRs, where
    /// it tells whether the worktree has moved on since.
    pub head_oid: String,
}

/// What the second query found: CI for open PRs, and merged PRs for the
/// branches that have none open.
#[derive(Debug, Default)]
pub struct Details {
    pub ci: HashMap<u64, Ci>,
    /// Keyed by the head branch name asked about.
    pub merged: HashMap<String, Pr>,
}

/// What a PR's checks and reviewers make of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ci {
    pub checks: Checks,
    pub review: Review,
}

/// CI on the PR's head commit, tallied across check runs and commit statuses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Checks {
    pub passed: usize,
    pub failed: usize,
    pub pending: usize,
}

/// The overall CI verdict, worst first: one failure fails the lot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Failing,
    Pending,
    Passing,
    /// No checks ran at all.
    None,
}

impl Checks {
    pub fn verdict(&self) -> Verdict {
        if self.failed > 0 {
            Verdict::Failing
        } else if self.pending > 0 {
            Verdict::Pending
        } else if self.passed > 0 {
            Verdict::Passing
        } else {
            Verdict::None
        }
    }

    pub fn total(&self) -> usize {
        self.passed + self.failed + self.pending
    }

    /// Tally the per-state counts GitHub keeps for a commit's checks. Check
    /// runs and commit statuses are counted separately, but their state names
    /// mean the same thing where they overlap, so one table covers both.
    fn from_counts(contexts: &Value) -> Checks {
        let mut checks = Checks::default();
        for list in ["checkRunCountsByState", "statusContextCountsByState"] {
            for entry in contexts
                .get(list)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let count = entry.get("count").and_then(Value::as_u64).unwrap_or(0) as usize;
                match entry.get("state").and_then(Value::as_str).unwrap_or("") {
                    // Skipped and neutral runs did not fail, so they pass.
                    "SUCCESS" | "NEUTRAL" | "SKIPPED" | "COMPLETED" => checks.passed += count,
                    "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED"
                    | "STARTUP_FAILURE" => checks.failed += count,
                    "PENDING" | "EXPECTED" | "QUEUED" | "IN_PROGRESS" | "WAITING" | "REQUESTED" => {
                        checks.pending += count
                    }
                    // STALE: superseded by a newer run, which is counted instead.
                    _ => {}
                }
            }
        }
        checks
    }
}

/// Where the PR stands with reviewers, per GitHub's branch protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Review {
    Approved,
    ChangesRequested,
    Required,
    /// The repo does not require reviews, so there is no decision to report.
    None,
}

impl Review {
    fn parse(decision: &str) -> Review {
        match decision {
            "APPROVED" => Review::Approved,
            "CHANGES_REQUESTED" => Review::ChangesRequested,
            "REVIEW_REQUIRED" => Review::Required,
            _ => Review::None,
        }
    }

    pub fn label(self) -> Option<&'static str> {
        match self {
            Review::Approved => Some("approved"),
            Review::ChangesRequested => Some("changes requested"),
            Review::Required => Some("review required"),
            Review::None => None,
        }
    }
}

/// Open PRs for the repo at `dir`, keyed by head branch name.
///
/// One `gh` call per repo — cheap enough to run for a repo with hundreds of
/// worktrees, unlike a per-branch lookup.
///
/// CI is deliberately not asked for here: `statusCheckRollup` lists every
/// check on every PR, and across the open PRs of a busy repo that is enough
/// for GitHub to time out — taking the PR numbers down with it.
pub fn fetch(dir: &Path) -> Result<HashMap<String, Pr>, String> {
    let parsed = gh(
        dir,
        &[
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            "1000",
            "--json",
            "number,headRefName,isDraft,title,url",
        ],
    )?;
    let mut prs = HashMap::new();
    for item in parsed.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let (Some(number), Some(head)) = (
            item.get("number").and_then(Value::as_u64),
            item.get("headRefName").and_then(Value::as_str),
        ) else {
            continue;
        };
        prs.insert(head.to_string(), pr_from(item, number));
    }
    Ok(prs)
}

/// The fields a PR list and a PR lookup have in common.
fn pr_from(item: &Value, number: u64) -> Pr {
    let text = |k: &str| {
        item.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    Pr {
        number,
        draft: item
            .get("isDraft")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        title: text("title"),
        url: text("url"),
        ci: None,
        merged: false,
        head_oid: text("headRefOid"),
    }
}

/// How many lookups to put in one GraphQL request.
const BATCH: usize = 50;

/// One aliased lookup in the details query.
enum Lookup<'a> {
    /// CI and review state for an open PR.
    Ci(u64),
    /// The latest merged PR from a branch.
    Merged(&'a str),
}

/// CI and review state for these open PRs, and the merged PR (if any) for
/// each of these branches.
///
/// Only PRs and branches that have a worktree are worth asking about, and for
/// CI GitHub's per-state check counts are enough — a few hundred bytes a PR,
/// where the full list of checks would be tens of kilobytes. Merged PRs are
/// looked up by branch rather than listed, since a repo's merged PRs run to
/// the tens of thousands.
pub fn fetch_details(dir: &Path, open: &[&Pr], branches: &[String]) -> Result<Details, String> {
    let lookups: Vec<Lookup> = open
        .iter()
        .map(|pr| Lookup::Ci(pr.number))
        .chain(branches.iter().map(|b| Lookup::Merged(b)))
        .collect();
    let mut found = Details::default();
    for batch in lookups.chunks(BATCH) {
        let fields: String = batch
            .iter()
            .enumerate()
            .map(|(i, lookup)| match lookup {
                Lookup::Ci(n) => format!(
                    "q{i}: pullRequest(number: {n}) {{ number reviewDecision \
                     commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ \
                     contexts(first: 0) {{ checkRunCountsByState {{ state count }} \
                     statusContextCountsByState {{ state count }} }} }} }} }} }} }}"
                ),
                // A JSON string is a valid GraphQL string, escapes and all.
                Lookup::Merged(branch) => format!(
                    "q{i}: pullRequests(headRefName: {}, states: MERGED, last: 1) {{ \
                     nodes {{ number title url isDraft headRefOid }} }}",
                    Value::from(*branch)
                ),
            })
            .collect::<Vec<_>>()
            .join(" ");
        let query = format!(
            "query($owner: String!, $name: String!) {{ \
             repository(owner: $owner, name: $name) {{ {fields} }} }}"
        );
        // gh fills in `{owner}` and `{repo}` from the repo it is run in.
        let parsed = gh(
            dir,
            &[
                "api",
                "graphql",
                "-F",
                "owner={owner}",
                "-F",
                "name={repo}",
                "-f",
                &format!("query={query}"),
            ],
        )?;
        let Some(repo) = parsed.pointer("/data/repository") else {
            continue;
        };
        for (i, lookup) in batch.iter().enumerate() {
            let Some(answer) = repo.get(format!("q{i}")) else {
                continue;
            };
            match lookup {
                Lookup::Ci(number) => {
                    let contexts = answer
                        .pointer("/commits/nodes/0/commit/statusCheckRollup/contexts")
                        .unwrap_or(&Value::Null);
                    let review = answer.get("reviewDecision").and_then(Value::as_str);
                    found.ci.insert(
                        *number,
                        Ci {
                            checks: Checks::from_counts(contexts),
                            review: Review::parse(review.unwrap_or("")),
                        },
                    );
                }
                Lookup::Merged(branch) => {
                    let Some(item) = answer.pointer("/nodes/0") else {
                        continue;
                    };
                    let Some(number) = item.get("number").and_then(Value::as_u64) else {
                        continue;
                    };
                    let mut pr = pr_from(item, number);
                    pr.merged = true;
                    found.merged.insert(branch.to_string(), pr);
                }
            }
        }
    }
    Ok(found)
}

/// Run `gh` in `dir` and parse what it prints as JSON, keeping gh's own
/// complaint when it fails.
fn gh(dir: &Path, args: &[&str]) -> Result<Value, String> {
    let out = Command::new("gh")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "gh not installed".to_string()
            } else {
                e.to_string()
            }
        })?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = stderr
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .unwrap_or_else(|| format!("gh {} failed", args[..2].join(" ")));
        return Err(msg);
    }
    serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
}

/// Open a URL in the user's browser. Best effort; failures are ignored.
pub fn open_url(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tallies_check_runs_and_commit_statuses_together() {
        let contexts = serde_json::json!({
            "checkRunCountsByState": [
                {"state": "SUCCESS", "count": 4},
                {"state": "SKIPPED", "count": 9},
                {"state": "IN_PROGRESS", "count": 1},
                {"state": "TIMED_OUT", "count": 1},
                {"state": "STALE", "count": 3},
                {"state": "QUEUED", "count": 0},
            ],
            "statusContextCountsByState": [
                {"state": "SUCCESS", "count": 1},
                {"state": "PENDING", "count": 1},
                {"state": "ERROR", "count": 1},
            ],
        });
        let checks = Checks::from_counts(&contexts);
        assert_eq!(
            checks,
            Checks {
                passed: 14,
                failed: 2,
                pending: 2
            }
        );
        assert_eq!(checks.verdict(), Verdict::Failing);
        // A commit nothing ran on has no rollup at all.
        assert_eq!(Checks::from_counts(&Value::Null), Checks::default());
    }

    #[test]
    fn a_running_check_holds_the_verdict_until_it_finishes() {
        let pending = Checks {
            passed: 4,
            failed: 0,
            pending: 1,
        };
        assert_eq!(pending.verdict(), Verdict::Pending);
        assert_eq!(Checks::default().verdict(), Verdict::None);
    }
}
