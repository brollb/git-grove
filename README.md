# git-worktrees

List the git worktrees in a directory, annotated with the number of the open
GitHub PR for each branch.

On a TTY it opens an interactive picker with fuzzy search; piped, it prints
tab-separated lines, so the same command works in a terminal and in a script.

```
 23 worktrees in ~/baseten/trainers  ·  8 with an open PR
 › loops worker▌                                                          3/23
  PR       BRANCH                                    STATUS          AGE  PATH
▸ #837     brollb/loops-worker                       ↑3                2h  …/brollb+loops-worker
  #1003    brollb/loops-worker-drop-storage-dir      clean             3d  …/loops-harness-fixes
  -        brollb/pickable-sampling-client           ↓12 ●1            5w  …/brollb/pickable-sampling-client

  modified 2 hours ago  ·  #837 (draft) feat(loops): real Megatron worker behind the broker
  type to filter  ·  ↑/↓ move  ·  enter: select  ·  ctrl-o: open PR  ·  esc: clear/quit
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

### Searching

Just type: the query fuzzy-matches the branch, path, PR number and (when more
than one repo is listed) the repo name, best match first, with the matched
characters highlighted. Space-separated tokens all have to match, so
`trainers loops` narrows to loops branches in the trainers repo, and `837`
finds a worktree by its PR number.

| Key | |
| --- | --- |
| *any character* | extend the filter |
| `↑` `↓`, `ctrl-p` `ctrl-n`, `PgUp` `PgDn`, `Home` `End` | move |
| `enter` | select |
| `ctrl-o` | open the PR in a browser |
| `backspace`, `ctrl-w`, `ctrl-u` | delete a character, a word, the query |
| `esc` | clear the query, or quit when it is already empty |
| `ctrl-c`, `ctrl-d` | quit |

### Jumping to a worktree

`--cd` opens a shell in the worktree you pick and returns you to the picker when
that shell exits, so you can hop between worktrees without retyping paths:

```sh
alias cdw='git-worktrees --cd'
```

`ctrl-d` (or `exit`) leaves the worktree shell and puts the picker back up with
your query and cursor where you left them; `esc` from there ends the session and
returns you to the shell you started in, in the directory you started in. The
worktree shell is a child process — it is `$SHELL` started with its working
directory set — so nothing is changed in the calling shell.

To change the calling shell's own directory instead, use `--pick`, which draws
the list on `/dev/tty` and prints only the selection to stdout:

```sh
wt() { cd "$(git-worktrees --pick "$@")" || return; }
```

Cancelling exits 130 with nothing on stdout, so `cd` is left alone.

### Options

| Option | Effect |
| --- | --- |
| `-c`, `--cd` | open a shell in the selected worktree, then return to the picker |
| `-q`, `--query Q` | start with the filter pre-filled; also filters `--plain`/`--json` |
| `-p`, `--pick` | force the picker, printing the selection to stdout |
| `--plain` | force tab-separated output: `path`, `branch`, `head`, `pr`, `flags` |
| `-j`, `--json` | JSON output |
| `-s`, `--status` | include working-tree status and last-modified time in `--plain`/`--json` output |
| `--no-pr` | skip the GitHub lookup |

Exit codes: `0` listed or selected, `1` nothing found or a fatal error, `2` bad
usage, `130` picker cancelled.

## Columns

- **PR** — open PR for the branch, `-` if there is none, `…` while `gh` is still
  running. Draft PRs are magenta; the title and draft state are shown for the
  selected row.
- **STATUS** — `↑`/`↓` commits ahead of and behind upstream, `●` modified files,
  `?` untracked files, plus `locked`, `prunable` and `missing`.
- **AGE** — how long ago the worktree was last touched: the newest of its HEAD
  commit date, its own directory mtime, and the mtimes of the files git reports
  as changed. So a clean worktree is dated by its last commit, and a dirty one
  by the actual last edit. The selected row spells the age out in full under the
  list.
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
  248-worktree directory draws immediately and fills in as you scroll. The age
  comes from the same pass, so both columns fill in together.
- `--plain` reports the age as `mtime=<unix seconds>` among the flags, and
  `--json` as `status.last_modified`; `status.modified` next to it is the count
  of modified files, not a time.
- Status reads pass `--no-optional-locks`, so listing worktrees never refreshes
  an index and never disturbs the timestamps it is reporting.
