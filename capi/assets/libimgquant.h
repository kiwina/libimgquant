/*
 * libimgquant.h - C API of libimgquant 3, a pure-Rust color quantizer.
 *
 * This is libimgquant's header. It is compatible with the libimagequant 2.4.x
 * API (same names, types and values): programs written for libimagequant 2.4.x
 * build against it by including this header instead of libimagequant.h and
 * linking with -limgquant. It is based on the libimagequant 2.4.1 header
 * (http://pngquant.org); declarations marked "libimgquant extension" are new.
 *
 * Copyright (C) 2009-2014 by Kornel Lesinski.
 * Copyright (C) 2026 libimgquant contributors.
 * All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions are met:
 *
 * 1. Redistributions of source code must retain the above copyright notice,
 *    this list of conditions and the following disclaimer.
 *
 * 2. Redistributions in binary form must reproduce the above copyright notice,
 *    this list of conditions and the following disclaimer in the documentation
 *    and/or other materials provided with the distribution.
 *
 * THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
 * AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
 * DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 * SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 * CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
 * OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
 * OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
 */

#ifndef LIBIMAGEQUANT_H
#define LIBIMAGEQUANT_H

#ifndef LIQ_EXPORT
#define LIQ_EXPORT extern
#endif

#define LIQ_VERSION 30000
#define LIQ_VERSION_STRING "3.0.0"

#ifndef LIQ_PRIVATE
#if defined(__GNUC__) || defined (__llvm__)
#define LIQ_PRIVATE __attribute__((visibility("hidden")))
#else
#define LIQ_PRIVATE
#endif
#endif

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct liq_attr liq_attr;
typedef struct liq_image liq_image;
typedef struct liq_result liq_result;

typedef struct liq_color {
    unsigned char r, g, b, a;
} liq_color;

typedef struct liq_palette {
    unsigned int count;
    liq_color entries[256];
} liq_palette;

typedef enum liq_error {
    LIQ_OK = 0,
    LIQ_QUALITY_TOO_LOW = 99,
    LIQ_VALUE_OUT_OF_RANGE = 100,
    LIQ_OUT_OF_MEMORY,
    LIQ_NOT_READY,
    LIQ_BITMAP_NOT_AVAILABLE,
    LIQ_BUFFER_TOO_SMALL,
    LIQ_INVALID_POINTER
} liq_error;

enum liq_ownership {LIQ_OWN_ROWS=4, LIQ_OWN_PIXELS=8};

/*
 * Custom allocators: give both functions or neither. libimgquant allocates its
 * own memory itself and never calls `malloc`; `free` is used only for memory
 * handed over with liq_image_set_memory_ownership().
 */
LIQ_EXPORT liq_attr* liq_attr_create(void);
LIQ_EXPORT liq_attr* liq_attr_create_with_allocator(void* (*malloc)(size_t), void (*free)(void*));
LIQ_EXPORT liq_attr* liq_attr_copy(liq_attr *orig);
LIQ_EXPORT void liq_attr_destroy(liq_attr *attr);

LIQ_EXPORT liq_error liq_set_max_colors(liq_attr* attr, int colors);
LIQ_EXPORT int liq_get_max_colors(const liq_attr* attr);
LIQ_EXPORT liq_error liq_set_speed(liq_attr* attr, int speed);
LIQ_EXPORT int liq_get_speed(const liq_attr* attr);
LIQ_EXPORT liq_error liq_set_min_opacity(liq_attr* attr, int min);
LIQ_EXPORT int liq_get_min_opacity(const liq_attr* attr);
LIQ_EXPORT liq_error liq_set_min_posterization(liq_attr* attr, int bits);
LIQ_EXPORT int liq_get_min_posterization(const liq_attr* attr);
LIQ_EXPORT liq_error liq_set_quality(liq_attr* attr, int minimum, int maximum);
LIQ_EXPORT int liq_get_min_quality(const liq_attr* attr);
LIQ_EXPORT int liq_get_max_quality(const liq_attr* attr);
LIQ_EXPORT void liq_set_last_index_transparent(liq_attr* attr, int is_last);

