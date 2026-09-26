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

// --- #40 support: classic demosaic + NR comparison (ADR-0023) ---
//
// Mirrors LibRaw's own `-q`/user_qual enum: 0=linear, 1=VNG, 2=PPG, 3=AHD, 4=DCB (patched build
// only), 11=DHT, 12=AAHD (patched build only -- LibRaw upstream reserves 5-10 for other forks'
// demosaic algorithms retina's vendored fork doesn't add).
typedef enum RetinaDemosaicQuality {
  RETINA_DEMOSAIC_LINEAR = 0,
  RETINA_DEMOSAIC_VNG = 1,
  RETINA_DEMOSAIC_PPG = 2,
  RETINA_DEMOSAIC_AHD = 3,
  RETINA_DEMOSAIC_DCB = 4,
  RETINA_DEMOSAIC_DHT = 11,
  RETINA_DEMOSAIC_AAHD = 12,
} RetinaDemosaicQuality;

// Must be called after a successful retina_libraw_decode_buffer, before any getter below. Unlike
// retina_libraw_process_linear (which deliberately disables WB/FBDD/wavelet-NR so calico can
// apply color correction itself), this runs LibRaw's classic pipeline with white balance applied
// (use_camera_wb=1, the as-shot cam_mul) and the caller's choice of demosaic algorithm plus
// LibRaw's own noise-reduction knobs -- FBDD (0=off, 1=before demosaic, 2=before demosaic +
// smoother) and post-demosaic wavelet denoise threshold (0 = off). Still output_color=0 ("raw")
// and gamm={1,1} (linear): #40's `rods` spike applies the same fixed cam_xyz->sRGB matrix +
// sRGB OETF to every candidate (classic and AI alike) rather than going through LibRaw's own
// output-color-space conversion, so demosaic/NR differences aren't confounded by a second color
// pipeline. See docs/research/rods-demosaic-denoise.md.
RetinaStatus retina_libraw_process_classic(RetinaLibRaw *handle,
                                            RetinaDemosaicQuality quality,
                                            int fbdd_noiserd,
                                            float wavelet_threshold);

// Pointer + length (in ushorts) of the demosaiced RGBG image (imgdata.image) produced by
// retina_libraw_process_classic. Same layout/lifetime rules as retina_linear_image (4
// ushorts/pixel R,G,B,G2; dimensions are retina_iwidth()/retina_iheight()).
const uint16_t *retina_classic_image(const RetinaLibRaw *handle, size_t *out_len);

// --- #40 support: Path A (Bayer-domain model) input (ADR-0023) ---
//
// Must be called after a successful retina_libraw_decode_buffer. Writes a black-subtracted,
// white-normalized (divided by retina_maximum()-retina_black(), clamped to [0,1]) copy of the
// still-mosaiced Bayer plane into caller-owned `out` (must hold exactly
// retina_raw_width()*retina_raw_height() floats, row-major, same RGGB-phase layout LibRaw itself
// uses -- callers determine per-pixel color via retina_filters()'s FC() pattern, same as
// raw_image()). No white balance, no demosaic. Returns false (leaving `out` untouched) if no
// Bayer plane was populated (see retina_raw_image's same caveat), true on success.
bool retina_cfa_normalized(const RetinaLibRaw *handle, float *out, size_t out_len);

}
