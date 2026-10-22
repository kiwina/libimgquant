//! C API of [`libimgquant`], declared in `include/libimgquant.h`.
//!
//! The functions and types follow libimagequant 2.4.1's `libimagequant.h`, so C and C++
//! programs written for libimagequant 2.4.x can use libimgquant by changing the include and
//! link flags. See the crate README for the differences.
//!
//! All functions are `extern "C"` and meant to be called from C. From Rust, use the
//! [`libimgquant`] crate directly.
//!
//! # Handles
//!
//! `liq_attr`, `liq_image` and `liq_result` are heap objects owned by the caller. Each starts
//! with a type tag, so passing a handle of the wrong type (or NULL) returns the documented
//! failure value instead of crashing, and using a handle after destroying it aborts the process
//! when the memory still holds the tag (like libimagequant 2.4.1).
//!
//! # Caller memory
//!
//! Pixel memory given to `liq_image_create_rgba` and `liq_image_create_rgba_rows` is only read
//! through raw pointers, a row at a time, and remapping writes into a temporary buffer that is
//! copied to the caller's buffer afterwards. So no Rust reference to caller memory outlives a
//! call, and the output buffer may be the input bitmap.

#![allow(non_camel_case_types)]
#![warn(missing_docs)]

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::io::Write as _;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, ThreadId};

use libimgquant::{Attributes, Error, Image, Profile, QuantizationResult, RGBA};

/// `LIQ_VERSION`: major * 10000 + minor * 100 + patch.
pub const LIQ_VERSION: c_int = 30000;
/// `LIQ_VERSION_STRING`.
pub const LIQ_VERSION_STRING: &CStr = c"3.0.0";

/// `LIQ_OWN_ROWS` for [`liq_image_set_memory_ownership`].
pub const LIQ_OWN_ROWS: c_int = 4;
/// `LIQ_OWN_PIXELS` for [`liq_image_set_memory_ownership`].
pub const LIQ_OWN_PIXELS: c_int = 8;

/// `LIQ_PROFILE_COMPAT` (libimgquant extension): libimagequant 2.4.1's algorithms.
pub const LIQ_PROFILE_COMPAT: c_int = 0;
/// `LIQ_PROFILE_IMPROVED` (libimgquant extension, the default): libimgquant 3's algorithms.
pub const LIQ_PROFILE_IMPROVED: c_int = 1;

/// `liq_color`: an 8-bit RGBA color.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct liq_color {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha (0 is transparent).
    pub a: u8,
}

/// `liq_palette`: `count` colors in `entries`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct liq_palette {
    /// Number of used entries.
    pub count: c_uint,
    /// The colors; entries from `count` on are zero.
    pub entries: [liq_color; 256],
}

/// `liq_error`, with the numeric values of libimagequant 2.4.1.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum liq_error {
    /// Success.
    LIQ_OK = 0,
    /// The image could not be quantized within the minimum quality.
    LIQ_QUALITY_TOO_LOW = 99,
    /// An argument was outside of its range.
    LIQ_VALUE_OUT_OF_RANGE = 100,
    /// Memory could not be allocated.
    LIQ_OUT_OF_MEMORY = 101,
    /// Not used.
    LIQ_NOT_READY = 102,
    /// Not used.
    LIQ_BITMAP_NOT_AVAILABLE = 103,
    /// A buffer is smaller than the image.
    LIQ_BUFFER_TOO_SMALL = 104,
    /// A NULL pointer, or a handle of the wrong type.
    LIQ_INVALID_POINTER = 105,
}

use liq_error::{LIQ_BUFFER_TOO_SMALL, LIQ_INVALID_POINTER, LIQ_OK, LIQ_OUT_OF_MEMORY, LIQ_VALUE_OUT_OF_RANGE};

fn error_code(err: Error) -> liq_error {
    match err {
        Error::QualityTooLow => liq_error::LIQ_QUALITY_TOO_LOW,
        Error::ValueOutOfRange => LIQ_VALUE_OUT_OF_RANGE,
        Error::OutOfMemory => LIQ_OUT_OF_MEMORY,
        Error::BufferTooSmall => LIQ_BUFFER_TOO_SMALL,
        _ => LIQ_VALUE_OUT_OF_RANGE,
    }
}

fn result_code(res: Result<(), Error>) -> liq_error {
    res.map_or_else(error_code, |()| LIQ_OK)
}

/// `liq_log_callback_function`.
pub type liq_log_callback_function = unsafe extern "C" fn(*const liq_attr, *const c_char, *mut c_void);
/// `liq_log_flush_callback_function`.
pub type liq_log_flush_callback_function = unsafe extern "C" fn(*const liq_attr, *mut c_void);
/// `liq_image_get_rgba_row_callback`: fills `width` pixels of row `row`.
pub type liq_image_get_rgba_row_callback = unsafe extern "C" fn(*mut liq_color, c_int, c_int, *mut c_void);
/// Custom `malloc` for [`liq_attr_create_with_allocator`].
pub type liq_malloc_function = unsafe extern "C" fn(usize) -> *mut c_void;
/// Custom `free` for [`liq_attr_create_with_allocator`].
pub type liq_free_function = unsafe extern "C" fn(*mut c_void);

unsafe extern "C" {
    /// The C library's `free`, for memory handed over without a custom allocator.
    #[link_name = "free"]
    fn libc_free(ptr: *mut c_void);
}

// ---------------------------------------------------------------------------------------------
// Handles

