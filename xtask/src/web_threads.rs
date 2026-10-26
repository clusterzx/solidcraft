//! `cargo xtask web-threads [DIST] [--from SITE]`: add the web app's build with threads to a built
//! site (default `dist/web`, made by `trunk build --release` in apps/solidcraft-web;
//! docs/web-threads.md).
//!
//! 1. Build the app again with threads (`+atomics`, its own standard library: `-Z build-std`, on
//!    the stable compiler through `RUSTC_BOOTSTRAP`) from `apps/solidcraft-web/threads.html`
//!    (`Trunk.threads.toml`). `--from SITE` takes that build from SITE instead (trunk's output).
//! 2. Make every wait in its module safe on the page's thread: a browser stops the whole page on
//!    a `memory.atomic.wait` there, so each becomes a call that waits on a worker and, on the
//!    page's thread, spins until the value changes (a timed wait there times out at once). Each
//!    worker marks itself by calling the module's `solidcraft_thread_can_block` export.
//! 3. Keep that module and its glue next to the site's own, and change the page's loader: a page
//!    that is cross-origin isolated (and can make the module's shared memory) loads the build
//!    with threads, any other the one without.
//! 4. Write the site's service worker again (`web-offline`), so it keeps both builds.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use walrus::ir::{AtomicWait, BinaryOp, Call, Instr, InstrLocId, LoadKind, MemArg, Value, VisitorMut};
use walrus::{ConstExpr, FunctionBuilder, FunctionId, GlobalId, MemoryId, ValType};

/// The memory the build with threads may grow to (the most a 32-bit module has: 4 GiB).
pub const MAX_MEMORY: u64 = 4 << 30;
/// The export a worker calls to say it may wait.
pub const CAN_BLOCK_EXPORT: &str = "solidcraft_thread_can_block";
/// The loader's mark in index.html (so a site gets the build with threads once).
const LOADER_MARK: &str = "solidcraft-threads-loader";

pub fn run(root: &Path, args: &[&str]) -> Result<(), String> {
    let mut dist = root.join("dist/web");
    let mut from = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "--from" => from = Some(PathBuf::from(it.next().ok_or("--from needs a directory")?)),
            d if !d.starts_with("--") => dist = PathBuf::from(d),
            other => return Err(format!("web-threads: unknown option {other}")),
        }
    }
    let index =
        std::fs::read_to_string(dist.join("index.html")).map_err(|e| format!("{}: {e} (build the site first)", dist.join("index.html").display()))?;
    let single = trunk_module(&index)?;
    let out = match from {
        Some(site) => site,
        None => {
            let out = root.join("target/web-threads-site");
            let _ = std::fs::remove_dir_all(&out);
            build(root, &out)?;
            out
        }
    };
    let built = trunk_module(&std::fs::read_to_string(out.join("index.html")).map_err(|e| format!("{}: {e}", out.join("index.html").display()))?)?;
    let wasm = std::fs::read(out.join(&built.wasm)).map_err(|e| format!("{}: {e}", built.wasm))?;
    let (wasm, waits) = rewrite_waits(&wasm)?;
    let hash = hex(&Sha256::digest(&wasm));
    let name = format!("solidcraft-web-mt-{}", hash.get(..16).unwrap_or(&hash));
    let threaded = Bundle { js: format!("{name}.js"), wasm: format!("{name}_bg.wasm") };
    let glue = std::fs::read_to_string(out.join(&built.js)).map_err(|e| format!("{}: {e}", built.js))?;
    std::fs::write(dist.join(&threaded.wasm), &wasm).map_err(|e| format!("{}: {e}", threaded.wasm))?;
    std::fs::write(dist.join(&threaded.js), glue.replace(&built.wasm, &threaded.wasm)).map_err(|e| format!("{}: {e}", threaded.js))?;
    std::fs::write(dist.join("index.html"), loader(&index, &single, &threaded)?).map_err(|e| format!("index.html: {e}"))?;
    let (version, n) = crate::web_offline::generate(root, &dist)?;
    println!("web-threads: {} ({waits} waits made safe on the page's thread); sw.js keeps {n} files (version {version})", threaded.wasm);
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the app with threads into `out` (a site of its own: only its module is kept).
fn build(root: &Path, out: &Path) -> Result<(), String> {
    let flags = format!("-C target-feature=+atomics,+bulk-memory,+mutable-globals -C link-arg=--max-memory={MAX_MEMORY}");
    let status = Command::new("trunk")
        .current_dir(root.join("apps/solidcraft-web"))
        .args(["--config", "Trunk.threads.toml", "build", "--release", "--dist"])
        .arg(out)
        // The wasm only: build scripts and proc macros stay as they are.
        .env("CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS", flags)
        .env("RUSTC_BOOTSTRAP", "1")
        .env("CARGO_UNSTABLE_BUILD_STD", "std,panic_abort")
        .env("CARGO_TARGET_DIR", root.join("target/wasm-threads"))
        // wasm-opt drops it anyway; it would only fill the disk.
        .env("CARGO_PROFILE_RELEASE_DEBUG", "false")
        .status()
        .map_err(|e| format!("trunk: {e} (install it: cargo install trunk)"))?;
    if !status.success() {
        return Err(format!(
            "trunk build of the app with threads failed ({status}); it needs the standard library's source: rustup component add rust-src"
        ));
    }
    Ok(())
}

