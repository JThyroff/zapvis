use anyhow::{anyhow, Result};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc::{channel, Sender},
};
use std::thread;
use zapvis::PersistentSsh;

/// Shared range state for remote worker to check if requests are still needed
#[derive(Clone)]
pub struct RemoteRange {
    min: Arc<AtomicU64>,
    max: Arc<AtomicU64>,
}

impl RemoteRange {
    pub fn new() -> Self {
        Self {
            min: Arc::new(AtomicU64::new(0)),
            max: Arc::new(AtomicU64::new(u64::MAX)),
        }
    }

    pub fn set(&self, min: u64, max: u64) {
        self.min.store(min, Ordering::Relaxed);
        self.max.store(max, Ordering::Relaxed);
    }

    pub fn contains(&self, idx: u64) -> bool {
        let min = self.min.load(Ordering::Relaxed);
        let max = self.max.load(Ordering::Relaxed);
        idx >= min && idx <= max
    }
}

/// Request sent to the remote worker thread
pub enum RemoteWorkerRequest {
    Exists {
        path: String,
        response_tx: Sender<Result<bool>>,
    },
    Cat {
        idx: u64,
        path: String,
        response_tx: Sender<Result<Vec<u8>>>,
    },
}

/// Spawn a remote worker thread that exclusively owns the SSH connection
/// and processes requests serially. Returns the request sender.
pub fn spawn_remote_worker(ssh: PersistentSsh, range: RemoteRange) -> Sender<RemoteWorkerRequest> {
    let (tx, rx) = channel::<RemoteWorkerRequest>();

    thread::spawn(move || {
        let mut ssh = ssh;
        while let Ok(req) = rx.recv() {
            match req {
                RemoteWorkerRequest::Exists { path, response_tx } => {
                    eprintln!("[SSH worker] executing: exists {}", path);
                    let result = ssh.exists(&path);
                    let _ = response_tx.send(result);
                }
                RemoteWorkerRequest::Cat { idx, path, response_tx } => {
                    // Check if idx is still in range before executing expensive cat
                    if !range.contains(idx) {
                        eprintln!("[SSH worker] cat SKIP idx={} (out of range)", idx);
                        let _ = response_tx.send(Err(anyhow!("cancelled: out of range")));
                        continue;
                    }

                    eprintln!("[SSH worker] executing: cat {} (idx={})", path, idx);
                    let result = ssh.cat(&path);
                    if let Ok(ref bytes) = result {
                        eprintln!("[SSH worker] cat result: {} bytes", bytes.len());
                    } else {
                        eprintln!("[SSH worker] cat error");
                    }
                    let _ = response_tx.send(result);
                }
            }
        }
        eprintln!("[SSH worker] exiting");
    });

    tx
}
