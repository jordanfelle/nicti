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

}