/// A trunk-built module: its glue and its wasm.
#[derive(Debug, PartialEq)]
struct Bundle {
    js: String,
    wasm: String,
}

/// The module trunk's loader in `index` starts.
fn trunk_module(index: &str) -> Result<Bundle, String> {
    let after = |key: &str| -> Option<String> {
        let start = index.find(key)? + key.len();
        let end = start + index.get(start..)?.find('\'')?;
        Some(index.get(start..end)?.trim_start_matches("./").to_string())
    };
    let js = after("from '").ok_or("index.html: no module loader (a trunk build of apps/solidcraft-web?)")?;
    let wasm = after("module_or_path: '").ok_or("index.html: the loader names no wasm")?;
    Ok(Bundle { js, wasm })
}

/// `index` with a loader that starts `threaded` where the page can run it, else `single`; trunk's
/// preloads of `single` go (a page with threads would download it for nothing).
fn loader(index: &str, single: &Bundle, threaded: &Bundle) -> Result<String, String> {
    if index.contains(LOADER_MARK) {
        return Err("index.html: already has the build with threads (build the site again first)".into());
    }
    let start = index.find("<script type=\"module\">").ok_or("index.html: no module script")?;
    let end = start + index.get(start..).and_then(|s| s.find("</script>")).ok_or("index.html: the module script has no end")? + "</script>".len();
    if !index.get(start..end).is_some_and(|s| s.contains(&single.js)) {
        return Err("index.html: the module script doesn't start the app".into());
    }
    let pages = MAX_MEMORY / 65536;
    let script = format!(
        r#"<script type="module" id="{LOADER_MARK}">
// Threads where the page is cross-origin isolated and can make the module's shared memory
// (docs/web-threads.md); the build without them anywhere else.
const threads = (() => {{ try {{ return self.crossOriginIsolated === true && !!new WebAssembly.Memory({{ initial: 1, maximum: {pages}, shared: true }}); }} catch {{ return false; }} }})();
const [js, bin] = threads ? ['./{tjs}', './{twasm}'] : ['./{sjs}', './{swasm}'];
if (threads) globalThis.solidcraftGlue = new URL(js, location.href).href;
const bindings = await import(js);
const wasm = await bindings.default({{ module_or_path: bin }});
window.wasmBindings = bindings;
dispatchEvent(new CustomEvent("TrunkApplicationStarted", {{ detail: {{ wasm }} }}));
</script>"#,
        tjs = threaded.js,
        twasm = threaded.wasm,
        sjs = single.js,
        swasm = single.wasm,
    );
    let mut html = format!("{}{script}{}", index.get(..start).unwrap_or_default(), index.get(end..).unwrap_or_default());
    // Trunk's preloads of the build without threads.
    for file in [&single.js, &single.wasm] {
        while let Some(at) = html.find(&format!("href=\"./{file}\"")) {
            let open = html.get(..at).and_then(|s| s.rfind("<link")).ok_or("index.html: a preload outside a link")?;
            let close = at + html.get(at..).and_then(|s| s.find('>')).ok_or("index.html: an unclosed preload")? + 1;
            html.replace_range(open..close, "");
        }
    }
    Ok(html)
}

/// A kind of wait: its memory, offset, alignment and width (64-bit or not).
type WaitKind = (MemoryId, u64, u32, bool);

