//! Build the browser bundle.
//!
//! esbuild turns `js/app.js` into a code-split ES module tree under `OUT_DIR/assets`
//! and `js/app.css` into `OUT_DIR/nashcode.css`; the server embeds the whole assets
//! directory plus the stylesheet. Splitting matters because @pierre/diffs pulls shiki,
//! whose grammars and themes sit behind `import()` — without it esbuild inlines every
//! language into the entry point. `cargo build` on a fresh clone produces a runnable
//! server, provided node+npm are installed: if `node_modules` is missing, this script
//! runs `npm ci` first.

use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

fn main() {
    println!("cargo:rerun-if-changed=js/app.js");
    println!("cargo:rerun-if-changed=js/app.css");
    println!("cargo:rerun-if-changed=package.json");
    println!("cargo:rerun-if-changed=package-lock.json");
    println!("cargo:rerun-if-changed=vendor");

    let out_dir = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let root = Path::new(&manifest);

    // Chunk names carry a content hash, so a stale chunk from a previous build would
    // otherwise linger in the directory and get embedded forever.
    let assets = Path::new(&out_dir).join("assets");
    if assets.exists() {
        std::fs::remove_dir_all(&assets).expect("clear assets dir");
    }
    std::fs::create_dir_all(&assets).expect("create assets dir");
    let entry_js = assets.join("nashcode.js");
    let app_css = format!("{out_dir}/nashcode.css");

    // A Rust-only compile without node: empty bundles, clearly marked.
    println!("cargo:rerun-if-env-changed=NASHCODE_SKIP_ASSET_BUILD");
    if std::env::var("NASHCODE_SKIP_ASSET_BUILD").is_ok_and(|v| !v.trim().is_empty()) {
        let stub = "/* NASHCODE_SKIP_ASSET_BUILD was set: assets not built */\n";
        std::fs::write(&entry_js, stub).expect("write stub js");
        std::fs::write(&app_css, stub).expect("write stub css");
        println!("cargo:rustc-env=NASHCODE_ASSET_HASH=skipped");
        return;
    }

    if !root.join("node_modules").exists() {
        run(Command::new("npm").arg("ci").current_dir(root), "npm ci");
    }

    let esbuild = root.join("node_modules/.bin/esbuild");
    if !esbuild.exists() {
        panic!(
            "node_modules/.bin/esbuild is missing after npm ci; \
             delete node_modules and rebuild"
        );
    }

    run(
        Command::new(&esbuild)
            .current_dir(root)
            .arg("js/app.js")
            .arg("--bundle")
            .arg("--format=esm")
            .arg("--splitting")
            .arg("--minify")
            .arg("--target=es2022")
            // Flat names inside one directory: chunks import each other as
            // `./chunk-*.js`, which resolves against `/assets/` in the browser.
            .arg("--entry-names=nashcode")
            .arg("--chunk-names=chunk-[hash]")
            .arg("--asset-names=asset-[hash]")
            .arg(format!("--outdir={}", assets.display())),
        "esbuild js",
    );

    run(
        Command::new(&esbuild)
            .current_dir(root)
            .arg("js/app.css")
            .arg("--bundle")
            .arg("--minify")
            // Browsers all take woff2; the legacy fallback formats in @font-face src
            // lists (woff/ttf/svg) are dropped so they don't bloat the bundle.
            .arg("--loader:.svg=empty")
            .arg("--loader:.woff=empty")
            .arg("--loader:.ttf=empty")
            .arg("--loader:.woff2=dataurl")
            .arg(format!("--outfile={app_css}")),
        "esbuild css",
    );

    // A short content hash busts browser caches when either bundle changes. Hashing the
    // entry is enough: a changed chunk changes the import name inside the entry.
    let mut hasher = Sha256::new();
    hasher.update(std::fs::read(&entry_js).expect("bundle exists"));
    hasher.update(std::fs::read(&app_css).expect("bundle exists"));
    let digest = hasher.finalize();
    let hash: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    println!("cargo:rustc-env=NASHCODE_ASSET_HASH={hash}");
}

fn run(command: &mut Command, what: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("cannot run {what}: {error}. Is node installed?"));
    if !status.success() {
        panic!("{what} failed with {status}");
    }
}
