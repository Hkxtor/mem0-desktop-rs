//! Dashboard assets embedded into the binary at compile time (see build.rs).
//! Used as the fallback when no on-disk export exists — i.e. the packaged
//! desktop exe, which has no source checkout next to it.

/// The dashboard export, copied to `crates/app/src/embedded-dashboard/` by
/// build.rs (gitignored build artifact).
pub static DASHBOARD: ::include_dir::Dir<'static> =
    ::include_dir::include_dir!("$CARGO_MANIFEST_DIR/src/embedded-dashboard");

/// Look up `path` (e.g. "index.html", "_next/static/x.js") in the embedded export.
pub fn get(path: &str) -> Option<&'static [u8]> {
    DASHBOARD.get_file(path).map(|f| f.contents())
}

/// MIME type by extension, matching what ServeDir would send for the same file.
pub fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("txt") => "text/x-component; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("webp") => "image/webp",
        Some("map") => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_export_has_index() {
        assert!(
            super::get("index.html").is_some(),
            "embedded dashboard must contain index.html (build.rs stub or real export)"
        );
    }
}
