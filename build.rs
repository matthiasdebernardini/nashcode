//! Build the browser bundle.
//!
//! esbuild turns `js/app.js` + `js/app.css` (Primer + @pierre/diffs) into two files in
//! `OUT_DIR`, which the server embeds with `include_bytes!`. `cargo build` on a fresh
//! clone therefore produces a runnable server, provided node+npm are installed: if
//! `node_modules` is missing, this script runs `npm ci` first.

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
            .arg("--minify")
            .arg("--target=es2022")
            .arg(format!("--outfile={out_dir}/nashgit.js")),
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
            .arg(format!("--outfile={out_dir}/nashgit.css")),
        "esbuild css",
    );

    // A short content hash busts browser caches when either bundle changes.
    let mut hasher = Sha256::new();
    hasher.update(std::fs::read(format!("{out_dir}/nashgit.js")).expect("bundle exists"));
    hasher.update(std::fs::read(format!("{out_dir}/nashgit.css")).expect("bundle exists"));
    let digest = hasher.finalize();
    let hash: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    println!("cargo:rustc-env=NASHGIT_ASSET_HASH={hash}");
}

fn run(command: &mut Command, what: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("cannot run {what}: {error}. Is node installed?"));
    if !status.success() {
        panic!("{what} failed with {status}");
    }
}
