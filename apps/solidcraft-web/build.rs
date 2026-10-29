//! The web app's build with threads (`+atomics`, docs/web-threads.md) links a memory the page
//! shares with its workers: imported by the module's glue, which needs the thread-local storage
//! exports wasm-bindgen's threads support looks for. The build without threads links as before.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let wasm = std::env::var("CARGO_CFG_TARGET_ARCH").is_ok_and(|a| a == "wasm32");
    let atomics = std::env::var("CARGO_CFG_TARGET_FEATURE").is_ok_and(|f| f.split(',').any(|f| f == "atomics"));
    if wasm && atomics {
        for arg in
            ["--shared-memory", "--import-memory", "--export=__wasm_init_tls", "--export=__tls_size", "--export=__tls_align", "--export=__tls_base"]
        {
            println!("cargo:rustc-link-arg-bins={arg}");
        }
    }
}