/// Turn every `memory.atomic.wait` in `wasm` into a call that waits only where the thread may
/// (a worker that called [`CAN_BLOCK_EXPORT`]) and spins on the page's thread. Returns the module
/// and how many waits it had.
pub fn rewrite_waits(wasm: &[u8]) -> Result<(Vec<u8>, usize), String> {
    let mut module = walrus::ModuleConfig::new().parse(wasm).map_err(|e| format!("wasm: {e}"))?;
    if module.exports.iter().any(|e| e.name == CAN_BLOCK_EXPORT) {
        return Err("wasm: its waits are made safe already".into());
    }
    // Which waits there are (by memory, offset, alignment and width).
    struct Find(Vec<WaitKind>);
    impl VisitorMut for Find {
        fn visit_instr_mut(&mut self, instr: &mut Instr, _: &mut InstrLocId) {
            if let Instr::AtomicWait(AtomicWait { memory, arg, sixty_four }) = instr {
                let key = (*memory, arg.offset, arg.align, *sixty_four);
                if !self.0.contains(&key) {
                    self.0.push(key);
                }
            }
        }
    }
    let mut kinds = Find(Vec::new());
    for (_, f) in module.funcs.iter_local_mut() {
        let entry = f.entry_block();
        walrus::ir::dfs_pre_order_mut(&mut kinds, f, entry);
    }
    let can_block = module.globals.add_local(ValType::I32, true, false, ConstExpr::Value(Value::I32(0)));
    let mut mark = FunctionBuilder::new(&mut module.types, &[], &[]);
    mark.func_body().i32_const(1).global_set(can_block);
    let mark = mark.finish(Vec::new(), &mut module.funcs);
    module.exports.add(CAN_BLOCK_EXPORT, mark);
    let helpers: Vec<(WaitKind, FunctionId)> = kinds.0.iter().map(|&key| (key, wait_helper(&mut module, can_block, key))).collect();
    // Replace each wait with its helper (the helpers keep theirs).
    struct Replace<'a> {
        helpers: &'a [(WaitKind, FunctionId)],
        n: usize,
    }
    impl VisitorMut for Replace<'_> {
        fn visit_instr_mut(&mut self, instr: &mut Instr, _: &mut InstrLocId) {
            if let Instr::AtomicWait(AtomicWait { memory, arg, sixty_four }) = instr {
                let key = (*memory, arg.offset, arg.align, *sixty_four);
                if let Some((_, func)) = self.helpers.iter().find(|(k, _)| *k == key) {
                    *instr = Instr::Call(Call { func: *func });
                    self.n += 1;
                }
            }
        }
    }
    let mut replace = Replace { helpers: &helpers, n: 0 };
    let own: Vec<FunctionId> = helpers.iter().map(|(_, f)| *f).collect();
    for (id, f) in module.funcs.iter_local_mut() {
        if own.contains(&id) {
            continue;
        }
        let entry = f.entry_block();
        walrus::ir::dfs_pre_order_mut(&mut replace, f, entry);
    }
    let n = replace.n;
    Ok((module.emit_wasm(), n))
}