const MAGIC_ATTR: u64 = u64::from_le_bytes(*b"liq_attr");
const MAGIC_IMAGE: u64 = u64::from_le_bytes(*b"liq_imag");
const MAGIC_RESULT: u64 = u64::from_le_bytes(*b"liq_resu");
const MAGIC_FREED: u64 = u64::from_le_bytes(*b"liq_free");

/// Start of every handle. Common allocators keep free-list links in the first two words of a
/// freed block, so the tag comes after them, where it survives a free more often and a later
/// use of the destroyed handle can be detected.
#[repr(C)]
struct Header {
    _allocator_scratch: [usize; 2],
    magic: u64,
}

impl Header {
    const fn new(magic: u64) -> Self {
        Self {
            _allocator_scratch: [0; 2],
            magic,
        }
    }
}

trait Handle {
    const MAGIC: u64;
    const NAME: &'static str;
}

/// Checks the tag of a handle: false for NULL or another handle type, and aborts if the handle
/// was destroyed (as far as the memory still says so).
///
/// # Safety
/// `ptr` must be NULL or point to readable memory at least as large as a [`Header`].
unsafe fn is_valid<T: Handle>(ptr: *const T) -> bool {
    if ptr.is_null() {
        return false;
    }
    // SAFETY: the caller guarantees the header is readable; no reference is created.
    let magic = unsafe { ptr::addr_of!((*ptr.cast::<Header>()).magic).read() };
    if magic == MAGIC_FREED {
        // Not error handling: the program used a destroyed handle and cannot continue safely.
        let _ = writeln!(std::io::stderr(), "{} used after being freed", T::NAME);
        std::process::abort();
    }
    magic == T::MAGIC
}

/// Marks a handle as destroyed, just before its memory is freed.
///
/// # Safety
/// `ptr` must be a valid handle.
unsafe fn mark_freed<T: Handle>(ptr: *mut T) {
    // SAFETY: the caller guarantees the handle is valid; no reference is created.
    unsafe { ptr::addr_of_mut!((*ptr.cast::<Header>()).magic).write(MAGIC_FREED) };
}

/// A raw pointer captured by callbacks that the library may call from worker threads.
/// The C API's contract makes the pointed-to data usable from any thread.
struct SendPtr<T>(*mut T);

impl<T> SendPtr<T> {
    /// Accessed through a method, so closures capture the whole (`Send`) wrapper.
    fn get(&self) -> *mut T {
        self.0
    }
}

// SAFETY: see the type's documentation; the pointers are only dereferenced in C callbacks
// and in row copies that the C API allows from any thread.
unsafe impl<T> Send for SendPtr<T> {}
// SAFETY: as above.
unsafe impl<T> Sync for SendPtr<T> {}

/// Runs user callbacks one at a time. Re-entrant, so a callback that calls back into the
/// library on the same thread (and causes another callback) does not deadlock.
#[derive(Default)]
struct CallbackLock {
    state: Mutex<(Option<ThreadId>, usize)>,
    released: Condvar,
}

impl CallbackLock {
    fn run<R>(&self, f: impl FnOnce() -> R) -> R {
        let me = thread::current().id();
        {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            while state.0.is_some_and(|owner| owner != me) {
                state = self.released.wait(state).unwrap_or_else(PoisonError::into_inner);
            }
            state.0 = Some(me);
            state.1 += 1;
        }
        let result = f();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.1 -= 1;
        if state.1 == 0 {
            state.0 = None;
            self.released.notify_one();
        }
        result
    }
}

/// `liq_attr`: quantization settings. Opaque in C.
#[repr(C)]
pub struct liq_attr {
    header: Header,
    attr: Attributes,
    malloc: Option<liq_malloc_function>,
    free: Option<liq_free_function>,
    log: Option<(liq_log_callback_function, *mut c_void)>,
    log_flush: Option<(liq_log_flush_callback_function, *mut c_void)>,
    /// Serializes this attr's log and flush callbacks.
    callback_lock: Arc<CallbackLock>,
}

