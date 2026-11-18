#!/usr/bin/env bash
# Prints a THIRD-PARTY-LICENSES.txt: the license texts of the Rust standard library and of
# every crate compiled into a package (normal dependencies, not build-time proc-macros),
# without the packages of this repository. Where a crate offers a choice that includes MIT,
# only its MIT license is printed.
#
#   scripts/third-party-licenses.sh <Cargo.toml> <target triple, or "all"> <name of the build>
#
# Used by wasm/build.sh, by release.yml for the imgquant archives, and for the committed
# python/THIRD-PARTY-LICENSES.txt (the Python wheels; CI checks that it is up to date):
#
#   scripts/third-party-licenses.sh python/Cargo.toml all "libimgquant's Python extension module" \
#       > python/THIRD-PARTY-LICENSES.txt
#
# Needs cargo, rustc and jq.
set -euo pipefail

manifest=$1
target=$2
name=$3
rule="================================================================================"

printf '%s contains the Rust standard library and these Rust crates besides libimgquant. %s\n' \
    "$name" "Their licenses follow; where a crate offers a choice that includes MIT, MIT is used." |
    fold -s -w 88 | sed 's/ *$//'
printf '\n%s\n%s\n%s\n\n' "$rule" "Rust standard library: core, alloc, std (MIT OR Apache-2.0)" "$rule"
sed 's/<year> <copyright holders>/The Rust Project Contributors/' \
    "$(rustc --print sysroot)/share/doc/rust/licenses/MIT.txt"

# "name vX.Y.Z<tab>directory" of every crates.io package
paths=$(cargo metadata --manifest-path "$manifest" --format-version 1 --locked |
    jq -r '.packages[] | select(.source != null) | "\(.name) v\(.version)\t\(.manifest_path | sub("/Cargo.toml$"; ""))"')
cargo tree --manifest-path "$manifest" --locked -e normal,no-proc-macro --target "$target" --prefix none \
    -f '{p}|{l}' | sed 's/ (\*)$//' | sort -u | while IFS='|' read -r package license; do
    dir=$(printf '%s\n' "$paths" | awk -F '\t' -v p="$package" '$1 == p { print $2; exit }')
    [ -n "$dir" ] || continue
    printf '\n%s\n%s (%s)\n%s\n' "$rule" "$package" "$license" "$rule"
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
