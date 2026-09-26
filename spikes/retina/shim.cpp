#include "shim.h"
#include "libraw/libraw.h"

#include <cstring>
#include <new>

namespace {
LibRaw *as_libraw(RetinaLibRaw *h) { return reinterpret_cast<LibRaw *>(h); }
const LibRaw *as_libraw(const RetinaLibRaw *h) {
  return reinterpret_cast<const LibRaw *>(h);
}
} // namespace

extern "C" {

RetinaLibRaw *retina_libraw_new() {
  // Unlike every LibRaw call below (open_buffer/unpack/raw2image all catch their own exceptions
  // internally and return a status code, confirmed by reading their implementations), a plain
  // `new LibRaw()` has no such handler -- a hostile review caught that a `std::bad_alloc` (the
  // only plausible throw here) would unwind straight across this extern "C" boundary into Rust,
  // which is undefined behavior. OOM is already a bad day, but returning null (which every caller
  // already checks, see libraw_ffi.rs's `assert!(!ptr.is_null())`) is well-defined; an uncaught
  // C++ exception in Rust is not.
  try {
    return reinterpret_cast<RetinaLibRaw *>(new LibRaw());
  } catch (const std::bad_alloc &) {
    return nullptr;
  }
}

void retina_libraw_free(RetinaLibRaw *handle) { delete as_libraw(handle); }

RetinaStatus retina_libraw_decode_buffer(RetinaLibRaw *handle, const uint8_t *data,
                                          size_t len) {
  LibRaw *lr = as_libraw(handle);

  int status = lr->open_buffer(data, len);
  if (status != LIBRAW_SUCCESS) {
    return status;
  }
  status = lr->unpack();
  if (status != LIBRAW_SUCCESS) {
    return status;
  }
  status = lr->raw2image();
  return status;
}

const char *retina_strerror(RetinaStatus status) {
  return libraw_strerror(status);
}

uint16_t retina_nef_compression(const RetinaLibRaw *handle) {
  return as_libraw(handle)->imgdata.makernotes.nikon.NEFCompression;
}

uint16_t retina_raw_width(const RetinaLibRaw *handle) {
  return as_libraw(handle)->imgdata.sizes.raw_width;
}
uint16_t retina_raw_height(const RetinaLibRaw *handle) {
  return as_libraw(handle)->imgdata.sizes.raw_height;
}
uint16_t retina_iwidth(const RetinaLibRaw *handle) {
  return as_libraw(handle)->imgdata.sizes.iwidth;
}
uint16_t retina_iheight(const RetinaLibRaw *handle) {
  return as_libraw(handle)->imgdata.sizes.iheight;
}
uint16_t retina_top_margin(const RetinaLibRaw *handle) {
  return as_libraw(handle)->imgdata.sizes.top_margin;
}
uint16_t retina_left_margin(const RetinaLibRaw *handle) {
  return as_libraw(handle)->imgdata.sizes.left_margin;
}
unsigned retina_raw_pitch(const RetinaLibRaw *handle) {
  return as_libraw(handle)->imgdata.sizes.raw_pitch;
}

unsigned retina_filters(const RetinaLibRaw *handle) {
  return as_libraw(handle)->imgdata.idata.filters;
}
int retina_colors(const RetinaLibRaw *handle) {
  return as_libraw(handle)->imgdata.idata.colors;
}

unsigned retina_black(const RetinaLibRaw *handle) {
  return as_libraw(handle)->imgdata.color.black;
}
unsigned retina_maximum(const RetinaLibRaw *handle) {
  return as_libraw(handle)->imgdata.color.maximum;
}

void retina_cam_mul(const RetinaLibRaw *handle, float out[4]) {
  const float *src = as_libraw(handle)->imgdata.color.cam_mul;
  std::memcpy(out, src, sizeof(float) * 4);
}

void retina_rgb_cam(const RetinaLibRaw *handle, float out[12]) {
  const float(&src)[3][4] = as_libraw(handle)->imgdata.color.rgb_cam;
  std::memcpy(out, src, sizeof(float) * 12);
}

const char *retina_make(const RetinaLibRaw *handle) {
  return as_libraw(handle)->imgdata.idata.make;
}
const char *retina_model(const RetinaLibRaw *handle) {
  return as_libraw(handle)->imgdata.idata.model;
}

const uint16_t *retina_raw_image(const RetinaLibRaw *handle, size_t *out_len) {
  const LibRaw *lr = as_libraw(handle);
  const ushort *img = lr->imgdata.rawdata.raw_image;
  if (img == nullptr) {
    *out_len = 0;
    return nullptr;
  }
  *out_len = static_cast<size_t>(lr->imgdata.sizes.raw_pitch) / 2 *
             lr->imgdata.sizes.raw_height;
  return reinterpret_cast<const uint16_t *>(img);
}

// UNVERIFIED in this sandbox (no LibRaw submodule checked out here to compile/run against, see
// docs/research/calico-color-pipeline.md): this assumes calling dcraw_process() after
// retina_libraw_decode_buffer's raw2image() has already run is safe -- LibRaw's own public
// samples (e.g. simple_dcraw.cpp) only ever call one of unpack()+dcraw_process() OR
// unpack()+raw2image(), never both on the same decode, and no LibRaw documentation this pass
// found confirms the two don't interact (e.g. dcraw_process() detecting imgdata.image already
// populated by raw2image() and skipping its own demosaic-and-populate step). If a real run
// produces a garbage/all-zero linear image, this ordering is the first thing to suspect --
// verify against a real NEF before trusting `dump-linear`'s output.
RetinaStatus retina_libraw_process_linear(RetinaLibRaw *handle) {
  LibRaw *lr = as_libraw(handle);
  libraw_output_params_t &p = lr->imgdata.params;

  p.output_color = 0;     // "raw" -- no camera->working-space matrix applied
  p.gamm[0] = 1.0;         // linear, no tone curve
  p.gamm[1] = 1.0;
  p.no_auto_bright = 1;    // no auto-exposure adjustment
  p.output_bps = 16;
  p.highlight = 0;         // simple clip -- highlight reconstruction is a develop-stage concern
  p.half_size = 0;
  p.use_camera_wb = 0;
  p.use_auto_wb = 0;
  // Disables LibRaw's own cam_mul/pre_mul auto-selection entirely: every channel scaled by
  // exactly 1, so the only per-channel difference in the output is the demosaic itself plus the
  // black/white-level linear scaling every LibRaw decode applies regardless of settings. calico
  // applies WB from the as-shot multipliers (retina_cam_mul) itself, downstream.
  p.user_mul[0] = 1.0f;
  p.user_mul[1] = 1.0f;
  p.user_mul[2] = 1.0f;
  p.user_mul[3] = 1.0f;

  return lr->dcraw_process();
}

const uint16_t *retina_linear_image(const RetinaLibRaw *handle, size_t *out_len) {
  const LibRaw *lr = as_libraw(handle);
  const ushort(*img)[4] = lr->imgdata.image;
  if (img == nullptr) {
    *out_len = 0;
    return nullptr;
  }
  size_t pixels = static_cast<size_t>(lr->imgdata.sizes.iwidth) *
                  lr->imgdata.sizes.iheight;
  *out_len = pixels * 4;
  return reinterpret_cast<const uint16_t *>(img);
}

void retina_pre_mul(const RetinaLibRaw *handle, float out[4]) {
  const float *src = as_libraw(handle)->imgdata.color.pre_mul;
  std::memcpy(out, src, sizeof(float) * 4);
}

void retina_cam_xyz(const RetinaLibRaw *handle, float out[12]) {
  const float(&src)[4][3] = as_libraw(handle)->imgdata.color.cam_xyz;
  std::memcpy(out, src, sizeof(float) * 12);
}

void retina_cblack(const RetinaLibRaw *handle, uint32_t out[4]) {
  const unsigned *src = as_libraw(handle)->imgdata.color.cblack;
  out[0] = src[0];
  out[1] = src[1];
  out[2] = src[2];
  out[3] = src[3];
}

}