impl Handle for liq_attr {
    const MAGIC: u64 = MAGIC_ATTR;
    const NAME: &'static str = "liq_attr";
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImageSource {
    Bitmap,
    Rows,
    Custom,
}

/// Where an image's pixels come from, for `liq_image_set_memory_ownership`.
struct Origin {
    source: ImageSource,
    /// The row pointer array of `liq_image_create_rgba_rows`.
    rows_array: *mut c_void,
    /// The bitmap, or the row with the lowest address.
    pixels: *mut c_void,
}

/// `liq_image`: an image to quantize or remap. Opaque in C.
#[repr(C)]
pub struct liq_image {
    header: Header,
    /// Copies of the image size, readable while `image` is borrowed (e.g. from a callback).
    width: c_int,
    height: c_int,
    image: Image<'static>,
    origin: Origin,
    /// The custom `free` of the attr the image was created with; `None` means C `free`.
    free: Option<liq_free_function>,
    own_rows: bool,
    own_pixels: bool,
}

impl Handle for liq_image {
    const MAGIC: u64 = MAGIC_IMAGE;
    const NAME: &'static str = "liq_image";
}

/// `liq_result`: a palette and remapping settings. Opaque in C.
#[repr(C)]
pub struct liq_result {
    header: Header,
    result: QuantizationResult,
    /// Returned by `liq_get_palette`; allocated separately so that its address never changes.
    palette: NonNull<liq_palette>,
}

impl Handle for liq_result {
    const MAGIC: u64 = MAGIC_RESULT;
    const NAME: &'static str = "liq_result";
}

// ---------------------------------------------------------------------------------------------
// Attributes

fn message_cstring(message: &str) -> CString {
    CString::new(message.replace('\0', " ")).unwrap_or_default()
}

/// Calls the log callback directly, for errors detected by this crate.
///
/// # Safety
/// `attr` must be a valid `liq_attr` that is not mutably borrowed.
unsafe fn log_error(attr: *const liq_attr, message: &str) {
    // SAFETY: valid handle; the fields are copied out without borrowing the handle.
    let (log, lock) = unsafe { ((*attr).log, &(*attr).callback_lock) };
    if let Some((callback, user_info)) = log {
        let message = message_cstring(&format!("  error: {message}"));
        // SAFETY: the C callback contract.
        lock.run(|| unsafe { callback(attr, message.as_ptr(), user_info) });
    }
}

/// Calls the log flush callback, as libimagequant 2.4.1 did before changing the log callback
/// and before destroying the attr.
///
/// # Safety
/// `attr` must be a valid `liq_attr` that is not mutably borrowed.
unsafe fn flush_log(attr: *const liq_attr) {
    // SAFETY: valid handle; the fields are copied out without borrowing the handle.
    let (flush, lock) = unsafe { ((*attr).log_flush, &(*attr).callback_lock) };
    if let Some((callback, user_info)) = flush {
        // SAFETY: the C callback contract.
        lock.run(|| unsafe { callback(attr, user_info) });
    }
}

/// Points the library's log callback at the attr's C callback, passing `attr` itself.
///
/// The flush callback is not given to the library: it is called by [`flush_log`] at the two
/// points where libimagequant 2.4.1 called it.
///
/// # Safety
/// `attr` must be a valid `liq_attr` with no other borrows.
unsafe fn install_log_callback(attr: *mut liq_attr) {
    // SAFETY: valid handle, exclusively accessed.
    let handle = unsafe { &mut *attr };
    match handle.log {
        None => handle.attr.clear_log_callback(),
        Some((callback, user_info)) => {
            let owner = SendPtr(attr);
            let user_info = SendPtr(user_info);
            let lock = Arc::clone(&handle.callback_lock);
            handle.attr.set_log_callback(move |_, message| {
                let message = message_cstring(message);
                // SAFETY: the C callback contract; `owner` is the handle that owns this closure.
                lock.run(|| unsafe { callback(owner.get(), message.as_ptr(), user_info.get()) });
            });
        }
    }
}

fn new_attr_handle(
    attr: Attributes,
    malloc: Option<liq_malloc_function>,
    free: Option<liq_free_function>,
    log: Option<(liq_log_callback_function, *mut c_void)>,
    log_flush: Option<(liq_log_flush_callback_function, *mut c_void)>,
) -> *mut liq_attr {
    let ptr = Box::into_raw(Box::new(liq_attr {
        header: Header::new(MAGIC_ATTR),
        attr,
        malloc,
        free,
        log,
        log_flush,
        callback_lock: Arc::default(),
    }));
    // SAFETY: freshly allocated, not shared yet.
    unsafe { install_log_callback(ptr) };
    ptr
}

/// Creates settings with the defaults: 256 colors, speed 3, no quality limit, the
/// [`LIQ_PROFILE_IMPROVED`] profile.
#[unsafe(no_mangle)]
pub extern "C" fn liq_attr_create() -> *mut liq_attr {
    new_attr_handle(Attributes::new(), None, None, None, None)
}

/// Like [`liq_attr_create`], with a custom allocator: both functions or neither (otherwise NULL).
///
/// libimgquant allocates its own memory itself and never calls `malloc`; `free` is used for
/// memory handed over with [`liq_image_set_memory_ownership`].
///
/// # Safety
/// The functions must behave like C `malloc` and `free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_attr_create_with_allocator(
    malloc: Option<liq_malloc_function>,
    free: Option<liq_free_function>,
) -> *mut liq_attr {
    if malloc.is_some() != free.is_some() {
        return ptr::null_mut(); // either specify both or none
    }
    new_attr_handle(Attributes::new(), malloc, free, None, None)
}

/// Copies settings, including the allocator and the callbacks. The callbacks of the copy
/// receive the copy. NULL if `orig` is not a valid `liq_attr`.
///
/// # Safety
/// `orig` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_attr_copy(orig: *mut liq_attr) -> *mut liq_attr {
    // SAFETY: caller contract.
    if !unsafe { is_valid(orig) } {
        return ptr::null_mut();
    }
    // SAFETY: valid handle.
    let orig = unsafe { &*orig };
    new_attr_handle(orig.attr.clone(), orig.malloc, orig.free, orig.log, orig.log_flush)
}

/// Calls the log flush callback and frees the settings. Images and results created with them
/// remain valid.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library, not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_attr_destroy(attr: *mut liq_attr) {
    // SAFETY: caller contract.
    if !unsafe { is_valid(attr) } {
        return;
    }
    // SAFETY: valid handle, owned by the caller and given up here.
    unsafe {
        flush_log(attr);
        mark_freed(attr);
        drop(Box::from_raw(attr));
    }
}

/// Returns the settings of a valid handle, or `None`.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library, not mutably borrowed.
unsafe fn attr_ref<'a>(attr: *const liq_attr) -> Option<&'a Attributes> {
    // SAFETY: caller contract.
    unsafe { is_valid(attr).then(|| &(*attr).attr) }
}

