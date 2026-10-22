#!/usr/bin/env bash
# Builds the npm package `libimgquant` into wasm/pkg (or the directory given as $1):
#
#   1. cargo build for wasm32-unknown-unknown (release, single-threaded library + png feature);
#   2. wasm-bindgen --target web: ES module glue and TypeScript types;
#   3. wasm-opt -O3, only if binaryen's wasm-opt is on PATH (set WASM_OPT=0 to skip it);
#   4. the hand-written files in npm/, the license, and the licenses of the Rust crates
#      compiled into the .wasm.
#
# Needs: rustup target add wasm32-unknown-unknown, and the wasm-bindgen CLI of exactly the
# wasm-bindgen version in Cargo.lock (the CLI and the crate must match):
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

cargo build --release --locked --target wasm32-unknown-unknown

rm -rf "$out"
mkdir -p "$out"
wasm-bindgen --target web --out-dir "$out" --out-name libimgquant \
    target/wasm32-unknown-unknown/release/libimgquant_wasm.wasm

if [ "${WASM_OPT:-1}" != 0 ] && command -v wasm-opt > /dev/null; then
    # the features rustc enables by default for wasm32-unknown-unknown
    wasm-opt -O3 --enable-bulk-memory --enable-multivalue --enable-mutable-globals \
        --enable-nontrapping-float-to-int --enable-reference-types --enable-sign-ext \
        "$out/libimgquant_bg.wasm" -o "$out/libimgquant_bg.wasm"
else
    echo "build.sh: wasm-opt not used"
fi

cp npm/package.json npm/index.js npm/node.js npm/index.d.ts npm/README.md "$out/"
cp ../COPYRIGHT "$out/LICENSE"

# The license texts of every crate compiled into the .wasm (not build-time proc-macros).
{
    echo "libimgquant's WebAssembly module contains these Rust crates besides libimgquant."
    echo "Their licenses follow; where a crate offers a choice that includes MIT, MIT is used."
    paths=$(cargo metadata --format-version 1 --locked --filter-platform wasm32-unknown-unknown |
        jq -r '.packages[] | "\(.name) v\(.version)\t\(.manifest_path | sub("/Cargo.toml$"; ""))"')
    cargo tree --locked -e normal,no-proc-macro --target wasm32-unknown-unknown --prefix none \
        -f '{p}|{l}' | sed 's/ (\*)$//' | sort -u | while IFS='|' read -r package license; do
        case $package in libimgquant\ * | libimgquant-wasm\ *) continue ;; esac
        dir=$(printf '%s\n' "$paths" | awk -F '\t' -v p="$package" '$1 == p { print $2; exit }')
        printf '\n%s\n%s (%s)\n%s\n' "================================================================================" \
            "$package" "$license" "================================================================================"
        files=$(find "$dir" -maxdepth 1 -type f \( -iname 'LICENSE*' -o -iname 'COPYING*' -o -iname 'NOTICE*' \) | sort)
        # where MIT is one of the choices ("MIT OR Apache-2.0"), it is the one taken
        mit=$(printf '%s\n' "$files" | grep -i 'mit' || true)
        if [[ $license != *AND* && $license =~ (^|[^A-Za-z-])MIT([^A-Za-z-]|$) && -n $mit ]]; then
            files=$mit
        fi
        if [ -z "$files" ]; then
            echo "(no license file in the package; license: $license)"
        fi
        for f in $files; do
            printf '\n--- %s ---\n\n' "$(basename "$f")"
            cat "$f"
        done
    done
} > "$out/THIRD-PARTY-LICENSES.txt"

ls -l "$out"
echo "build.sh: $(wc -c < "$out/libimgquant_bg.wasm") bytes .wasm," \
    "$(gzip -9 < "$out/libimgquant_bg.wasm" | wc -c) gzipped"
