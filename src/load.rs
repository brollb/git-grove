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

pub struct Loader {
    queue: Queue,
    requested: HashSet<(bool, PathBuf)>,
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
