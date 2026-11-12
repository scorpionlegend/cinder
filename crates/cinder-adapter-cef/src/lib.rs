//! CEF-backed engine adapter (L0/L1).
//!
//! This is the only crate in the workspace that links native code, and the only
//! one allowed to contain `unsafe`. Everything it exposes to the rest of Cinder
//! is safe Rust: owned buffers, `Result`, and plain enums.
//!
//! The native surface is a flat C shim (`shim/cinder_cef_shim.c`) rather than a
//! hand-mirrored CEF vtable, so a CEF upgrade cannot silently shift a function
//! pointer out from under us — the compiler checks the shim against the real
//! headers on every build.
#![deny(unsafe_op_in_unsafe_fn)]

use std::ffi::{CStr, CString};
use std::fmt;
use std::os::raw::{c_char, c_int, c_uchar};
use std::path::Path;

mod ffi {
    use super::*;

    extern "C" {
        pub fn cinder_cef_execute_process(argc: c_int, argv: *mut *mut c_char) -> c_int;
        pub fn cinder_cef_init(
            argc: c_int,
            argv: *mut *mut c_char,
            resources_dir: *const c_char,
            locales_dir: *const c_char,
            cache_dir: *const c_char,
        ) -> c_int;
        pub fn cinder_cef_create_browser(
            url: *const c_char,
            width: c_int,
            height: c_int,
        ) -> c_int;
        pub fn cinder_cef_pump(ms: c_int);
        pub fn cinder_cef_state() -> c_int;
        pub fn cinder_cef_frame_size(width: *mut c_int, height: *mut c_int) -> c_int;
        pub fn cinder_cef_copy_frame(dst: *mut c_uchar, capacity: c_int) -> c_int;
        pub fn cinder_cef_title() -> *const c_char;
        pub fn cinder_cef_url() -> *const c_char;
        pub fn cinder_cef_version() -> *const c_char;
        pub fn cinder_cef_last_error() -> *const c_char;
        pub fn cinder_cef_shutdown();
    }
}

pub const STATE_CREATED: i32 = 1;
pub const STATE_LOADED: i32 = 2;
pub const STATE_PAINTED: i32 = 4;
pub const STATE_CLOSED: i32 = 8;

/// Event kinds the shim reports through its callback. Kept in sync with the
/// `CINDER_CEF_EV_*` defines in `shim/cinder_cef_shim.h`.
pub const EV_CREATED: i32 = 1;
pub const EV_LOAD_START: i32 = 2;
pub const EV_LOAD_END: i32 = 3;
pub const EV_LOAD_ERROR: i32 = 4;
pub const EV_TITLE: i32 = 5;
pub const EV_URL: i32 = 6;
pub const EV_CLOSED: i32 = 7;
pub const EV_CLOSE_REQUESTED: i32 = 8;
pub const EV_PAINTED: i32 = 9;
pub const EV_BLOCKED: i32 = 10;

mod engine;
mod runtime;

pub use engine::{measured_capabilities, CefEngine, CefEngineConfig};
pub use runtime::{prepare as prepare_runtime, root as cef_root, stage as stage_runtime};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CefError {
    /// `cef_initialize` returned false.
    InitFailed(String),
    /// Browser creation was refused.
    BrowserFailed(String),
    /// The shim reported a fault.
    Shim(String),
    /// A C string argument contained an interior NUL.
    BadArgument(&'static str),
}

impl fmt::Display for CefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InitFailed(m) => write!(f, "CEF initialization failed: {m}"),
            Self::BrowserFailed(m) => write!(f, "CEF browser creation failed: {m}"),
            Self::Shim(m) => write!(f, "CEF shim error: {m}"),
            Self::BadArgument(what) => write!(f, "invalid argument: {what}"),
        }
    }
}

impl std::error::Error for CefError {}

/// One painted frame, BGRA, tightly packed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl Frame {
    pub fn pixel_count(&self) -> usize {
        self.bgra.len() / 4
    }

    /// Number of pixels that are not the opaque white background CEF paints
    /// before content arrives. A non-zero count is evidence that something was
    /// actually rendered.
    pub fn non_white_pixels(&self) -> usize {
        self.bgra
            .chunks_exact(4)
            .filter(|p| !(p[0] >= 250 && p[1] >= 250 && p[2] >= 250))
            .count()
    }

    /// Distinct RGB triples, capped so a photo-like frame cannot blow up memory.
    pub fn distinct_colors(&self, cap: usize) -> usize {
        let mut seen = std::collections::HashSet::new();
        for p in self.bgra.chunks_exact(4) {
            if seen.len() >= cap {
                break;
            }
            seen.insert((p[0], p[1], p[2]));
        }
        seen.len()
    }

    /// Write a binary PPM (P6). Chosen because it needs no encoder dependency
    /// and can be read back and checked programmatically.
    pub fn write_ppm(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
        write!(out, "P6\n{} {}\n255\n", self.width, self.height)?;
        let mut rgb = Vec::with_capacity(self.pixel_count() * 3);
        for p in self.bgra.chunks_exact(4) {
            rgb.push(p[2]);
            rgb.push(p[1]);
            rgb.push(p[0]);
        }
        out.write_all(&rgb)?;
        out.flush()
    }
}

fn cstr_arg(value: &str, what: &'static str) -> Result<CString, CefError> {
    CString::new(value).map_err(|_| CefError::BadArgument(what))
}

fn copy_c_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: the shim returns pointers to its own NUL-terminated static
    // buffers, valid until the next pump.
    unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
}

/// Process-wide CEF handle. CEF allows exactly one initialization per process
/// and requires it on the main thread, so this is deliberately not `Clone`.
#[derive(Debug)]
pub struct Cef {
    _private: (),
}