/// Returns the settings of a valid handle mutably, or `None`.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library, not borrowed.
unsafe fn attr_mut<'a>(attr: *mut liq_attr) -> Option<&'a mut Attributes> {
    // SAFETY: caller contract.
    unsafe { is_valid(attr).then(|| &mut (*attr).attr) }
}

/// Maximum palette size, 2-256 (default 256).
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_set_max_colors(attr: *mut liq_attr, colors: c_int) -> liq_error {
    // SAFETY: caller contract.
    let Some(attr) = (unsafe { attr_mut(attr) }) else {
        return LIQ_INVALID_POINTER;
    };
    match u32::try_from(colors) {
        Ok(colors) => result_code(attr.set_max_colors(colors)),
        Err(_) => LIQ_VALUE_OUT_OF_RANGE,
    }
}

/// Maximum palette size, or -1 for an invalid handle.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_get_max_colors(attr: *const liq_attr) -> c_int {
    // SAFETY: caller contract.
    unsafe { attr_ref(attr) }.map_or(-1, |a| a.max_colors() as c_int)
}

/// Speed 1 (slowest, best) to 10 (fastest), default 3.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_set_speed(attr: *mut liq_attr, speed: c_int) -> liq_error {
    // SAFETY: caller contract.
    let Some(attr) = (unsafe { attr_mut(attr) }) else {
        return LIQ_INVALID_POINTER;
    };
    result_code(attr.set_speed(speed))
}

/// Speed setting, or -1 for an invalid handle.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_get_speed(attr: *const liq_attr) -> c_int {
    // SAFETY: caller contract.
    unsafe { attr_ref(attr) }.map_or(-1, |a| a.speed() as c_int)
}

/// Alpha values at or above `min` (0-255) become opaque; 255 (the default) disables this.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_set_min_opacity(attr: *mut liq_attr, min: c_int) -> liq_error {
    // SAFETY: caller contract.
    let Some(attr) = (unsafe { attr_mut(attr) }) else {
        return LIQ_INVALID_POINTER;
    };
    match u8::try_from(min) {
        Ok(min) => result_code(attr.set_min_opacity(min)),
        Err(_) => LIQ_VALUE_OUT_OF_RANGE,
    }
}

/// Minimum opacity setting, or -1 for an invalid handle.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_get_min_opacity(attr: *const liq_attr) -> c_int {
    // SAFETY: caller contract.
    unsafe { attr_ref(attr) }.map_or(-1, |a| a.min_opacity().into())
}

/// Ignores the given number of least significant bits (0-4) in the output.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_set_min_posterization(attr: *mut liq_attr, bits: c_int) -> liq_error {
    // SAFETY: caller contract.
    let Some(attr) = (unsafe { attr_mut(attr) }) else {
        return LIQ_INVALID_POINTER;
    };
    match u8::try_from(bits) {
        Ok(bits) => result_code(attr.set_min_posterization(bits)),
        Err(_) => LIQ_VALUE_OUT_OF_RANGE,
    }
}

/// Minimum posterization setting, or -1 for an invalid handle.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_get_min_posterization(attr: *const liq_attr) -> c_int {
    // SAFETY: caller contract.
    unsafe { attr_ref(attr) }.map_or(-1, |a| a.min_posterization().into())
}

/// Quality range 0-100 (`minimum` <= `target`), like JPEG quality.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_set_quality(attr: *mut liq_attr, minimum: c_int, target: c_int) -> liq_error {
    // SAFETY: caller contract.
    let Some(attr) = (unsafe { attr_mut(attr) }) else {
        return LIQ_INVALID_POINTER;
    };
    if !(0..=100).contains(&target) || !(0..=target).contains(&minimum) {
        return LIQ_VALUE_OUT_OF_RANGE;
    }
    result_code(attr.set_quality(minimum as u8, target as u8))
}

/// Minimum quality, or -1 for an invalid handle.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_get_min_quality(attr: *const liq_attr) -> c_int {
    // SAFETY: caller contract.
    unsafe { attr_ref(attr) }.map_or(-1, |a| a.quality().0.into())
}

/// Target quality, or -1 for an invalid handle.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_get_max_quality(attr: *const liq_attr) -> c_int {
    // SAFETY: caller contract.
    unsafe { attr_ref(attr) }.map_or(-1, |a| a.quality().1.into())
}

/// Puts the transparent color last in the palette (a workaround for Blu-ray subtitles).
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_set_last_index_transparent(attr: *mut liq_attr, is_last: c_int) {
    // SAFETY: caller contract.
    if let Some(attr) = unsafe { attr_mut(attr) } {
        attr.set_last_index_transparent(is_last != 0);
    }
}

/// Sets (or with NULL removes) the log callback, after calling the log flush callback.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library. `callback` must be safe to call
/// with `user_info` from any thread until it is replaced or the attr is destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_set_log_callback(
    attr: *mut liq_attr,
    callback: Option<liq_log_callback_function>,
    user_info: *mut c_void,
) {
    // SAFETY: caller contract.
    if !unsafe { is_valid(attr) } {
        return;
    }
    // SAFETY: valid handle; no borrows are live while the flush callback runs.
    unsafe {
        flush_log(attr);
        (*attr).log = callback.map(|cb| (cb, user_info));
        install_log_callback(attr);
    }
}

