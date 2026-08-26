# grove

> *Take a stroll through your git worktrees.*

Browse, search and prune them — a grove being the stand of trees around a repo.
Each is annotated with the number of the open GitHub PR for its branch.

On a TTY it opens an interactive picker — fuzzy search, multi-select, creating
worktrees and pruning the ones you are done with; piped, it prints
tab-separated lines, so the same command works in a terminal and in a script.

```
 23 worktrees in ~/src/toolkit  ·  8 with an open PR
 /cache worker                                              2 marked  ·  3/23
   PR       BRANCH                                   STATUS          AGE  PATH
▸  #42      brogan/cache-worker                      ↑3                2h  …/brogan+cache-worker
 ● #57      brogan/cache-worker-drop-storage-dir     clean             3d  …/cache-harness-fixes
 ● -        brogan/pickable-sampling-client          ↓12 ●1            5w  …/brogan/pickable-sampling-client

  modified 2 hours ago  ·  #42 (draft) feat(cache): real worker behind the broker
  /: search  ·  n: new  ·  space: mark  ·  d: delete  ·  enter: open  ·  s: sort  ·  o: PR  ·  q: quit
```

## Install

```sh
cargo install --path .
```

That puts `grove` on your `PATH`. For it to double as a git subcommand, link it
under the name git looks for:

```sh
ln -s "$(command -v grove)" ~/.cargo/bin/git-grove   # then: git grove
```

## Usage

```sh
grove                  # worktrees of the repo you are standing in
grove ~/src            # a directory of repos: every repo beneath it
grove --json | jq .    # machine-readable
```

`DIRECTORY` defaults to the current directory. If it is inside a git repo, that
repo's worktrees are listed. Otherwise every repo directly beneath it is
scanned, grouped by repo — handy for a `~/src`-style directory.

### Keys

The picker is modal: keys are commands until `/` opens the filter, which leaves
the letters free for acting on the list.

| Key | |
| --- | --- |
| `/` | open the fuzzy filter |
| `n` | create a worktree for a branch you name |
| `j` `k`, `↑` `↓`, `ctrl-p` `ctrl-n`, `PgUp` `PgDn`, `g` `G` | move |
| `space` | mark the worktree under the cursor, and move down |
| `a` | mark every listed worktree, or unmark them if they already are |
| `d` | delete the marked worktrees — or the one under the cursor |
| `enter` | open a shell in the worktree, and come back here when it exits |
| `s` | cycle the sort: by name → newest first → oldest first |
| `o` | open the PR in a browser |
| `esc` | clear the filter, or quit when there is none |
| `q`, `ctrl-c`, `ctrl-d` | quit |

While the filter is open, typing extends it, `backspace`/`ctrl-w`/`ctrl-u`
delete a character/word/all of it, the arrows still move, and `enter` or `esc`
returns to the list with the filter still applied. `esc` from there clears it.

### Searching

`/` fuzzy-matches the branch, path, PR number and (when more than one repo is
listed) the repo name, best match first, with the matched characters
highlighted. Space-separated tokens all have to match, so `toolkit cache`
narrows to cache branches in the toolkit repo, and `42` finds a worktree by its
PR number.

### Creating a worktree

`n` asks for a branch name and makes a worktree for it. If the branch already
exists it is checked out; otherwise it is created from whatever the repo's main
worktree has checked out.

New worktrees go in `.grove/` inside the repo, always:

```
~/src/toolkit/                     the repo
~/src/toolkit/.grove/brogan+fix    a worktree for brogan/fix
```

One convention rather than a guess at the layout already in use, so you know
where a worktree will land before you press `n`, and where to look for it
afterwards. A `/` in the branch name becomes `+` in the directory name, since a
`/` would nest instead.

Worktrees made some other way are still listed wherever they are — the
convention governs where grove *puts* things, not what it will show you.

The first time grove creates a worktree in a repo it adds `/.grove/` to that
repo's `.git/info/exclude`, so the worktrees do not turn up as untracked in
every `git status`. That file is local to your clone; grove does not touch a
tracked `.gitignore`, and a repo that already ignores the directory is left
alone.

The checkout runs in the background — it is a full checkout, seconds on a large
repo — so the list keeps drawing and taking keys while it happens. When it
lands, the new worktree is selected, and the filter is cleared if it would have
hidden it.

### Deleting worktrees

`space` marks worktrees — the marks are on the worktrees themselves, so they
survive re-sorting and re-filtering, and you can filter, mark, clear the filter,
filter again, and delete the accumulated set in one go. `d` then asks to
confirm, naming the count, and only `y` proceeds.

