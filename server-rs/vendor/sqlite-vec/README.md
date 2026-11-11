# Vendored sqlite-vec Rust binding

Source: https://github.com/asg017/sqlite-vec @ 04d28bd21773981e2d266bbf6aa4efbd011eb4f6
(`bindings/rust/`), version 0.1.10-alpha.4.

Why vendored: the crates.io package `sqlite-vec 0.1.10-alpha.4` ships only `sqlite-vec.c`
and misses its sibling translation units (`sqlite-vec-ivf.c`, `sqlite-vec-diskann.c`, …),
so `cc` fails with `fatal error: sqlite-vec-diskann.c: No such file or directory`. The
upstream `bindings/rust/Makefile` copies those files in before building; vendoring does
the same, once, and keeps the build hermetic (no git fetch at build time).

Files copied per upstream Makefile:
- sqlite-vec.c + siblings (sqlite-vec-ivf.c, sqlite-vec-ivf-kmeans.c, sqlite-vec-diskann.c, sqlite-vec-rescore.c)
- sqlite-vec.h  (generated from sqlite-vec.h.tmpl with VERSION=0.1.10-alpha.4)
- sqlite3.h / sqlite3ext.h (from the SQLite 3.50.4 amalgamation)
- Cargo.toml (generated from Cargo.toml.tmpl), build.rs, src/
