# Contributing

- **No GPL code:** libimgquant is BSD-licensed because it is derived only from libimagequant
  2.4.1. Never copy, port or paraphrase code from libimagequant 2.5 or later, imagequant 4
  or pngquant. Published papers and permissively licensed code are fine; say where an idea
  comes from.
- **Compat stays bit-identical:** `Profile::Compat` output must not change;
  `tests/golden.rs` freezes it. Measure changes to `Profile::Improved` with
  [`tools/iqeval`](tools/iqeval/README.md).
- **Pure Rust:** no C code, no build scripts and no C library dependencies. The library stays
  `#![forbid(unsafe_code)]`; unsafe code belongs only in `capi/`. New dependencies must pass
  `cargo deny check` (`deny.toml`: permissive licenses only).
- **MSRV 1.85** for libimgquant, libimgquant-capi and imgquant (tools/iqeval: 1.89).
- **Before a pull request:** `cargo fmt --all`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` and
  `cargo +1.85.0 test -p libimgquant -p libimgquant-capi -p imgquant`. The `wasm/` and
  `python/` packages are separate workspaces; CI (`.github/workflows/ci.yml`) shows how to
  test them.
