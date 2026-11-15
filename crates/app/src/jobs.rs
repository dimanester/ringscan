//! Background queries over the tree (largest files, file types, search,
//! folder contents).
//!
//! Each query lives in a [`Slot`]: the UI asks for the result of a key every
//! frame with [`Slot::ensure`]; if the cached result has another key and no
//! job is running, a thread is spawned that takes a normal read lock, runs
//! the query and sends the result back (waking the UI with a repaint). The
//! UI keeps showing the previous result until the new one arrives, so a
//! 100 ms search over millions of nodes never stalls a frame.
//!
//! Keys include a "generation bucket": the tree generation when idle, or a
//! time slot while a scan is running, which throttles recomputation during
//! a scan to once per slot instead of once per inserted directory.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use eframe::egui;

pub struct Slot<K, T> {
    current: Option<(K, Arc<T>)>,
    running: Option<(K, Receiver<T>)>,
}

impl<K, T> Default for Slot<K, T> {
    fn default() -> Self {
        Slot {
            current: None,
            running: None,
        }
    }
}

impl<K: Clone + PartialEq + Send + 'static, T: Send + 'static> Slot<K, T> {
    /// Collects a finished job.
    pub fn poll(&mut self) {
        if let Some((_, rx)) = &self.running {
            match rx.try_recv() {
                Ok(v) => {
                    let (k, _) = self.running.take().expect("running");
                    self.current = Some((k, Arc::new(v)));
                }
                Err(TryRecvError::Disconnected) => self.running = None,
                Err(TryRecvError::Empty) => {}
            }
        }
    }

    /// Latest result (possibly for an older key).
    pub fn get(&self) -> Option<&Arc<T>> {
        self.current.as_ref().map(|c| &c.1)
    }

    pub fn current_key(&self) -> Option<&K> {
        self.current.as_ref().map(|c| &c.0)
    }

    pub fn is_fresh(&self, key: &K) -> bool {
        self.current_key() == Some(key)
    }

    pub fn clear(&mut self) {
        self.current = None;
        self.running = None;
    }

    /// Makes sure a result for `key` is (being) computed.
    pub fn ensure(
        &mut self,
        ctx: &egui::Context,
        key: K,
        job: impl FnOnce() -> T + Send + 'static,
    ) {
        self.poll();
        if self.is_fresh(&key) || self.running.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("ringscan-query".into())
            .spawn(move || {
                let v = job();
                let _ = tx.send(v);
                ctx.request_repaint();
            });
        if spawned.is_ok() {
            self.running = Some((key, rx));
        }
    }
}

/// Generation bucket for job keys: the generation itself when no scan runs,
/// otherwise a time slot of `every` seconds (see the module docs).
pub fn gen_bucket(generation: u64, scanning: bool, now: f64, every: f64) -> u64 {
    if scanning {
        u64::MAX - (now / every).floor() as u64
    } else {
        generation
    }
}