/// Sets (or with NULL removes) the log flush callback.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library. `callback` must be safe to call
/// with `user_info` until it is replaced or the attr is destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_set_log_flush_callback(
    attr: *mut liq_attr,
    callback: Option<liq_log_flush_callback_function>,
    user_info: *mut c_void,
) {
    // SAFETY: caller contract.
    if !unsafe { is_valid(attr) } {
        return;
    }
    // SAFETY: valid handle.
    unsafe { (*attr).log_flush = callback.map(|cb| (cb, user_info)) };
}

/// Selects the algorithms (libimgquant extension): [`LIQ_PROFILE_COMPAT`] or
/// [`LIQ_PROFILE_IMPROVED`]. Settings are kept.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_set_profile(attr: *mut liq_attr, profile: c_int) -> liq_error {
    // SAFETY: caller contract.
    let Some(attr) = (unsafe { attr_mut(attr) }) else {
        return LIQ_INVALID_POINTER;
    };
    let profile = match profile {
        LIQ_PROFILE_COMPAT => Profile::Compat,
        LIQ_PROFILE_IMPROVED => Profile::Improved,
        _ => return LIQ_VALUE_OUT_OF_RANGE,
    };
    attr.set_profile(profile);
    LIQ_OK
}

/// The selected profile (libimgquant extension), or -1 for an invalid handle.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_get_profile(attr: *const liq_attr) -> c_int {
    // SAFETY: caller contract.
    unsafe { attr_ref(attr) }.map_or(-1, |a| match a.profile() {
        Profile::Compat => LIQ_PROFILE_COMPAT,
        Profile::Improved => LIQ_PROFILE_IMPROVED,
        _ => -1,
    })
}

/// `LIQ_VERSION` of this library (libimgquant extension; libimagequant 2.4.1 exported it
/// without declaring it).
#[unsafe(no_mangle)]
pub extern "C" fn liq_version() -> c_int {
    LIQ_VERSION
}

// ---------------------------------------------------------------------------------------------
// Images

/// Checks the arguments shared by the image constructors, logging like libimagequant 2.4.1.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library.
unsafe fn check_image_size(attr: *const liq_attr, width: c_int, height: c_int) -> bool {
    // SAFETY: caller contract.
    if !unsafe { is_valid(attr) } {
        return false;
    }
    let message = if width <= 0 || height <= 0 {
        "width and height must be > 0"
    } else if width > c_int::MAX / height {
        "image too large"
    } else {
        return true;
    };
    // SAFETY: valid handle.
    unsafe { log_error(attr, message) };
    false
}

/// Creates the image handle around a row source.
///
/// # Safety
/// `attr` must be a valid `liq_attr`; `fill_row` must be callable until the image is destroyed.
unsafe fn create_image<F>(
    attr: *const liq_attr,
    fill_row: F,
    width: c_int,
    height: c_int,
    gamma: f64,
    origin: Origin,
) -> *mut liq_image
where
    F: Fn(&mut [RGBA], usize) + Send + Sync + 'static,
{
    // SAFETY: valid handle, shared borrow only (the library may call the log callback).
    let (settings, free) = unsafe { (&(*attr).attr, (*attr).free) };
    // Sizes were checked by `check_image_size`.
    let Ok(image) = settings.new_image_fn(fill_row, width as usize, height as usize, gamma) else {
        return ptr::null_mut();
    };
    Box::into_raw(Box::new(liq_image {
        header: Header::new(MAGIC_IMAGE),
        width,
        height,
        image,
        origin,
        free,
        own_rows: false,
        own_pixels: false,
    }))
}

/// Creates an image from a contiguous bitmap of `width * height` RGBA pixels (4 bytes each).
/// Returns NULL on invalid arguments.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library. `bitmap` must be NULL or point to
/// `width * height * 4` readable bytes that stay valid and unchanged until the image is
/// destroyed (remapping the image into the bitmap itself is allowed).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_image_create_rgba(
    attr: *mut liq_attr,
    bitmap: *mut c_void,
    width: c_int,
    height: c_int,
    gamma: f64,
) -> *mut liq_image {
    // SAFETY: caller contract.
    if !unsafe { check_image_size(attr, width, height) } {
        return ptr::null_mut();
    }
    if bitmap.is_null() {
        // SAFETY: checked above.
        unsafe { log_error(attr, "invalid bitmap pointer") };
        return ptr::null_mut();
    }
    let base = SendPtr(bitmap.cast::<RGBA>());
    let w = width as usize;
    let fill_row = move |out: &mut [RGBA], row: usize| {
        // SAFETY: row < height, so the row lies inside the caller's bitmap; RGBA is 4 bytes
        // with alignment 1, and `out` is a separate Rust buffer.
        unsafe { ptr::copy_nonoverlapping(base.get().add(row * w), out.as_mut_ptr(), out.len().min(w)) };
    };
    let origin = Origin {
        source: ImageSource::Bitmap,
        rows_array: ptr::null_mut(),
        pixels: bitmap,
    };
    // SAFETY: checked above.
    unsafe { create_image(attr, fill_row, width, height, gamma, origin) }
}

