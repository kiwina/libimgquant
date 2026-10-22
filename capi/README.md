# libimgquant C API

[![crates.io](https://img.shields.io/crates/v/libimgquant-capi.svg)](https://crates.io/crates/libimgquant-capi)
[![docs.rs](https://img.shields.io/docsrs/libimgquant-capi)](https://docs.rs/libimgquant-capi)
[![License: BSD-2-Clause](https://img.shields.io/crates/l/libimgquant-capi.svg)](#license)
[![CI](https://github.com/kiwina/libimgquant/actions/workflows/ci.yml/badge.svg)](https://github.com/kiwina/libimgquant/actions/workflows/ci.yml)

A C API for [libimgquant](https://crates.io/crates/libimgquant), the pure-Rust, BSD-licensed color quantizer. The header
[`assets/libimgquant.h`](https://docs.rs/crate/libimgquant-capi/latest/source/assets/libimgquant.h) has the same functions, types and values as
libimagequant 2.4.x's `libimagequant.h`. C and C++ programs written for libimagequant 2.4.x
can switch to libimgquant by changing two things:

- the include: `libimgquant.h` instead of `libimagequant.h`;
- the link flag: `-limgquant` instead of `-limagequant`.

The C API is a set of Rust `extern "C"` functions in this crate. All unsafe code lives here;
the library crate stays `#![forbid(unsafe_code)]`.

## Building

```bash
cargo build --release -p libimgquant-capi
```

The files are written to `target/release/`:

| Platform | Shared library | Static library |
| --- | --- | --- |
| Linux, BSD | `libimgquant.so` | `libimgquant.a` |
| macOS | `libimgquant.dylib` | `libimgquant.a` |
| Windows (MSVC) | `imgquant.dll`, import library `imgquant.dll.lib` | `imgquant.lib` |
| Windows (GNU) | `imgquant.dll`, import library `libimgquant.dll.a` | `libimgquant.a` |

Build notes:

- **Threads:** quantization and remapping run in parallel by default. For a single-threaded
  library, add `--no-default-features`.
- **Size:** the workspace's release profile keeps line-number debug info. Strip the library
  before shipping it, for example with `strip libimgquant.so`.
- **SONAME:** cargo does not set one. Add one with
  `RUSTFLAGS="-C link-arg=-Wl,-soname,libimgquant.so.3"`, or install with cargo-c.
- **cargo-c:** see [Packaging](#packaging).

## Using it

```bash
# shared library
cc -I capi/assets example.c -L target/release -limgquant -o example

# static library (Linux; `cargo rustc --release -p libimgquant-capi --crate-type staticlib
# -- --print native-static-libs` prints the system libraries for other targets)
cc -I capi/assets example.c target/release/libimgquant.a -lpthread -ldl -lm -lrt -lutil -lgcc_s -o example
```

A minimal program:

```c
#include "libimgquant.h"

/* rgba: width * height pixels, 4 bytes each. On success, out holds width * height
   palette indices and *palette the colors they refer to. */
int quantize(unsigned char *rgba, int width, int height,
             unsigned char *out, liq_palette *palette)
{
    liq_attr *attr = liq_attr_create();
    liq_set_speed(attr, 3);
    liq_set_quality(attr, 65, 90);

    liq_image *image = liq_image_create_rgba(attr, rgba, width, height, 0);
    liq_result *result = image ? liq_quantize_image(attr, image) : NULL;
    int ok = 0;
    if (result) {
        liq_set_dithering_level(result, 1.0);
        /* out may also be rgba itself */
        if (liq_write_remapped_image(result, image, out, (size_t)width * height) == LIQ_OK) {
            *palette = *liq_get_palette(result);
            ok = 1;
        }
        liq_result_destroy(result);
    }
    liq_image_destroy(image);
    liq_attr_destroy(attr);
    return ok;
}
```

## Compatibility with libimagequant 2.4.x

### The same

- **Declarations:** every function, type and constant of the 2.4.1 header, with the same
  names, types and values, including the numeric `liq_error` codes.
- **Argument checks:**
  - functions return NULL, -1 or `LIQ_INVALID_POINTER` for a NULL handle or a handle of the
    wrong type;
  - out-of-range values return `LIQ_VALUE_OUT_OF_RANGE`;
  - images are limited to `INT_MAX` pixels;
  - a short output buffer returns `LIQ_BUFFER_TOO_SMALL`.
- **Using a destroyed handle:** this is undefined behavior. When the memory still shows that
  the handle was destroyed, the library prints a message and aborts, as 2.4.1 did.
- **Quantization:** `liq_quantize_image` returns NULL on failure, including when the minimum
  quality can't be reached.
- **Error messages:** errors are logged with the same messages.
- **Log flushing:** the flush callback runs when the log callback is replaced and when the
  `liq_attr` is destroyed.
- **Memory ownership** (`liq_image_set_memory_ownership`):
  - `LIQ_OWN_ROWS` only applies to `liq_image_create_rgba_rows` images;
  - for those, `LIQ_OWN_PIXELS` frees the row with the lowest address;
  - the memory is freed with the custom `free`, or with C `free` when there is no custom
    allocator.
- **Remapping in place:** remapping into the input bitmap works, as in the 2.4.1 README.
- **Handle lifetimes:** images and results stay valid after their `liq_attr` is destroyed.

### Different

- **Profiles:** the default is `LIQ_PROFILE_IMPROVED`, which uses libimgquant 3's algorithms.
  It gives better and smaller output, but different output. `liq_set_profile(attr,
  LIQ_PROFILE_COMPAT)` reproduces 2.4.1 apart from the fixes listed in the
  [libimgquant README](https://crates.io/crates/libimgquant).
  `LIQ_HAS_PROFILES` is defined when these functions exist.
- **Custom allocators:**
  - the library never calls the custom `malloc`; handles and internal buffers use Rust's
    allocator;
  - the custom `free` is used only for caller memory handed over with
    `liq_image_set_memory_ownership`.
- **`liq_get_palette`:**
  - it returns the same pointer for every call on one result, valid until
    `liq_result_destroy`;
  - its contents are updated by `liq_get_palette` and by each remapping;
  - in 2.4.1 the pointer could change after remapping, and an old one could dangle.
- **Callbacks:**
  - log and row callbacks may be called from worker threads, but never concurrently for the
    same `liq_attr` (log) or `liq_image` (rows);
  - a row callback may be called more than once for the same row;
  - callbacks must not call functions that change a handle that is being quantized or
    remapped (getters are fine).
- **Caller pixel memory:** it is read row by row when needed, so it must stay valid until the
  image is destroyed. 2.4.1 also read it lazily. Remapping into the same bitmap works, as in
  2.4.1; after that, don't quantize or remap that image again.
- **Bug fixes:**
  - `liq_set_dithering_level` validates the new value; 2.4.1 checked the previous one;
  - after `liq_set_output_gamma`, the palette uses the new gamma;
  - `liq_image_create_rgba` returns NULL when the image can't be created; 2.4.1 then wrote
    through a NULL pointer.
- **Version and extensions:**
  - `LIQ_VERSION` is 30000;
  - `liq_set_profile`/`liq_get_profile` choose between the version 3 algorithms and exact
    2.4.1 behavior (`LIQ_HAS_PROFILES` is defined); `liq_get_profile` returns an `int`, -1 for
    an invalid handle;
  - `liq_version()` is declared; 2.4.1 exported it without declaring it;
  - 2.4.1's undeclared helper symbols (`liq_crash_if_invalid_handle_pointer_given`,
    `liq_crash_if_invalid_pointer_given`, `liq_executing_user_callback`) are not exported.
- **Windows:** memory handed over without a custom allocator is freed with the C runtime that
  the DLL links. If the program uses another C runtime, give a custom allocator.

As in 2.4.1, one handle must not be used from two threads at the same time. Different handles
can be used from different threads.

## Packaging

The crate carries [cargo-c](https://crates.io/crates/cargo-c) metadata
(`[package.metadata.capi]`), so distributions can build and install it like a C library:

```bash
cargo install cargo-c
cargo cinstall --release -p libimgquant-capi --prefix=/usr --libdir=/usr/lib64 --destdir="$PKGDIR"
```

This installs:

| File | Notes |
| --- | --- |
| `libimgquant.h` | The hand-written header from `assets/` (cargo-c's `header.generation = false`), installed directly in the include directory, which the pkg-config `Cflags` point to. |
| `libimgquant.so.3.0.0`, with the links `libimgquant.so.3` and `libimgquant.so` | cargo-c sets the SONAME `libimgquant.so.3`. macOS and Windows get the platform's equivalents (`.dylib`; `imgquant.dll` with its import library). |
| `libimgquant.a` | The static library. |
| `libimgquant.pc` | pkg-config file: `pkg-config --cflags --libs libimgquant`. The `Libs.private` line lists the system libraries for static linking. |

Notes for packagers:

- **Source:** the `libimgquant-capi` crate on crates.io contains the header, the Rust sources
  and the tests. It depends on the `libimgquant` crate of the same version; `cargo vendor`
  fetches it for offline builds.
- **Threads:** `--no-default-features` builds the single-threaded variant without rayon.
- **Version:** the library version is the crate version (`LIQ_VERSION` 30000, `liq_version()`).
  The ABI is libimagequant 2.4.x's, but the library name differs (`imgquant`), so it can be
  installed next to libimagequant.
- **Tests:** `cargo test -p libimgquant-capi` exercises the C API through its exported
  functions and checks that the header declares exactly the exported symbols.

## License

BSD-2-Clause, plus the original permissive notice by Jef Poskanzer. See
[COPYRIGHT](https://docs.rs/crate/libimgquant-capi/latest/source/COPYRIGHT).
