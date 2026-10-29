//! Threads for work that must not hold up the window: the system's threads on native; in a
//! browser build with threads (`target_feature = "atomics"`, a cross-origin isolated page, see
//! docs/web-threads.md) Web Workers that share the module's memory. The browser build without
//! threads can't start one: [`Builder::spawn`] says so, and callers compute in place, as they do
//! when the system refuses a thread.
//!
//! In the browser, the page's own thread must never wait: a wait there would stop the page (the
//! build turns such a wait into a busy loop, `xtask web-threads`, but a busy page is still a
//! frozen one). [`can_block`] says whether this thread may wait, for code that would otherwise
//! join a thread or wait on its pool.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::io;

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
mod web;

/// Starts a thread, like [`std::thread::Builder`].
#[derive(Debug, Default)]
pub struct Builder {
    name: Option<String>,
    stack_size: Option<usize>,
}

impl Builder {
    pub fn new() -> Builder {
        Builder::default()
    }

    /// The thread's name (shown by debuggers and in panic messages).
    pub fn name(mut self, name: impl Into<String>) -> Builder {
        self.name = Some(name.into());
        self
    }

    /// The thread's stack, in bytes. A browser worker has the pool's stack
    /// ([`web::STACK`]) whatever is asked.
    pub fn stack_size(mut self, bytes: usize) -> Builder {
        self.stack_size = Some(bytes);
        self
    }

    /// Run `f` on another thread. In the browser it runs on an idle worker of the pool, which the
    /// page's thread grows up to its size; with no worker free (or no threads in this build) the
    /// error says so, and the caller computes in place.
    pub fn spawn<F, T>(self, f: F) -> io::Result<JoinHandle<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut b = std::thread::Builder::new();
            if let Some(name) = self.name {
                b = b.name(name);
            }
            if let Some(size) = self.stack_size {
                b = b.stack_size(size);
            }
            b.spawn(f).map(|h| JoinHandle { inner: h })
        }
        #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
        {
            let (tx, rx) = std::sync::mpsc::channel();
            web::spawn(
                self.name.as_deref(),
                Box::new(move || {
                    let _ = tx.send(f());
                }),
            )?;
            Ok(JoinHandle { inner: rx, done: std::cell::OnceCell::new() })
        }
        #[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
        {
            let _ = (self, f);
            Err(no_threads())
        }
    }
}

#[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
fn no_threads() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "this browser build has no threads")
}

/// Waits for a thread started by [`Builder::spawn`].
#[derive(Debug)]
pub struct JoinHandle<T> {
    #[cfg(not(target_arch = "wasm32"))]
    inner: std::thread::JoinHandle<T>,
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    inner: std::sync::mpsc::Receiver<T>,
    /// What [`JoinHandle::is_finished`] took from the thread already.
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    done: std::cell::OnceCell<T>,
    #[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
    inner: std::marker::PhantomData<T>,
}

impl<T> JoinHandle<T> {
    /// Wait for the thread's result. `Err` when it panicked (in the browser: when its worker
    /// stopped). Only for a thread that may wait ([`can_block`]).
    pub fn join(self) -> std::thread::Result<T> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.inner.join()
        }
        #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
        {
            let JoinHandle { inner, mut done } = self;
            match done.take() {
                Some(value) => Ok(value),
                None => inner.recv().map_err(|_| Box::new("the thread stopped") as Box<dyn std::any::Any + Send>),
            }
        }
        #[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
        {
            Err(Box::new(no_threads().to_string()))
        }
    }

    /// Has the thread finished (without waiting)?
    pub fn is_finished(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.inner.is_finished()
        }
        #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
        {
            if self.done.get().is_some() {
                return true;
            }
            match self.inner.try_recv() {
                Ok(value) => {
                    let _ = self.done.set(value);
                    true
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => false,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => true,
            }
        }
        #[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
        {
            true
        }
    }
}

/// May this thread wait (join a thread, wait for a pool)? Not the browser page's own thread.
pub fn can_block() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        true
    }
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    {
        web::on_worker()
    }
    #[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
    {
        false
    }
}

/// Can this build start threads at all (in the browser: is the worker pool there)?
pub fn available() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        true
    }
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    {
        web::started()
    }
    #[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
    {
        false
    }
}

/// How many threads can compute at once (the processor's cores; 1 without threads).
pub fn available_parallelism() -> usize {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::available_parallelism().map(usize::from).unwrap_or(1)
    }
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    {
        if web::started() { web::cores() } else { 1 }
    }
    #[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
    {
        1
    }
}

/// Start the browser's worker pool, from the page's thread, once the module runs: `glue` is the
/// URL of the module's JavaScript glue (the page's loader knows it). Nothing to do on native, or
/// in the browser build without threads (`Err` says why there are none).
pub fn start(glue: Option<&str>) -> io::Result<()> {
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    {
        web::start(glue)
    }
    #[cfg(not(all(target_arch = "wasm32", target_feature = "atomics")))]
    {
        let _ = glue;
        #[cfg(target_arch = "wasm32")]
        return Err(no_threads());
        #[cfg(not(target_arch = "wasm32"))]
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_runs_and_hands_back_its_result() {
        let h = Builder::new().name("test").stack_size(256 * 1024).spawn(|| 6 * 7).unwrap();
        assert_eq!(h.join().unwrap(), 42);
        assert!(can_block() && available() && available_parallelism() >= 1);
        assert!(start(None).is_ok());
    }

    #[test]
    fn a_finished_thread_says_so() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let h = Builder::new().spawn(move || rx.recv().is_ok()).unwrap();
        assert!(!h.is_finished());
        tx.send(()).unwrap();
        while !h.is_finished() {
            std::thread::yield_now();
        }
        assert!(h.join().unwrap());
    }
}