impl Cef {
    /// Call first, before [`Cef::init`]. Returns `Some(code)` when this process
    /// is a CEF subprocess and must exit immediately with that code.
    pub fn execute_process() -> Option<i32> {
        let args = std::env::args().collect::<Vec<_>>();
        let mut owned: Vec<CString> = args
            .iter()
            .map(|a| CString::new(a.as_str()).unwrap_or_else(|_| CString::new("cinder").unwrap()))
            .collect();
        let mut ptrs: Vec<*mut c_char> = owned
            .iter_mut()
            .map(|c| c.as_ptr() as *mut c_char)
            .collect();
        let argc = ptrs.len() as c_int;
        // SAFETY: argv is a valid, NUL-terminated array of pointers that
        // outlives the call.
        let code = unsafe { ffi::cinder_cef_execute_process(argc, ptrs.as_mut_ptr()) };
        if code >= 0 {
            Some(code)
        } else {
            None
        }
    }

    /// Initialize CEF with off-screen rendering enabled.
    pub fn init(
        resources_dir: &Path,
        locales_dir: &Path,
        cache_dir: &Path,
    ) -> Result<Self, CefError> {
        Self::init_with_args(resources_dir, locales_dir, cache_dir, &[])
    }

    /// Initialize CEF, appending `extra_args` to the command line Chromium
    /// parses (for example `--icu-data-file=...`).
    pub fn init_with_args(
        resources_dir: &Path,
        locales_dir: &Path,
        cache_dir: &Path,
        extra_args: &[String],
    ) -> Result<Self, CefError> {
        let mut args = std::env::args().collect::<Vec<_>>();
        args.extend(extra_args.iter().cloned());
        let mut owned: Vec<CString> = args
            .iter()
            .map(|a| CString::new(a.as_str()).unwrap_or_else(|_| CString::new("cinder").unwrap()))
            .collect();
        let mut ptrs: Vec<*mut c_char> = owned
            .iter_mut()
            .map(|c| c.as_ptr() as *mut c_char)
            .collect();
        let argc = ptrs.len() as c_int;

        let resources = cstr_arg(&resources_dir.to_string_lossy(), "resources dir")?;
        let locales = cstr_arg(&locales_dir.to_string_lossy(), "locales dir")?;
        let cache = cstr_arg(&cache_dir.to_string_lossy(), "cache dir")?;

        // SAFETY: all pointers are valid for the duration of the call; argv
        // outlives it.
        let ok = unsafe {
            ffi::cinder_cef_init(
                argc,
                ptrs.as_mut_ptr(),
                resources.as_ptr(),
                locales.as_ptr(),
                cache.as_ptr(),
            )
        };
        if ok == 1 {
            Ok(Self { _private: () })
        } else {
            Err(CefError::InitFailed(Self::last_error()))
        }
    }

    pub fn version() -> String {
        // SAFETY: static NUL-terminated string owned by the shim.
        copy_c_string(unsafe { ffi::cinder_cef_version() })
    }

    pub fn last_error() -> String {
        // SAFETY: static NUL-terminated buffer owned by the shim.
        copy_c_string(unsafe { ffi::cinder_cef_last_error() })
    }

    /// Create one windowless browser.
    pub fn create_browser(&self, url: &str, width: u32, height: u32) -> Result<(), CefError> {
        let url = cstr_arg(url, "url")?;
        // SAFETY: url is a valid NUL-terminated string; dimensions are checked.
        let ok = unsafe {
            ffi::cinder_cef_create_browser(url.as_ptr(), width as c_int, height as c_int)
        };
        if ok == 1 {
            Ok(())
        } else {
            Err(CefError::BrowserFailed(Self::last_error()))
        }
    }

    /// Run the CEF message loop for `ms` milliseconds.
    pub fn pump(&self, ms: u32) {
        // SAFETY: must be called on the initializing thread; the type is not
        // Send/Sync so it cannot be moved to another thread.
        unsafe { ffi::cinder_cef_pump(ms as c_int) }
    }

    pub fn state(&self) -> i32 {
        // SAFETY: reads an int from the shim.
        unsafe { ffi::cinder_cef_state() }
    }

    pub fn is_loaded(&self) -> bool {
        self.state() & STATE_LOADED != 0
    }

    pub fn is_painted(&self) -> bool {
        self.state() & STATE_PAINTED != 0
    }

    pub fn title(&self) -> String {
        // SAFETY: static buffer owned by the shim.
        copy_c_string(unsafe { ffi::cinder_cef_title() })
    }

    pub fn url(&self) -> String {
        // SAFETY: static buffer owned by the shim.
        copy_c_string(unsafe { ffi::cinder_cef_url() })
    }

    /// Copy the last painted frame, if any.
    pub fn frame(&self) -> Option<Frame> {
        let mut w: c_int = 0;
        let mut h: c_int = 0;
        // SAFETY: out-pointers are valid.
        let has = unsafe { ffi::cinder_cef_frame_size(&mut w, &mut h) };
        if has != 1 || w <= 0 || h <= 0 {
            return None;
        }
        let len = (w as usize) * (h as usize) * 4;
        let mut buf = vec![0u8; len];
        // SAFETY: buffer is exactly len bytes and capacity is passed in bytes.
        let written = unsafe { ffi::cinder_cef_copy_frame(buf.as_mut_ptr(), len as c_int) };
        if written as usize != len {
            return None;
        }
        Some(Frame {
            width: w as u32,
            height: h as u32,
            bgra: buf,
        })
    }

    /// Shut CEF down. Consumes the handle so it cannot be used afterwards.
    pub fn shutdown(self) {
        // SAFETY: called once, on the initializing thread, after all pumping.
        unsafe { ffi::cinder_cef_shutdown() }
    }
}
