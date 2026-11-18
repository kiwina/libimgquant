#!/usr/bin/env bash
# Builds the npm package `libimgquant` into wasm/pkg (or the directory given as $1):
#
#   1. cargo build for wasm32-unknown-unknown (release, single-threaded library + png feature);
#   2. wasm-bindgen --target web: ES module glue and TypeScript types;
#   3. wasm-opt -O3, only if binaryen's wasm-opt is on PATH (set WASM_OPT=0 to skip it);
#   4. the hand-written files in npm/, LICENSE and NOTICE, and the licenses of the Rust
#      crates compiled into the .wasm (scripts/third-party-licenses.sh).
#
# Needs: rustup target add wasm32-unknown-unknown, jq, and the wasm-bindgen CLI of exactly
# the wasm-bindgen version in Cargo.lock (the CLI and the crate must match):
#   cargo install wasm-bindgen-cli --version <version> --locked
set -euo pipefail

cd "$(dirname "$0")"
out=${1:-pkg}

version=$(awk '$0 == "name = \"wasm-bindgen\"" { getline; gsub(/[^0-9.]/, ""); print; exit }' Cargo.lock)
found=$(wasm-bindgen --version 2>/dev/null | awk '{ print $2 }' || true)
if [ "$found" != "$version" ]; then
    echo "build.sh: needs wasm-bindgen $version (found: ${found:-none})" >&2
    echo "  cargo install wasm-bindgen-cli --version $version --locked" >&2
    exit 1
fi

# panic locations embed source paths: don't ship the builder's home directory
RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=~" \
    cargo build --release --locked --target wasm32-unknown-unknown
# target/, or $CARGO_TARGET_DIR
target_dir=$(cargo metadata --format-version 1 --no-deps --locked | jq -r .target_directory)

rm -rf "$out"
mkdir -p "$out"
wasm-bindgen --target web --out-dir "$out" --out-name libimgquant \
    "$target_dir/wasm32-unknown-unknown/release/libimgquant_wasm.wasm"
# Drop the declarations of wasm-bindgen's init functions: the package doesn't export them,
# and their DOM types (RequestInfo, Response) break type checks in Node-only projects.
sed -i '/^export type InitInput/,$d' "$out/libimgquant.d.ts"
if grep -qE 'WebAssembly|BufferSource|RequestInfo' "$out/libimgquant.d.ts"; then
    echo "build.sh: DOM types left in libimgquant.d.ts" >&2
    exit 1
fi

if [ "${WASM_OPT:-1}" != 0 ] && command -v wasm-opt > /dev/null; then
    # the features rustc enables by default for wasm32-unknown-unknown
    wasm-opt -O3 --enable-bulk-memory --enable-multivalue --enable-mutable-globals \
        --enable-nontrapping-float-to-int --enable-reference-types --enable-sign-ext \
        "$out/libimgquant_bg.wasm" -o "$out/libimgquant_bg.wasm"
else
    echo "build.sh: wasm-opt not used"
fi

# npm/package.json is marked private so that it can't be published by mistake; the built
# package can.
jq 'del(.private)' npm/package.json > "$out/package.json"
cp npm/index.js npm/node.js npm/index.d.ts npm/README.md "$out/"
cp ../LICENSE ../NOTICE "$out/"

# The license texts of every crate compiled into the .wasm (not build-time proc-macros).
../scripts/third-party-licenses.sh Cargo.toml wasm32-unknown-unknown "libimgquant's WebAssembly module" \
    > "$out/THIRD-PARTY-LICENSES.txt"

ls -l "$out"
echo "build.sh: $(wc -c < "$out/libimgquant_bg.wasm") bytes .wasm," \
    "$(gzip -9 < "$out/libimgquant_bg.wasm" | wc -c) gzipped"
