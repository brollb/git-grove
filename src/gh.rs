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
}

/// Open PRs for the repo at `dir`, keyed by head branch name.
///
/// One `gh` call per repo — cheap enough to run for a repo with hundreds of
/// worktrees, unlike a per-branch lookup.
pub fn fetch(dir: &Path) -> Result<HashMap<String, Pr>, String> {
    let out = Command::new("gh")
        .arg("pr")
        .arg("list")
        .args(["--state", "open", "--limit", "1000"])
        .args(["--json", "number,headRefName,isDraft,title,url"])
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
            .unwrap_or("gh pr list failed")
            .trim()
            .to_string();
        return Err(msg);
    }

    let parsed: Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let mut prs = HashMap::new();
    for item in parsed.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let (Some(number), Some(head)) = (
            item.get("number").and_then(Value::as_u64),
            item.get("headRefName").and_then(Value::as_str),
        ) else {
            continue;
        };
        prs.insert(
            head.to_string(),
            Pr {
                number,
                draft: item
                    .get("isDraft")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                title: item
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                url: item
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            },
        );
    }
    Ok(prs)
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