/// Creates an image from `height` row pointers, each to `width` RGBA pixels.
/// Returns NULL on invalid arguments (including NULL rows).
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library. `rows` must be NULL or point to
/// `height` pointers, each NULL or to `width * 4` readable bytes. The pixels must stay valid
/// and unchanged until the image is destroyed; the array itself only during this call, unless
/// it is handed over with [`LIQ_OWN_ROWS`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_image_create_rgba_rows(
    attr: *mut liq_attr,
    rows: *mut *mut c_void,
    width: c_int,
    height: c_int,
    gamma: f64,
) -> *mut liq_image {
    // SAFETY: caller contract.
    if !unsafe { check_image_size(attr, width, height) } {
        return ptr::null_mut();
    }
    let mut row_ptrs = Vec::new();
    if !rows.is_null() && row_ptrs.try_reserve_exact(height as usize).is_ok() {
        // SAFETY: the caller guarantees `height` readable pointers.
        row_ptrs.extend((0..height as usize).map(|i| unsafe { rows.add(i).read() }));
    }
    if row_ptrs.len() != height as usize || row_ptrs.iter().any(|r| r.is_null()) {
        // SAFETY: checked above.
        unsafe { log_error(attr, "invalid row pointers") };
        return ptr::null_mut();
    }
    let lowest = row_ptrs
        .iter()
        .copied()
        .min_by_key(|r| *r as usize)
        .unwrap_or(ptr::null_mut());
    let row_ptrs: Box<[SendPtr<RGBA>]> = row_ptrs.into_iter().map(|r| SendPtr(r.cast())).collect();
    let w = width as usize;
    let fill_row = move |out: &mut [RGBA], row: usize| {
        if let Some(src) = row_ptrs.get(row) {
            // SAFETY: the caller guarantees `width` readable pixels per row; `out` is a
            // separate Rust buffer.
            unsafe { ptr::copy_nonoverlapping(src.get(), out.as_mut_ptr(), out.len().min(w)) };
        }
    };
    let origin = Origin {
        source: ImageSource::Rows,
        rows_array: rows.cast(),
        pixels: lowest,
    };
    // SAFETY: checked above.
    unsafe { create_image(attr, fill_row, width, height, gamma, origin) }
}

/// Creates an image whose rows are produced by `row_callback(row_out, row, width, user_info)`.
/// Returns NULL on invalid arguments.
///
/// The callback may be called from a worker thread and several times for the same row, but
/// never concurrently for the same image.
///
/// # Safety
/// `attr` must be NULL or a handle returned by this library. The callback must fill `width`
/// pixels and stay callable with `user_info` until the image is destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_image_create_custom(
    attr: *mut liq_attr,
    row_callback: Option<liq_image_get_rgba_row_callback>,
    user_info: *mut c_void,
    width: c_int,
    height: c_int,
    gamma: f64,
) -> *mut liq_image {
    // SAFETY: caller contract.
    if !unsafe { check_image_size(attr, width, height) } {
        return ptr::null_mut();
    }
    let Some(callback) = row_callback else {
        // 2.4.1 reports an invalid gamma before missing row data
        let message = if (0.0..=1.0).contains(&gamma) {
            "missing row data"
        } else {
            "gamma must be >= 0 and <= 1 (try 1/gamma instead)"
        };
        // SAFETY: checked above.
        unsafe { log_error(attr, message) };
        return ptr::null_mut();
    };
    let user_info = SendPtr(user_info);
    let lock = CallbackLock::default();
    let fill_row = move |out: &mut [RGBA], row: usize| {
        let row_width = c_int::try_from(out.len()).unwrap_or(width).min(width);
        // SAFETY: the C callback contract; `out` holds `row_width` pixels, and liq_color has
        // RGBA's layout. `row` < height fits in c_int.
        lock.run(|| unsafe { callback(out.as_mut_ptr().cast(), row as c_int, row_width, user_info.get()) });
    };
    let origin = Origin {
        source: ImageSource::Custom,
        rows_array: ptr::null_mut(),
        pixels: ptr::null_mut(),
    };
    // SAFETY: checked above.
    unsafe { create_image(attr, fill_row, width, height, gamma, origin) }
}

/// Hands memory over to the image, to be freed when it is destroyed: [`LIQ_OWN_PIXELS`] (the
/// bitmap, or for `liq_image_create_rgba_rows` the row with the lowest address) and/or
/// [`LIQ_OWN_ROWS`] (the row pointer array of `liq_image_create_rgba_rows`). Memory is freed
/// with the attr's custom `free`, or C `free`.
///
/// # Safety
/// `image` must be NULL or a handle returned by this library. The memory must come from the
/// matching allocator.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_image_set_memory_ownership(image: *mut liq_image, ownership_flags: c_int) -> liq_error {
    // SAFETY: caller contract.
    if !unsafe { is_valid(image) } {
        return LIQ_INVALID_POINTER;
    }
    // SAFETY: valid handle; only plain fields are accessed.
    let img = unsafe { &mut *image };
    if img.origin.source == ImageSource::Custom
        || ownership_flags == 0
        || ownership_flags & !(LIQ_OWN_ROWS | LIQ_OWN_PIXELS) != 0
    {
        return LIQ_VALUE_OUT_OF_RANGE;
    }
    if ownership_flags & LIQ_OWN_ROWS != 0 {
        // the rows of liq_image_create_rgba are internal
        if img.origin.source == ImageSource::Bitmap {
            return LIQ_VALUE_OUT_OF_RANGE;
        }
        img.own_rows = true;
    }
    if ownership_flags & LIQ_OWN_PIXELS != 0 {
        img.own_pixels = true;
    }
    LIQ_OK
}

/// Reserves a color that is always in the palette (up to 256).
///
/// # Safety
/// `image` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_image_add_fixed_color(image: *mut liq_image, color: liq_color) -> liq_error {
    // SAFETY: caller contract.
    if !unsafe { is_valid(image) } {
        return LIQ_INVALID_POINTER;
    }
    // SAFETY: valid handle.
    let image = unsafe { &mut (*image).image };
    result_code(image.add_fixed_color(RGBA::new(color.r, color.g, color.b, color.a)))
}

