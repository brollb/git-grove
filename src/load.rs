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
    /// PRs for the repo at this index, or the reason they are unavailable.
    Prs(usize, Result<HashMap<String, Pr>, String>),
}

type Queue = Arc<(Mutex<Vec<PathBuf>>, Condvar)>;

pub struct Loader {
    queue: Queue,
    requested: HashSet<PathBuf>,
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
                tx,
            },
            rx,
        )
    }

    /// Queue a status lookup, unless one was already requested for this path.
    pub fn request_status(&mut self, path: &Path) {
        if !self.requested.insert(path.to_path_buf()) {
            return;
        }
        let (lock, cv) = &*self.queue;
        lock.lock().unwrap().push(path.to_path_buf());
        cv.notify_one();
    }

    /// Drop the memo for `path` so its status is recomputed on next request —
    /// used after returning from a shell that may have changed the worktree.
    pub fn forget(&mut self, path: &Path) {
        self.requested.remove(path);
    }

    pub fn pending(&self) -> usize {
        self.requested.len()
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
        let path = {
            let (lock, cv) = &*queue;
            let mut pending = lock.lock().unwrap();
            loop {
                match pending.pop() {
                    Some(p) => break p,
                    None => pending = cv.wait(pending).unwrap(),
                }
            }
        };
        if tx
            .send(Msg::Status(path.clone(), git::status(&path)))
            .is_err()
        {
            return; // receiver gone: we are shutting down
        }
    }
}
