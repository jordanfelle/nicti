// Flat C-ABI accessors over LibRaw's C++ class, hand-written rather than bindgen'd against
// libraw_data_t (see #37's ADR/write-up for why: libraw_data_t is a deep, manufacturer-union-heavy
// struct, and bindgen redriving that layout independently of the headers this shim actually
// compiles against is a real fragility risk -- this shim is compiled against the exact same
// headers LibRaw itself uses, so there is no second source of truth to drift).
//
// Only the fields retina's `RawFrame` (a Bayer CFA frame + the metadata needed to interpret it)
// needs are exposed. Nothing here is a stable public API -- this is throwaway spike code, see
// spikes/retina's Cargo.toml description.
#pragma once
#include <cstddef>
#include <cstdint>

extern "C" {

// Opaque handle. The real type is `LibRaw*`; Rust never sees the definition.
typedef struct RetinaLibRaw RetinaLibRaw;

// Mirrors LibRaw's own error codes closely enough for retina's purposes: 0 = LIBRAW_SUCCESS,
// negative = a LibRaw enum value (see libraw_const.h), passed through unchanged so error messages
// can call retina_strerror.
typedef int32_t RetinaStatus;

RetinaLibRaw *retina_libraw_new();
void retina_libraw_free(RetinaLibRaw *handle);

// Full decode pipeline: open the in-memory buffer, unpack (Bayer-plane decode -- this is where
// LibRaw's stock decoder or the vendored nikon_he/* decoder actually runs), then raw2image (fills
// in imgdata.image / crop bookkeeping used by the getters below). Returns the first non-zero
// status, if any.
RetinaStatus retina_libraw_decode_buffer(RetinaLibRaw *handle, const uint8_t *data, size_t len);

const char *retina_strerror(RetinaStatus status);

// --- Metadata, valid only after a successful retina_libraw_decode_buffer ---

// NEFCompression tag value (see docs/ref-10k-manifest.csv's `compression` column derivation):
// 6/7/8/9 map to Lossy/Lossless/Uncompressed/whatever this vendored fork defines for
// HighEfficiency/HighEfficiencyStar -- retina's Rust side maps the raw tag value to a label rather
// than hand-duplicating LibRaw's own enum.
uint16_t retina_nef_compression(const RetinaLibRaw *handle);

uint16_t retina_raw_width(const RetinaLibRaw *handle);
uint16_t retina_raw_height(const RetinaLibRaw *handle);
uint16_t retina_iwidth(const RetinaLibRaw *handle);
uint16_t retina_iheight(const RetinaLibRaw *handle);
uint16_t retina_top_margin(const RetinaLibRaw *handle);
uint16_t retina_left_margin(const RetinaLibRaw *handle);
unsigned retina_raw_pitch(const RetinaLibRaw *handle);

unsigned retina_filters(const RetinaLibRaw *handle);
int retina_colors(const RetinaLibRaw *handle);

unsigned retina_black(const RetinaLibRaw *handle);
unsigned retina_maximum(const RetinaLibRaw *handle);

// Writes exactly 4 floats.
void retina_cam_mul(const RetinaLibRaw *handle, float out[4]);
// Writes exactly 12 floats (row-major 3x4).
void retina_rgb_cam(const RetinaLibRaw *handle, float out[12]);

// Model/make strings, NUL-terminated, valid for the handle's lifetime (point into
// imgdata.idata, not a fresh allocation -- don't free).
const char *retina_make(const RetinaLibRaw *handle);
const char *retina_model(const RetinaLibRaw *handle);

// Pointer + length (in ushorts) of the still-mosaiced Bayer plane (imgdata.rawdata.raw_image),
// i.e. before any demosaic. Valid only until the next call on this handle or
// retina_libraw_free. Null if the decoder didn't populate a Bayer plane (e.g. an X-Trans/Foveon
// body -- not expected for retina's Nikon-only sweep, but checked rather than assumed).
const uint16_t *retina_raw_image(const RetinaLibRaw *handle, size_t *out_len);

// --- #38/calico support: demosaic-only decode, no WB/color-matrix/gamma applied ---
//
// Must be called after a successful retina_libraw_decode_buffer, before any getter below. Sets
// LibRaw's own dcraw_process() parameters so the only transform it performs is demosaicing plus
// the black/white-level linear scaling every LibRaw decode does regardless of settings --
// deliberately NOT applying white balance (user_mul = {1,1,1,1} disables LibRaw's own cam_mul/
// pre_mul auto-selection), NOT converting to any output color space (output_color = 0, "raw"),
// and NOT applying a gamma/tone curve (gamm = {1,1}, linear). calico's own pipeline.rs applies
// WB, the camera-to-XYZ matrix, and tone curve itself from the metadata this shim also exposes
// (retina_cam_xyz/retina_pre_mul/retina_cblack alongside the existing retina_cam_mul), so the
// two must not double-apply any of those stages.
RetinaStatus retina_libraw_process_linear(RetinaLibRaw *handle);

// Pointer + length (in ushorts) of the demosaiced RGBG image (imgdata.image), valid only after a
// successful retina_libraw_process_linear. Length is iwidth * iheight * 4 (4 ushorts/pixel: R,
// G, B, G2 -- G2 is folded into G by LibRaw during raw2image for non-Bayer-4-color sensors, which
// covers every body ref-10k contains, so Rust only reads channels 0/1/2). Dimensions are
// retina_iwidth()/retina_iheight() (post-crop, i.e. the *usable* image, not raw_width/raw_height).
const uint16_t *retina_linear_image(const RetinaLibRaw *handle, size_t *out_len);

// Writes exactly 4 floats: LibRaw's own daylight-calibration multipliers (imgdata.color.pre_mul),
// distinct from retina_cam_mul's as-shot multipliers -- DNG's dual-illuminant interpolation needs
// both to estimate a correlated color temperature from the as-shot neutral.
void retina_pre_mul(const RetinaLibRaw *handle, float out[4]);

// Writes exactly 12 floats (row-major 4x3: up to 4 camera channels x XYZ). Rows for unused
// channels (colors < 4) are zero. This is LibRaw's own camera->XYZ matrix (imgdata.color.cam_xyz),
// derived from whichever profile LibRaw picked for this camera model -- a fallback for cameras
// or DCP-less setups where calico has no ForwardMatrix/ColorMatrix of its own; ADR-0021 covers
// when each is used.
void retina_cam_xyz(const RetinaLibRaw *handle, float out[12]);

// Writes exactly 4 unsigned ints: the per-channel black-level additions LibRaw already folded
// into retina_black() as a single scalar (imgdata.color.cblack[0..3]). Does NOT include LibRaw's
// per-pixel black pattern map (cblack[4]/cblack[5] and beyond) -- large-scale black-level shading
// patterns are out of scope for this research pass; see ADR-0021's Deferred section.
void retina_cblack(const RetinaLibRaw *handle, uint32_t out[4]);

}