/*
 * Log callbacks receive the liq_attr they were set on (for a liq_attr_copy()
 * copy, the copy). They may be called from a worker thread, but never
 * concurrently for the same liq_attr, and must not modify that liq_attr.
 */
typedef void liq_log_callback_function(const liq_attr*, const char *message, void* user_info);
typedef void liq_log_flush_callback_function(const liq_attr*, void* user_info);
LIQ_EXPORT void liq_set_log_callback(liq_attr*, liq_log_callback_function*, void* user_info);
LIQ_EXPORT void liq_set_log_flush_callback(liq_attr*, liq_log_flush_callback_function*, void* user_info);

/*
 * The pixel memory must stay valid until the image is destroyed. As in 2.4.1, a
 * remapped image may be written into the same bitmap; the image then must not be
 * quantized or remapped again.
 */
LIQ_EXPORT liq_image *liq_image_create_rgba_rows(liq_attr *attr, void* rows[], int width, int height, double gamma);
LIQ_EXPORT liq_image *liq_image_create_rgba(liq_attr *attr, void* bitmap, int width, int height, double gamma);

/*
 * The row callback may be called from a worker thread, and several times for
 * the same row, but never concurrently for the same image. Callbacks must not
 * call functions that change a handle which is being quantized or remapped
 * (getters are fine).
 */
typedef void liq_image_get_rgba_row_callback(liq_color row_out[], int row, int width, void* user_info);
LIQ_EXPORT liq_image *liq_image_create_custom(liq_attr *attr, liq_image_get_rgba_row_callback *row_callback, void* user_info, int width, int height, double gamma);

LIQ_EXPORT liq_error liq_image_set_memory_ownership(liq_image *image, int ownership_flags);
LIQ_EXPORT liq_error liq_image_add_fixed_color(liq_image *img, liq_color color);
LIQ_EXPORT int liq_image_get_width(const liq_image *img);
LIQ_EXPORT int liq_image_get_height(const liq_image *img);
LIQ_EXPORT void liq_image_destroy(liq_image *img);

LIQ_EXPORT liq_result *liq_quantize_image(liq_attr *options, liq_image *input_image);

LIQ_EXPORT liq_error liq_set_dithering_level(liq_result *res, float dither_level);
LIQ_EXPORT liq_error liq_set_output_gamma(liq_result* res, double gamma);
LIQ_EXPORT double liq_get_output_gamma(const liq_result *result);

/*
 * The returned pointer is the same for every call on one result and stays
 * valid until liq_result_destroy(). Its contents are updated by this function
 * and by the remapping functions.
 */
LIQ_EXPORT const liq_palette *liq_get_palette(liq_result *result);

/* The output may be the same memory as the image's input bitmap. */
LIQ_EXPORT liq_error liq_write_remapped_image(liq_result *result, liq_image *input_image, void *buffer, size_t buffer_size);
LIQ_EXPORT liq_error liq_write_remapped_image_rows(liq_result *result, liq_image *input_image, unsigned char **row_pointers);

/* Both return -1 when the error has not been measured yet (it is after remapping). */
LIQ_EXPORT double liq_get_quantization_error(liq_result *result);
LIQ_EXPORT int liq_get_quantization_quality(liq_result *result);

LIQ_EXPORT void liq_result_destroy(liq_result *);

/* libimgquant extension: returns LIQ_VERSION of the linked library. */
LIQ_EXPORT int liq_version(void);

/*
 * libimgquant extension: algorithm profiles.
 *
 * LIQ_PROFILE_IMPROVED (the default) uses libimgquant 3's algorithms.
 * LIQ_PROFILE_COMPAT reproduces libimagequant 2.4.1 step by step (apart from
 * its bug fixes). Changing the profile keeps all settings. liq_get_profile()
 * returns a liq_profile value, or -1 for an invalid handle.
 */
#define LIQ_HAS_PROFILES 1
typedef enum liq_profile {
    LIQ_PROFILE_COMPAT = 0,
    LIQ_PROFILE_IMPROVED = 1
} liq_profile;

LIQ_EXPORT liq_error liq_set_profile(liq_attr* attr, liq_profile profile);
LIQ_EXPORT int liq_get_profile(const liq_attr* attr);

#ifdef __cplusplus
}
#endif

#endif