Deleting runs in the background: an `rm -rf` of a full checkout takes a moment,
and a batch of them takes minutes, so the list keeps drawing and taking keys
while it works. The footer counts the batch down, the rows still going show
`removing…`, and they leave the list together when the batch finishes rather
than one at a time under your cursor. Keys that would leave the picker are held
back while removals are in flight — quitting would kill them mid-`rm` — with
`ctrl-c` still there if you mean it.

Removals within one repo run one at a time so they cannot race each other's
administrative files; separate repos run in parallel.

Nothing that could lose work is deleted quietly:

- A worktree with uncommitted or untracked files is refused. Those refusals come
  back as a second prompt offering to force exactly the ones that failed, which
  says in as many words that the uncommitted work goes with them; only `f`
  proceeds.
- The branch is never touched, so nothing committed can be lost — only
  `git worktree remove` runs.
- A repo's main worktree is never removed, marked or not.
- A worktree whose directory is already gone is pruned rather than reported as
  an error, which is how `prunable` rows get cleaned up.

### Sorting

`s` cycles the order between by name (repo order, main worktree first),
newest first, and oldest first — the last being the one to reach for when
deciding what to prune. `--sort recent|oldest|name` sets it from the command
line, for the picker and for `--plain`/`--json` alike.

An age sort outranks match score, so it keeps applying while you filter.
Changing the sort moves the cursor to the top of the new order, since that is
the row you asked to see; a list that re-orders on its own as ages arrive keeps
your cursor on the worktree it was already on.

Sorting by age needs every row dated, which the cheap age pass does in well
under a second even for a few hundred worktrees. Rows that have not come back
yet sit at the end rather than jumping around, and the header counts them off
while they land.

### Jumping to a worktree

`enter` opens a shell in the worktree under the cursor and returns you to the
picker when that shell exits, so you can hop between worktrees without retyping
paths.

`ctrl-d` (or `exit`) leaves the worktree shell and puts the picker back up with
your query and cursor where you left them; `esc` from there ends the session and
returns you to the shell you started in, in the directory you started in. The
worktree shell is a child process — it is `$SHELL` started with its working
directory set — so nothing is changed in the calling shell.

To change the calling shell's own directory instead, use `--pick`. `enter` then
prints the selected path to stdout and exits, with the list still drawn on
`/dev/tty` so it stays out of the way of a redirect:

```sh
wt() { cd "$(grove --pick "$@")" || return; }
```

Cancelling exits 130 with nothing on stdout, so `cd` is left alone.

### Options

| Option | Effect |
| --- | --- |
| `-q`, `--query Q` | start with the filter pre-filled; also filters `--plain`/`--json` |
| `-S`, `--sort S` | order by `name` (default), `recent`, or `oldest` |
| `-p`, `--pick` | make `enter` print the selected path and exit, instead of opening a shell |
| `--plain` | force tab-separated output: `path`, `branch`, `head`, `pr`, `flags` |
| `-j`, `--json` | JSON output |
| `-s`, `--status` | include working-tree status and last-modified time in `--plain`/`--json` output |
| `--no-pr` | skip the GitHub lookup |

Exit codes: `0` listed or the picker was left, `1` nothing found or a fatal
error, `2` bad usage, `130` `--pick` cancelled, so nothing was printed.

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
  worktree convention: `worktree-brogan+fix` is pushed as `brogan/fix`.
- In the picker, `git status` is only run for the rows on screen, so a
  248-worktree directory draws immediately and fills in as you scroll.
- Ages come from a second, much cheaper pass — a `git log -1` and a stat, no
  status scan — because sorting needs every row dated at once. Across the 187
  worktrees of a large monorepo that is ~0.9s, where a full status sweep is
  ~24s. The age shown starts as that estimate and is refined upward when the
  full status for a row arrives, which is what notices uncommitted edits.
- `--plain` reports the age as `mtime=<unix seconds>` among the flags, and
  `--json` as `status.last_modified`; `status.modified` next to it is the count
  of modified files, not a time.
- Status reads pass `--no-optional-locks`, so listing worktrees never refreshes
  an index and never disturbs the timestamps it is reporting.
- `--version` names the commit the binary came from — `grove 0.1.0 (58cab722)`,
  with a `-dirty` suffix when the code it was built from had uncommitted
  changes. The dirty check covers what goes into the binary (`src/`,
  `Cargo.toml`), since that is what cargo re-runs the build script for; an
  uncommitted README does not make a build dirty. Built from a source tarball
  rather than a checkout there is no commit to name, and it prints the bare
  `grove 0.1.0`.