/// Width in pixels, or -1 for an invalid handle.
///
/// # Safety
/// `image` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_image_get_width(image: *const liq_image) -> c_int {
    // SAFETY: caller contract; a plain field is read without borrowing the image.
    unsafe { if is_valid(image) { (*image).width } else { -1 } }
}

/// Height in pixels, or -1 for an invalid handle.
///
/// # Safety
/// `image` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_image_get_height(image: *const liq_image) -> c_int {
    // SAFETY: caller contract; a plain field is read without borrowing the image.
    unsafe { if is_valid(image) { (*image).height } else { -1 } }
}

/// Frees the image, and the memory handed over with [`liq_image_set_memory_ownership`].
///
/// # Safety
/// `image` must be NULL or a handle returned by this library, not used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_image_destroy(image: *mut liq_image) {
    // SAFETY: caller contract.
    if !unsafe { is_valid(image) } {
        return;
    }
    // SAFETY: valid handle, given up by the caller.
    let img = unsafe {
        mark_freed(image);
        Box::from_raw(image)
    };
    let free = img.free;
    let pixels = (img.own_pixels && !img.origin.pixels.is_null()).then_some(img.origin.pixels);
    let rows = (img.own_rows && !img.origin.rows_array.is_null()).then_some(img.origin.rows_array);
    // The row source must be gone before the memory it reads is freed.
    drop(img);
    for ptr in [pixels, rows].into_iter().flatten() {
        // SAFETY: the caller handed this memory over, allocated with the matching allocator.
        unsafe {
            match free {
                Some(free) => free(ptr),
                None => libc_free(ptr),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Quantization results

/// Generates a palette for the image. Returns NULL on failure (invalid handles, quality below
/// the minimum, out of memory).
///
/// # Safety
/// `attr` and `image` must be NULL or handles returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_quantize_image(attr: *mut liq_attr, image: *mut liq_image) -> *mut liq_result {
    // SAFETY: caller contract.
    if !unsafe { is_valid(attr) } {
        return ptr::null_mut();
    }
    // SAFETY: caller contract.
    if !unsafe { is_valid(image) } {
        // SAFETY: valid handle.
        unsafe { log_error(attr, "invalid image pointer") };
        return ptr::null_mut();
    }
    // SAFETY: valid handles of different types, so distinct objects. The attr is only shared
    // (its log callback may read it), and only the image's `image` field is borrowed mutably.
    let (settings, img) = unsafe { (&(*attr).attr, &mut (*image).image) };
    let Ok(result) = settings.quantize(img) else {
        return ptr::null_mut();
    };
    let palette = Box::new(liq_palette {
        count: 0,
        entries: [liq_color::default(); 256],
    });
    Box::into_raw(Box::new(liq_result {
        header: Header::new(MAGIC_RESULT),
        result,
        palette: NonNull::from(Box::leak(palette)),
    }))
}

/// Dithering level for remapping, 0 (none, the default) to 1 (full).
///
/// # Safety
/// `result` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_set_dithering_level(result: *mut liq_result, dither_level: f32) -> liq_error {
    // SAFETY: caller contract.
    if !unsafe { is_valid(result) } {
        return LIQ_INVALID_POINTER;
    }
    // SAFETY: valid handle.
    result_code(unsafe { (*result).result.set_dithering_level(dither_level) })
}

/// Gamma of the output palette, between 0 and 1 exclusive (default: the image's gamma).
///
/// # Safety
/// `result` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_set_output_gamma(result: *mut liq_result, gamma: f64) -> liq_error {
    // SAFETY: caller contract.
    if !unsafe { is_valid(result) } {
        return LIQ_INVALID_POINTER;
    }
    // SAFETY: valid handle.
    result_code(unsafe { (*result).result.set_output_gamma(gamma) })
}

/// Gamma of the output palette, or -1 for an invalid handle.
///
/// # Safety
/// `result` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_get_output_gamma(result: *const liq_result) -> f64 {
    // SAFETY: caller contract.
    unsafe {
        if is_valid(result) {
            (*result).result.output_gamma()
        } else {
            -1.0
        }
    }
}

/// Copies the current palette into the result's `liq_palette` and returns it.
///
/// # Safety
/// `result` must be a valid `liq_result` with no other borrows.
unsafe fn update_palette(result: *mut liq_result) -> *const liq_palette {
    // SAFETY: valid handle; the palette allocation is separate from the result.
    let (colors, out) = unsafe { ((*result).result.palette(), (*result).palette) };
    let mut palette = liq_palette {
        count: colors.len() as c_uint,
        entries: [liq_color::default(); 256],
    };
    for (entry, c) in palette.entries.iter_mut().zip(colors) {
        *entry = liq_color {
            r: c.r,
            g: c.g,
            b: c.b,
            a: c.a,
        };
    }
    // SAFETY: owned by the handle; C may hold a pointer to it but does not access it during
    // this call.
    unsafe { out.as_ptr().write(palette) };
    out.as_ptr()
}

/// The palette. After remapping, it is the palette the remapped pixels refer to. The pointer is
/// the same for every call on one result and valid until [`liq_result_destroy`]. NULL for an
/// invalid handle.
///
/// # Safety
/// `result` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_get_palette(result: *mut liq_result) -> *const liq_palette {
    // SAFETY: caller contract.
    if !unsafe { is_valid(result) } {
        return ptr::null();
    }
    // SAFETY: valid handle.
    unsafe { update_palette(result) }
}

