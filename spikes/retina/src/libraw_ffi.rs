//! Safe wrapper over `shim.h`'s flat C-ABI accessors. See shim.h for why this isn't bindgen'd.

use std::ffi::{c_char, CStr};

#[repr(C)]
struct RetinaLibRawOpaque {
    _private: [u8; 0],
}

type RetinaStatus = i32;

unsafe extern "C" {
    fn retina_libraw_new() -> *mut RetinaLibRawOpaque;
    fn retina_libraw_free(handle: *mut RetinaLibRawOpaque);
    fn retina_libraw_decode_buffer(
        handle: *mut RetinaLibRawOpaque,
        data: *const u8,
        len: usize,
    ) -> RetinaStatus;
    fn retina_strerror(status: RetinaStatus) -> *const c_char;

    fn retina_nef_compression(handle: *const RetinaLibRawOpaque) -> u16;
    fn retina_raw_width(handle: *const RetinaLibRawOpaque) -> u16;
    fn retina_raw_height(handle: *const RetinaLibRawOpaque) -> u16;
    fn retina_iwidth(handle: *const RetinaLibRawOpaque) -> u16;
    fn retina_iheight(handle: *const RetinaLibRawOpaque) -> u16;
    fn retina_top_margin(handle: *const RetinaLibRawOpaque) -> u16;
    fn retina_left_margin(handle: *const RetinaLibRawOpaque) -> u16;
    fn retina_raw_pitch(handle: *const RetinaLibRawOpaque) -> u32;
    fn retina_filters(handle: *const RetinaLibRawOpaque) -> u32;
    fn retina_colors(handle: *const RetinaLibRawOpaque) -> i32;
    fn retina_black(handle: *const RetinaLibRawOpaque) -> u32;
    fn retina_maximum(handle: *const RetinaLibRawOpaque) -> u32;
    fn retina_cam_mul(handle: *const RetinaLibRawOpaque, out: *mut f32);
    fn retina_rgb_cam(handle: *const RetinaLibRawOpaque, out: *mut f32);
    fn retina_make(handle: *const RetinaLibRawOpaque) -> *const c_char;
    fn retina_model(handle: *const RetinaLibRawOpaque) -> *const c_char;
    fn retina_raw_image(handle: *const RetinaLibRawOpaque, out_len: *mut usize) -> *const u16;
}

#[derive(Debug, thiserror::Error)]
pub enum LibRawError {
    #[error("LibRaw error {code}: {message}")]
    Status { code: i32, message: String },
    #[error(
        "LibRaw decoded the file but reported no Bayer plane (raw_image is null) -- \
             not expected for a Nikon-only sweep, check the file/decoder path"
    )]
    NoRawImage,
}

/// Owns a `LibRaw` C++ instance for exactly one decode. Not `Send`/`Sync` -- callers doing
/// concurrent decodes (retina's `sweep --threads N`) must construct one `LibRawHandle` per
/// rayon task, never share one across threads (matches how the reentrant `libraw_r` build is
/// meant to be used: one instance per concurrent decode, see build.rs's `-pthread` note).
pub struct LibRawHandle {
    ptr: *mut RetinaLibRawOpaque,
}

impl LibRawHandle {
    pub fn new() -> Self {
        let ptr = unsafe { retina_libraw_new() };
        assert!(!ptr.is_null(), "LibRaw allocation failed (out of memory?)");
        LibRawHandle { ptr }
    }

    /// Decodes `data` (a full NEF/DNG file read into memory). On success, the metadata/`raw_image`
    /// getters below become valid until this handle is dropped or `decode` is called again.
    pub fn decode(&mut self, data: &[u8]) -> Result<(), LibRawError> {
        let status = unsafe { retina_libraw_decode_buffer(self.ptr, data.as_ptr(), data.len()) };
        if status != 0 {
            let msg = unsafe {
                let s = retina_strerror(status);
                if s.is_null() {
                    "<null>".to_string()
                } else {
                    CStr::from_ptr(s).to_string_lossy().into_owned()
                }
            };
            return Err(LibRawError::Status {
                code: status,
                message: msg,
            });
        }
        Ok(())
    }

    pub fn nef_compression(&self) -> u16 {
        unsafe { retina_nef_compression(self.ptr) }
    }

    pub fn metadata(&self) -> DecodedMetadata {
        let mut cam_mul = [0f32; 4];
        let mut rgb_cam = [0f32; 12];
        unsafe {
            retina_cam_mul(self.ptr, cam_mul.as_mut_ptr());
            retina_rgb_cam(self.ptr, rgb_cam.as_mut_ptr());
        }
        DecodedMetadata {
            make: read_c_string(unsafe { retina_make(self.ptr) }),
            model: read_c_string(unsafe { retina_model(self.ptr) }),
            nef_compression: self.nef_compression(),
            raw_width: unsafe { retina_raw_width(self.ptr) },
            raw_height: unsafe { retina_raw_height(self.ptr) },
            iwidth: unsafe { retina_iwidth(self.ptr) },
            iheight: unsafe { retina_iheight(self.ptr) },
            top_margin: unsafe { retina_top_margin(self.ptr) },
            left_margin: unsafe { retina_left_margin(self.ptr) },
            raw_pitch: unsafe { retina_raw_pitch(self.ptr) },
            filters: unsafe { retina_filters(self.ptr) },
            colors: unsafe { retina_colors(self.ptr) },
            black: unsafe { retina_black(self.ptr) },
            maximum: unsafe { retina_maximum(self.ptr) },
            cam_mul,
            rgb_cam,
        }
    }

    /// The still-mosaiced Bayer plane. Borrowed from the handle -- copy it out (e.g. into a
    /// `RawFrame`) before calling `decode` again or dropping this handle.
    pub fn raw_image(&self) -> Result<&[u16], LibRawError> {
        let mut len = 0usize;
        let ptr = unsafe { retina_raw_image(self.ptr, &mut len as *mut usize) };
        if ptr.is_null() {
            return Err(LibRawError::NoRawImage);
        }
        Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
    }
}

impl Drop for LibRawHandle {
    fn drop(&mut self) {
        unsafe { retina_libraw_free(self.ptr) };
    }
}

// `LibRaw*` itself isn't documented thread-safe to share; `LibRawHandle` is neither `Send` nor
// `Sync` by default since it's a raw pointer, which is exactly the safety property we want here
// (see the doc comment on the struct) -- no explicit impl needed.

fn read_c_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // iwidth/iheight/raw_pitch/rgb_cam captured for completeness, not yet consumed
pub struct DecodedMetadata {
    pub make: String,
    pub model: String,
    pub nef_compression: u16,
    pub raw_width: u16,
    pub raw_height: u16,
    pub iwidth: u16,
    pub iheight: u16,
    pub top_margin: u16,
    pub left_margin: u16,
    pub raw_pitch: u32,
    pub filters: u32,
    pub colors: i32,
    pub black: u32,
    pub maximum: u32,
    pub cam_mul: [f32; 4],
    pub rgb_cam: [f32; 12],
}
