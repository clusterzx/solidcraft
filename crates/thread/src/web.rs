//! The browser's worker pool. Web Workers load the page's module with its memory (shared, the
//! page being cross-origin isolated) and take jobs from one queue. Only the page's thread can
//! start a worker, so the pool starts a couple at once and grows when the page asks for a thread
//! and none is free; a worker that asks when none is free gets an error and computes in place.
//! A worker stays up after its job, waiting for the next one.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError, TryLockError};

use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::*;

/// Each worker's stack (the page's own is 1 MiB).
pub const STACK: usize = 4 << 20;
/// Most workers the pool grows to.
const MAX_WORKERS: usize = 16;
/// Workers started with the pool, so the first jobs don't wait for one to load.
const WARM: usize = 2;

type Job = Box<dyn FnOnce() + Send>;

struct Queue {
    jobs: VecDeque<Job>,
    /// Workers waiting for a job.
    idle: usize,
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue { jobs: VecDeque::new(), idle: 0 });
static READY: Condvar = Condvar::new();
static STARTED: AtomicBool = AtomicBool::new(false);
/// Workers started and not failed.
static WORKERS: AtomicUsize = AtomicUsize::new(0);
/// Workers that came up (reached the queue).
static LIVE: AtomicUsize = AtomicUsize::new(0);
static CORES: AtomicUsize = AtomicUsize::new(1);

thread_local! {
    static WORKER: Cell<bool> = const { Cell::new(false) };
    /// The page thread's workers (kept, or the browser would collect them) and their script.
    static PAGE: RefCell<Option<Page>> = const { RefCell::new(None) };
}

struct Page {
    script: String,
    workers: Vec<web_sys::Worker>,
    on_error: Vec<Closure<dyn FnMut(web_sys::Event)>>,
}

pub fn on_worker() -> bool {
    WORKER.with(Cell::get)
}

pub fn started() -> bool {
    STARTED.load(Ordering::Acquire)
}

pub fn cores() -> usize {
    CORES.load(Ordering::Relaxed)
}

fn unsupported(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, why.to_string())
}

fn js(e: JsValue) -> io::Error {
    io::Error::other(e.as_string().unwrap_or_else(|| format!("{e:?}")))
}

/// The queue. The page's thread spins rather than waits for it (a wait there stops the page);
/// workers hold it only to take or count a job.
fn lock() -> MutexGuard<'static, Queue> {
    if on_worker() {
        return QUEUE.lock().unwrap_or_else(PoisonError::into_inner);
    }
    loop {
        match QUEUE.try_lock() {
            Ok(q) => return q,
            Err(TryLockError::Poisoned(p)) => return p.into_inner(),
            Err(TryLockError::WouldBlock) => std::hint::spin_loop(),
        }
    }
}

pub fn start(glue: Option<&str>) -> io::Result<()> {
    if on_worker() {
        return Err(unsupported("the worker pool starts on the page's thread"));
    }
    if started() {
        return Ok(());
    }
    let glue = glue.ok_or_else(|| unsupported("the page did not say where the module's JavaScript is"))?;
    if glue.contains(['"', '\\', '\n', '\r']) {
        return Err(unsupported("the module's JavaScript has an address this can't load"));
    }
    // The worker: load the page's module with the page's memory, say it may wait (the export
    // `xtask web-threads` adds: without it a wait spins here as on the page), take jobs for good.
    let script = format!(
        "import init, {{ solidcraft_thread_work }} from \"{glue}\";\n\
         self.onmessage = async (event) => {{\n\
         const [module, memory] = event.data;\n\
         const wasm = await init({{ module_or_path: module, memory, thread_stack_size: {STACK} }});\n\
         wasm.solidcraft_thread_can_block?.();\n\
         solidcraft_thread_work();\n\
         }};\n"
    );
    let parts = js_sys::Array::of1(&JsValue::from_str(&script));
    let props = web_sys::BlobPropertyBag::new();
    props.set_type("text/javascript");
    let blob = web_sys::Blob::new_with_str_sequence_and_options(&parts, &props).map_err(js)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(js)?;
    let cores = web_sys::window().map(|w| w.navigator().hardware_concurrency()).filter(|c| c.is_finite() && *c >= 1.0).unwrap_or(1.0);
    CORES.store((cores as usize).clamp(1, MAX_WORKERS), Ordering::Relaxed);
    PAGE.with(|p| *p.borrow_mut() = Some(Page { script: url, workers: Vec::new(), on_error: Vec::new() }));
    for _ in 0..WARM {
        start_worker()?;
    }
    STARTED.store(true, Ordering::Release);
    Ok(())
}

pub fn spawn(_name: Option<&str>, job: Job) -> io::Result<()> {
    if !started() {
        return Err(unsupported("the worker pool has not started"));
    }
    {
        let mut q = lock();
        if q.idle > q.jobs.len() {
            q.jobs.push_back(job);
            drop(q);
            READY.notify_one();
            return Ok(());
        }
    }
    if on_worker() || WORKERS.load(Ordering::Acquire) >= MAX_WORKERS {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "every worker is busy"));
    }
    // A new worker takes it once it is up (or one that finishes first does).
    start_worker()?;
    lock().jobs.push_back(job);
    READY.notify_one();
    Ok(())
}

fn start_worker() -> io::Result<()> {
    PAGE.with(|page| {
        let mut page = page.borrow_mut();
        let page = page.as_mut().ok_or_else(|| unsupported("the worker pool has not started"))?;
        let options = web_sys::WorkerOptions::new();
        options.set_type(web_sys::WorkerType::Module);
        options.set_name(&format!("solidcraft-{}", page.workers.len() + 1));
        let worker = web_sys::Worker::new_with_options(&page.script, &options).map_err(js)?;
        let on_error = Closure::<dyn FnMut(web_sys::Event)>::new(failed);
        worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        worker.post_message(&js_sys::Array::of2(&wasm_bindgen::module(), &wasm_bindgen::memory())).map_err(js)?;
        page.workers.push(worker);
        page.on_error.push(on_error);
        WORKERS.fetch_add(1, Ordering::AcqRel);
        Ok(())
    })
}

/// A worker failed (it could not load the module, or its job stopped it). When none ever came up
/// the pool is off: what waits in the queue is dropped (its callers see their thread gone and
/// compute in place) and threads are refused from here on.
fn failed(_: web_sys::Event) {
    let left = WORKERS.fetch_sub(1, Ordering::AcqRel).saturating_sub(1);
    web_sys::console::warn_1(&JsValue::from_str("SolidCraft: a worker thread stopped"));
    if left == 0 && LIVE.load(Ordering::Acquire) == 0 {
        STARTED.store(false, Ordering::Release);
        let dropped: Vec<Job> = lock().jobs.drain(..).collect();
        drop(dropped);
    }
}

/// A worker's life: take the next job and run it, for good. Called by the worker's script once
/// the module is up.
#[wasm_bindgen]
pub fn solidcraft_thread_work() {
    WORKER.with(|w| w.set(true));
    LIVE.fetch_add(1, Ordering::AcqRel);
    loop {
        let job = {
            let mut q = lock();
            loop {
                if let Some(job) = q.jobs.pop_front() {
                    break job;
                }
                q.idle += 1;
                q = READY.wait(q).unwrap_or_else(PoisonError::into_inner);
                q.idle = q.idle.saturating_sub(1);
            }
        };
        job();
    }
}
