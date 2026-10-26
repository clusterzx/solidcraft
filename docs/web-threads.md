# Threads in the browser

The desktop app computes slow work beside its window, so the window keeps drawing while it runs:
- a feature dialog's OK and its live preview;
- the history marker, undo and redo, and a delete's check;
- an agent's commands (Preferences ▸ AI ▸ *Keep the window responsive while agents run commands*);
- Save;
- the display meshes of a design that has just opened;
- the picking index and the levels of detail of large meshes.

The web app does the same where the browser lets a page share memory with its workers. Elsewhere
it computes all of this on the page's own thread, and the page stops responding until each one
is done.

## What a site needs

The page must be *cross-origin isolated*. The site sends these headers with every file:

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

The Docker image's nginx sends them (`packaging/docker/nginx.conf`). Every file of the app
comes from its own site, so nothing else is needed. A reverse proxy in front must pass the
headers on. Check in the browser's console that `crossOriginIsolated` is `true`.

A page that is not isolated runs the build without threads. This covers a host that can't set
headers (GitHub Pages), an older browser, and an embedded view (`?embed`) in another site's
frame. That build is the same app, only without the work beside the page.

## How it is built

`trunk build --release` in `apps/solidcraft-web` builds the site as before. Then
`cargo xtask web-threads dist/web` adds the build with threads beside it. The Docker image and
the Docker workflow run both.

1. **The build.** The app is built again from `apps/solidcraft-web/threads.html`, with
   `-C target-feature=+atomics,+bulk-memory,+mutable-globals`. Its memory is shared and may grow
   to 4 GiB. It needs a standard library built with the same features, so it uses `-Z build-std`.
   On the stable compiler this goes through `RUSTC_BOOTSTRAP=1`, and it needs the standard
   library's source (`rustup component add rust-src`). Its cache is `target/wasm-threads`.
2. **Waits made safe.** A browser stops the whole page when its own thread waits on shared
   memory (`memory.atomic.wait`). This happens whenever a lock is held by a worker, a thread's
   result is not in yet, or a one-time value is still being made. The command rewrites every
   such wait in the module into a call.
   - On a worker, the call waits as before. Each worker marks itself through the module's
     `solidcraft_thread_can_block` export.
   - On the page's thread, it spins until the value changes. A timed wait there returns at once,
     as though it had timed out.

   The page freezes for as long as such a wait lasts, but it never fails.
3. **The loader.** `index.html`'s module script loads the build with threads only where it can
   run: the page is cross-origin isolated and can make the module's shared memory. Everywhere
   else it loads the build without threads. The service worker keeps both builds.

## In the code

`crates/thread` (`solidcraft-thread`) starts threads.
- **On native:** the system's threads.
- **In the build with threads:** Web Workers from a pool that the page's thread starts and grows,
  up to 16 workers. Each worker loads the page's module with its memory and takes jobs from one
  queue.
- **In the build without threads:** `Builder::spawn` says it can't start one, and the code
  computes in place, as it does when the system refuses a thread.

Code that wants a thread uses `solidcraft_thread::Builder` instead of `std::thread::Builder`.
- Don't make the page's thread wait for one: no `join`, no `recv`, no `recv_timeout`.
- Look for its result each frame, as the off-thread paths above do (`try_recv`).
- `solidcraft_ui_egui`'s `within(rx, wait)` takes what is in, without waiting, on a thread that
  may not wait.
- `solidcraft_thread::can_block()` says whether this thread may wait.

The web app's in-memory files (`solidcraft_engine::io::vfs` in the browser) are one store for
the page and its workers.

A panic in the browser stops the thread it happens on.
- On a worker, the worker stops and the browser's console says so.
- What waited for that worker's result stays waiting, for example a dialog that shows it is
  computing. A dialog's Cancel or the agent chip's Stop ends that wait.
- The design on the page is not changed, because the worker computed on a copy.
