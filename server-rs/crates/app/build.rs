//! Embed the dashboard static export into the binary so the packaged exe can
//! serve `/` with no source checkout on disk (the Windows/macOS artifact case).
//!
//! The dashboard is built (`MEM0_DASHBOARD_OUTPUT=export pnpm build`) before
//! cargo in both build_desktop_rs.sh and CI, so `server/dashboard/out` exists
//! and is copied here. On a fresh clone without it, a stub is written so
//! `cargo test` still compiles. `src/embedded-dashboard/` is gitignored; it is
//! a build artifact, never source.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let src = manifest.join("../../../server/dashboard/out");
    let dst = manifest.join("src/embedded-dashboard");

    println!("cargo:rerun-if-changed=src/embedded-dashboard/index.html");
    // Start from an empty copy: every dashboard build renames its chunk files, so copying
    // on top of the previous export leaves dead chunks (and dead page routes) embedded in
    // the binary forever.
    if dst.exists() {
        fs::remove_dir_all(&dst).expect("clear previous embedded dashboard");
    }
    if src.join("index.html").is_file() {
        println!("cargo:rerun-if-changed={}", src.display());
        copy_dir(&src, &dst);
    } else {
        fs::create_dir_all(&dst).expect("create stub dashboard dir");
        fs::write(
            dst.join("index.html"),
            "<!doctype html><title>mem0-desktop</title><p>dashboard not built: run `MEM0_DASHBOARD_OUTPUT=export pnpm build` in server/dashboard</p>",
        )
        .expect("write stub index.html");
        println!("cargo:warning=dashboard export missing; embedded a stub (build server/dashboard first for the real UI)");
    }
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("mkdir");
    for entry in fs::read_dir(src).expect("read dashboard dir") {
        let from = entry.expect("dir entry").path();
        let to = dst.join(from.file_name().unwrap());
        if from.is_dir() {
            copy_dir(&from, &to);
        } else {
            fs::copy(&from, &to).expect("copy asset");
        }
    }
}
