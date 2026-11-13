//! Background loading: run a closure on a worker thread and poll the result from the UI loop.

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

pub struct Loader<T> {
    pub data: Option<T>,
    pub error: Option<String>,
    rx: Option<Receiver<anyhow::Result<T>>>,
    pub at: Option<Instant>,
}

impl<T> Default for Loader<T> {
    fn default() -> Self {
        Self { data: None, error: None, rx: None, at: None }
    }
}

impl<T: Send + 'static> Loader<T> {
    pub fn load<F>(&mut self, f: F)
    where
        F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    {
        if self.rx.is_some() {
            return; // already loading
        }
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(f());
        });
    }

    /// Returns true when new data (or an error) arrived.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else { return false };
        match rx.try_recv() {
            Ok(Ok(v)) => {
                self.data = Some(v);
                self.error = None;
            }
            Ok(Err(e)) => {
                self.error = Some(format!("{e:#}"));
            }
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => {
                self.error = Some("worker thread died".into());
            }
        }
        self.rx = None;
        self.at = Some(Instant::now());
        true
    }

    pub fn loading(&self) -> bool {
        self.rx.is_some()
    }

    pub fn stale(&self, ttl: Duration) -> bool {
        !self.loading() && self.at.map(|t| t.elapsed() >= ttl).unwrap_or(true)
    }

    pub fn clear(&mut self) {
        self.data = None;
        self.error = None;
        self.rx = None;
        self.at = None;
    }
}

/// A one-shot background job whose result is consumed once.
pub struct Job<T> {
    rx: Receiver<anyhow::Result<T>>,
}

impl<T: Send + 'static> Job<T> {
    pub fn spawn<F>(f: F) -> Self
    where
        F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
        Self { rx }
    }

    pub fn poll(&self) -> Option<anyhow::Result<T>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!("worker thread died"))),
        }
    }
}