/// Remaps the image into a temporary buffer, then calls `write_out` with it.
///
/// # Safety
/// `result` and `image` must be valid handles.
unsafe fn remap(result: *mut liq_result, image: *mut liq_image, write_out: impl FnOnce(&[u8])) -> liq_error {
    // SAFETY: valid handles.
    let (width, height) = unsafe { ((*image).width as usize, (*image).height as usize) };
    let len = width * height; // at most c_int::MAX
    let mut buffer = Vec::new();
    if buffer.try_reserve_exact(len).is_err() {
        return LIQ_OUT_OF_MEMORY;
    }
    buffer.resize(len, 0u8);
    // SAFETY: valid handles of different types, so distinct objects; only their Rust fields
    // are borrowed.
    let (res, img) = unsafe { (&mut (*result).result, &mut (*image).image) };
    if let Err(err) = res.remap_into(img, &mut buffer) {
        return error_code(err);
    }
    write_out(&buffer);
    // SAFETY: valid handle; the borrows above have ended.
    unsafe { update_palette(result) };
    LIQ_OK
}

/// Remaps the image to palette indices, written row by row without padding into `buffer`,
/// which must hold at least `width * height` bytes. The buffer may be the image's bitmap.
///
/// # Safety
/// `result` and `image` must be NULL or handles returned by this library. `buffer` must be
/// NULL or point to `buffer_size` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_write_remapped_image(
    result: *mut liq_result,
    image: *mut liq_image,
    buffer: *mut c_void,
    buffer_size: usize,
) -> liq_error {
    // SAFETY: caller contract.
    if !unsafe { is_valid(result) && is_valid(image) } || buffer.is_null() {
        return LIQ_INVALID_POINTER;
    }
    // SAFETY: valid handle.
    let required = unsafe { (*image).width as usize * (*image).height as usize };
    if buffer_size < required {
        return LIQ_BUFFER_TOO_SMALL;
    }
    // SAFETY: valid handles; `buffer` holds `required` bytes and is only written after
    // remapping, when the image is no longer read.
    unsafe {
        remap(result, image, |indices| {
            ptr::copy_nonoverlapping(indices.as_ptr(), buffer.cast::<u8>(), indices.len());
        })
    }
}

/// Remaps the image to palette indices, written into `height` rows of at least `width` bytes.
///
/// # Safety
/// `result` and `image` must be NULL or handles returned by this library. `row_pointers` must
/// be NULL or point to `height` pointers, each NULL or to `width` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_write_remapped_image_rows(
    result: *mut liq_result,
    image: *mut liq_image,
    row_pointers: *mut *mut u8,
) -> liq_error {
    // SAFETY: caller contract.
    if !unsafe { is_valid(result) && is_valid(image) } || row_pointers.is_null() {
        return LIQ_INVALID_POINTER;
    }
    // SAFETY: valid handle.
    let (width, height) = unsafe { ((*image).width as usize, (*image).height as usize) };
    let mut rows = Vec::new();
    if rows.try_reserve_exact(height).is_err() {
        return LIQ_OUT_OF_MEMORY;
    }
    // SAFETY: the caller guarantees `height` readable pointers.
    rows.extend((0..height).map(|i| unsafe { row_pointers.add(i).read() }));
    if rows.iter().any(|r| r.is_null()) {
        return LIQ_INVALID_POINTER;
    }
    // SAFETY: valid handles; each row holds `width` bytes and is only written after remapping.
    unsafe {
        remap(result, image, |indices| {
            for (row, src) in rows.iter().zip(indices.chunks_exact(width)) {
                ptr::copy_nonoverlapping(src.as_ptr(), *row, width);
            }
        })
    }
}

/// Mean square error of the palette, scaled like pngquant's `--verbose` output, or -1 if it
/// has not been measured yet (it is after remapping) or the handle is invalid.
///
/// # Safety
/// `result` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_get_quantization_error(result: *mut liq_result) -> f64 {
    // SAFETY: caller contract.
    if !unsafe { is_valid(result) } {
        return -1.0;
    }
    // SAFETY: valid handle.
    unsafe { (*result).result.quantization_error() }.unwrap_or(-1.0)
}

/// Quality 0-100 matching `liq_set_quality`, or -1 if it has not been measured yet (it is after
/// remapping) or the handle is invalid.
///
/// # Safety
/// `result` must be NULL or a handle returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_get_quantization_quality(result: *mut liq_result) -> c_int {
    // SAFETY: caller contract.
    if !unsafe { is_valid(result) } {
        return -1;
    }
    // SAFETY: valid handle.
    unsafe { (*result).result.quantization_quality() }.map_or(-1, c_int::from)
}

/// Frees the result and its palette.
///
/// # Safety
/// `result` must be NULL or a handle returned by this library, not used afterwards; pointers
/// returned by [`liq_get_palette`] become invalid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn liq_result_destroy(result: *mut liq_result) {
    // SAFETY: caller contract.
    if !unsafe { is_valid(result) } {
        return;
    }
    // SAFETY: valid handle, given up by the caller; the palette was allocated by Box.
    unsafe {
        mark_freed(result);
        let result = Box::from_raw(result);
        drop(Box::from_raw(result.palette.as_ptr()));
    }
}

const _: () = {
    assert!(size_of::<liq_color>() == size_of::<RGBA>());
    assert!(align_of::<liq_color>() == align_of::<RGBA>());
    assert!(size_of::<liq_error>() == size_of::<c_int>());
};
