//! Background loading of the slow columns: `git status` per worktree and open
//! PRs per repo. Status work is queued LIFO so the visible rows — the ones the
//! picker asks for last — are computed first.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::gh::{self, Pr};
use crate::git::{self, Status};

pub enum Msg {
    Status(PathBuf, Status),
    /// A worktree removal finished, successfully or not.
    Removed(PathBuf, Result<(), String>),
    /// A worktree creation finished: the repo it belongs to, and the new path.
    Added(usize, Result<PathBuf, String>),
    /// The cheap age estimate for a worktree.
    Age(PathBuf, Option<u64>),
    /// PRs for the repo at this index, or the reason they are unavailable.
    Prs(usize, Result<HashMap<String, Pr>, String>),
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Status,
    Age,
}

type Queue = Arc<(Mutex<Vec<(Kind, PathBuf)>>, Condvar)>;

/// Work that changes which worktrees a repo has.
enum RepoJob {
    Remove {
        path: PathBuf,
        force: bool,
    },
    Add {
        repo: usize,
        path: PathBuf,
        branch: String,
    },
}

pub struct Loader {
    queue: Queue,
    requested: HashSet<(bool, PathBuf)>,
    /// One job channel per repo — see [`Loader::repo_job`].
    removers: HashMap<PathBuf, Sender<RepoJob>>,
    tx: Sender<Msg>,
}

impl Loader {
    pub fn new(workers: usize) -> (Loader, Receiver<Msg>) {
        let (tx, rx) = channel();
        let queue: Queue = Arc::new((Mutex::new(Vec::new()), Condvar::new()));

        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let tx = tx.clone();
            thread::spawn(move || worker(queue, tx));
        }

        (
            Loader {
                queue,
                requested: HashSet::new(),
                removers: HashMap::new(),
                tx,
            },
            rx,
        )
    }

    /// Queue a full status lookup. Returns whether it was newly queued, so
    /// callers can count the replies they are waiting for.
    pub fn request_status(&mut self, path: &Path) -> bool {
        self.request(Kind::Status, path)
    }

    /// Queue a cheap age lookup.
    pub fn request_age(&mut self, path: &Path) -> bool {
        self.request(Kind::Age, path)
    }

    fn request(&mut self, kind: Kind, path: &Path) -> bool {
        if !self
            .requested
            .insert((kind == Kind::Status, path.to_path_buf()))
        {
            return false;
        }
        let (lock, cv) = &*self.queue;
        // LIFO: the picker asks for the rows on screen last, so they run first.
        lock.lock().unwrap().push((kind, path.to_path_buf()));
        cv.notify_one();
        true
    }

    /// Delete a worktree off the UI thread, reporting the result as a message.
    pub fn remove(&mut self, repo_main: &Path, path: PathBuf, force: bool) {
        self.repo_job(repo_main, RepoJob::Remove { path, force });
    }

    /// Create a worktree off the UI thread, reporting the result as a message.
    pub fn add(&mut self, repo_main: &Path, repo: usize, path: PathBuf, branch: String) {
        self.repo_job(repo_main, RepoJob::Add { repo, path, branch });
    }

    /// Queue work that mutates a repo's worktrees.
    ///
    /// Each repo gets one thread, so this work runs one item at a time within a
    /// repo and cannot race another item's administrative files (a `prune` for
    /// one stale worktree could otherwise clear an entry a removal is still
    /// working on), while separate repos proceed in parallel. Both adding and
    /// removing move a whole checkout around, so neither belongs on the thread
    /// that draws the screen.
    fn repo_job(&mut self, repo_main: &Path, job: RepoJob) {
        let tx = self.tx.clone();
        let repo_main = repo_main.to_path_buf();
        let sender = self
            .removers
            .entry(repo_main.clone())
            .or_insert_with(move || {
                let (jobs_tx, jobs_rx) = channel::<RepoJob>();
                thread::spawn(move || {
                    for job in jobs_rx {
                        let msg = match job {
                            RepoJob::Remove { path, force } => {
                                let result = git::remove_worktree(&repo_main, &path, force);
                                Msg::Removed(path, result)
                            }
                            RepoJob::Add { repo, path, branch } => {
                                let result = git::add_worktree(&repo_main, &path, &branch)
                                    .map(|()| path.clone());
                                Msg::Added(repo, result)
                            }
                        };
                        if tx.send(msg).is_err() {
                            return; // receiver gone: we are shutting down
                        }
                    }
                });
                jobs_tx
            });
        let _ = sender.send(job);
    }

    /// Drop the memos for `path` so it is recomputed on next request — used
    /// after returning from a shell that may have changed the worktree.
    pub fn forget(&mut self, path: &Path) {
        self.requested.remove(&(true, path.to_path_buf()));
        self.requested.remove(&(false, path.to_path_buf()));
    }

    pub fn spawn_prs(&self, repo: usize, dir: PathBuf) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Msg::Prs(repo, gh::fetch(&dir)));
        });
    }
}

fn worker(queue: Queue, tx: Sender<Msg>) {
    loop {
        let (kind, path) = {
            let (lock, cv) = &*queue;
            let mut pending = lock.lock().unwrap();
            loop {
                match pending.pop() {
                    Some(p) => break p,
                    None => pending = cv.wait(pending).unwrap(),
                }
            }
        };
        let msg = match kind {
            Kind::Status => Msg::Status(path.clone(), git::status(&path)),
            Kind::Age => Msg::Age(path.clone(), git::age(&path)),
        };
        if tx.send(msg).is_err() {
            return; // receiver gone: we are shutting down
        }
    }
}