/// `(addr, expected, timeout) -> result` as the wait it stands for returns it: on a thread that
/// may block, that wait; on the page's thread, 1 ("not equal") once the value differs, or 2
/// ("timed out") at once for a timed wait.
fn wait_helper(module: &mut walrus::Module, can_block: GlobalId, (memory, offset, align, wide): WaitKind) -> FunctionId {
    let value = if wide { ValType::I64 } else { ValType::I32 };
    let mut b = FunctionBuilder::new(&mut module.types, &[ValType::I32, value, ValType::I64], &[ValType::I32]);
    let addr = module.locals.add(ValType::I32);
    let expected = module.locals.add(value);
    let timeout = module.locals.add(ValType::I64);
    let arg = MemArg { align, offset };
    let load = if wide { LoadKind::I64 { atomic: true } } else { LoadKind::I32 { atomic: true } };
    let differs = if wide { BinaryOp::I64Ne } else { BinaryOp::I32Ne };
    let mut body = b.func_body();
    body.global_get(can_block);
    body.if_else(
        ValType::I32,
        |wait| {
            wait.local_get(addr).local_get(expected).local_get(timeout).atomic_wait(memory, arg, wide);
        },
        |spin| {
            spin.block(ValType::I32, |done| {
                let done_id = done.id();
                done.loop_(None, |each| {
                    let again = each.id();
                    each.local_get(addr).load(memory, load, arg).local_get(expected).binop(differs);
                    each.if_else(
                        None,
                        |changed| {
                            changed.i32_const(1).br(done_id);
                        },
                        |_| {},
                    );
                    each.local_get(timeout).i64_const(0).binop(BinaryOp::I64GeS);
                    each.if_else(
                        None,
                        |timed| {
                            timed.i32_const(2).br(done_id);
                        },
                        |_| {},
                    );
                    each.br(again);
                });
                done.unreachable();
            });
        },
    );
    b.finish(vec![addr, expected, timeout], &mut module.funcs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = r#"<!DOCTYPE html><html><head>
<script type="module">
import init, * as bindings from './solidcraft-web-0123456789abcdef.js';
const wasm = await init({ module_or_path: './solidcraft-web-0123456789abcdef_bg.wasm' });
window.wasmBindings = bindings;
dispatchEvent(new CustomEvent("TrunkApplicationStarted", {detail: {wasm}}));
</script>
<link rel="stylesheet" href="./loading.css"/>
<link rel="modulepreload" href="./solidcraft-web-0123456789abcdef.js" crossorigin="anonymous" integrity="sha384-x"><link rel="preload" href="./solidcraft-web-0123456789abcdef_bg.wasm" crossorigin="anonymous" integrity="sha384-y" as="fetch" type="application/wasm"></head>
<body><canvas id="solidcraft_canvas"></canvas></body></html>"#;

    #[test]
    fn the_loader_starts_threads_only_where_the_page_can_run_them() {
        let single = trunk_module(INDEX).unwrap();
        assert_eq!(single, Bundle { js: "solidcraft-web-0123456789abcdef.js".into(), wasm: "solidcraft-web-0123456789abcdef_bg.wasm".into() });
        let threaded = Bundle { js: "solidcraft-web-mt-1111.js".into(), wasm: "solidcraft-web-mt-1111_bg.wasm".into() };
        let html = loader(INDEX, &single, &threaded).unwrap();
        assert!(html.contains("self.crossOriginIsolated === true") && html.contains("shared: true") && html.contains("maximum: 65536"));
        assert!(html.contains("['./solidcraft-web-mt-1111.js', './solidcraft-web-mt-1111_bg.wasm']"));
        assert!(html.contains("['./solidcraft-web-0123456789abcdef.js', './solidcraft-web-0123456789abcdef_bg.wasm']"));
        assert!(html.contains("globalThis.solidcraftGlue") && html.contains("TrunkApplicationStarted"));
        assert!(!html.contains("modulepreload") && !html.contains("rel=\"preload\""), "{html}");
        assert!(html.contains("loading.css") && html.contains("solidcraft_canvas"));
        assert_eq!(html.matches("<script").count(), 1);
        assert!(loader(&html, &single, &threaded).is_err(), "a site gets the build with threads once");
    }

    /// A module with shared memory and a function that waits, as `std` compiles a lock.
    fn waiting_module() -> Vec<u8> {
        let mut m = walrus::Module::default();
        let memory = m.memories.add_local(true, false, 1, Some(16), None);
        let mut f = FunctionBuilder::new(&mut m.types, &[ValType::I32, ValType::I32], &[ValType::I32]);
        let (a, e) = (m.locals.add(ValType::I32), m.locals.add(ValType::I32));
        f.func_body().local_get(a).local_get(e).i64_const(-1).atomic_wait(memory, MemArg { align: 4, offset: 8 }, false);
        let f = f.finish(vec![a, e], &mut m.funcs);
        m.exports.add("lock", f);
        m.emit_wasm()
    }

    #[test]
    fn every_wait_becomes_one_the_page_thread_survives() {
        let (wasm, n) = rewrite_waits(&waiting_module()).unwrap();
        assert_eq!(n, 1);
        let mut m = walrus::ModuleConfig::new().parse(&wasm).unwrap();
        assert!(m.exports.iter().any(|e| e.name == CAN_BLOCK_EXPORT));
        // The one wait left is the helper's, behind the check of the thread's mark.
        let mut waits = 0;
        struct Count<'a>(&'a mut usize);
        impl VisitorMut for Count<'_> {
            fn visit_instr_mut(&mut self, instr: &mut Instr, _: &mut InstrLocId) {
                if let Instr::AtomicWait(w) = instr {
                    assert_eq!((w.arg.offset, w.arg.align, w.sixty_four), (8, 4, false));
                    *self.0 += 1;
                }
            }
        }
        for (_, f) in m.funcs.iter_local_mut() {
            let entry = f.entry_block();
            walrus::ir::dfs_pre_order_mut(&mut Count(&mut waits), f, entry);
        }
        assert_eq!(waits, 1);
        assert!(rewrite_waits(&wasm).is_err(), "made safe once");
    }
}
