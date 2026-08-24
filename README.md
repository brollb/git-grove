# git-worktrees

List the git worktrees in a directory, annotated with the number of the open
GitHub PR for each branch.

On a TTY it opens an interactive picker; piped, it prints tab-separated lines,
so the same command works in a terminal and in a script.

```
 23 worktrees in ~/baseten/trainers  ·  8 with an open PR

  PR       BRANCH                                    STATUS          PATH
  #822     xiaohan/multilora-adapter-state           ●2 ?7           .
▸ #837     brollb/loops-worker                       ↑3              …/brollb+loops-worker
  #1003    brollb/loops-worker-drop-storage-dir      clean           …/loops-harness-fixes
  -        brollb/pickable-sampling-client           ↓12 ●1          …/brollb/pickable-sampling-client
  #366     brollb/test-max-seq-len-error             prunable        /private/tmp/trainers-test-max-seq-len

  #837 (draft) feat(loops): real Megatron worker behind the broker
  ↑/↓ move  ·  enter: print path  ·  o: open PR  ·  q: quit
```

## Install

```sh
cargo install --path .
```

The binary is named `git-worktrees`, so once it is on your `PATH` git picks it
up as a subcommand: `git worktrees`.

## Usage

```sh
git worktrees                  # worktrees of the repo you are standing in
git worktrees ~/baseten        # a directory of repos: every repo beneath it
git worktrees --json | jq .    # machine-readable
```

`DIRECTORY` defaults to the current directory. If it is inside a git repo, that
repo's worktrees are listed. Otherwise every repo directly beneath it is
scanned, grouped by repo — handy for a `~/src`-style directory.

### Jumping to a worktree

`enter` prints the selected worktree's absolute path to stdout, which is what
makes this work:

```sh
wt() { cd "$(git-worktrees --pick "$@")" || return; }
```

`--pick` forces the picker even when stdout is redirected: the list is drawn on
`/dev/tty` and only the selection goes to stdout. Cancelling exits 130 with
nothing on stdout, so `cd` is left alone.

### Options

| Option | Effect |
| --- | --- |
| `-p`, `--pick` | force the picker, printing the selection to stdout |
| `--plain` | force tab-separated output: `path`, `branch`, `head`, `pr`, `flags` |
| `-j`, `--json` | JSON output |
| `-s`, `--status` | include working-tree status in `--plain`/`--json` output |
| `--no-pr` | skip the GitHub lookup |

Exit codes: `0` listed or selected, `1` nothing found or a fatal error, `2` bad
usage, `130` picker cancelled.

## Columns

- **PR** — open PR for the branch, `-` if there is none, `…` while `gh` is still
  running. Draft PRs are magenta; the title and draft state are shown for the
  selected row.
- **STATUS** — `↑`/`↓` commits ahead of and behind upstream, `●` modified files,
  `?` untracked files, plus `locked`, `prunable` and `missing`.
- **PATH** — relative to the directory you asked about. `.claude/worktrees/` is
  collapsed to `…/` since it is the same on nearly every row.

## Notes

- PRs come from one `gh pr list` per repo, not one lookup per branch, so a repo
  with a couple of hundred worktrees still costs a single API call. Without
  `gh`, or on a non-GitHub remote, the PR column degrades to `-` and the reason
  is shown under the list.
- Branch names are matched against PR head branches allowing for the Claude Code
  worktree convention: `worktree-brollb+fix` is pushed as `brollb/fix`.
- In the picker, `git status` is only run for the rows on screen, so a
  248-worktree directory draws immediately and fills in as you scroll.
