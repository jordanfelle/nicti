//! Compiles the vendored LibRaw fork (`vendor/LibRaw`, git submodule pinned to
//! yogthos/LibRaw#nikon-he-decoder) plus `shim.cpp` directly with the `cc` crate -- no
//! `libraw-sys`/bindgen. See shim.h for why. Only the sources `lib_libraw_a_SOURCES` names in
//! LibRaw's own `Makefile.am` are compiled (list copied by hand below, not parsed from the
//! Makefile -- if this ever drifts from upstream's list, `cargo build` still succeeds but a
//! decoder path could silently be missing; the sweep's 100%-decode-rate check is what would
//! catch that in practice). None of these sources need libjpeg/zlib/lcms2 (checked: every
//! external-lib `#include` in the tree is behind a `USE_JPEG`/etc guard this build never
//! defines), so there's nothing to vendor beyond LibRaw's own C++ and this shim.

use std::path::Path;

const LIBRAW_DIR: &str = "vendor/LibRaw";

// Copied from vendor/LibRaw/Makefile.am's `lib_libraw_a_SOURCES` (the non-reentrant variant's
// source list -- retina builds the reentrant semantics instead, see the `-pthread`/no-NOTHREADS
// note below, but the file list itself is identical to `lib_libraw_r_a_SOURCES`).
const SOURCES: &[&str] = &[
    "src/libraw_c_api.cpp",
    "src/libraw_datastream.cpp",
    "src/decoders/canon_600.cpp",
    "src/decoders/crx.cpp",
    "src/decoders/pana8.cpp",
    "src/decoders/decoders_dcraw.cpp",
    "src/decoders/sonycc.cpp",
    "src/decompressors/losslessjpeg.cpp",
    "src/decoders/decoders_libraw_dcrdefs.cpp",
    "src/decoders/nikon_he_decoder.cpp",
    "src/decoders/nikon_he/nikon_he_bayer.cpp",
    "src/decoders/nikon_he/nikon_he_bit_reader.cpp",
    "src/decoders/nikon_he/nikon_he_coefficient_decode.cpp",
    "src/decoders/nikon_he/nikon_he_decode.cpp",
    "src/decoders/nikon_he/nikon_he_dequantize.cpp",
    "src/decoders/nikon_he/nikon_he_gcli_decode.cpp",
    "src/decoders/nikon_he/nikon_he_gtli_table.cpp",
    "src/decoders/nikon_he/nikon_he_idwt_horizontal.cpp",
    "src/decoders/nikon_he/nikon_he_idwt_vertical.cpp",
    "src/decoders/nikon_he/nikon_he_precinct_decode.cpp",
    "src/decoders/nikon_he/nikon_he_precinct_header.cpp",
    "src/decoders/nikon_he/nikon_he_picture_header.cpp",
    "src/decoders/nikon_he/nikon_he_predecessor.cpp",
    "src/decoders/nikon_he/nikon_he_predict_lut.cpp",
    "src/decoders/nikon_he/nikon_he_subband_config.cpp",
    "src/decoders/nikon_he/nikon_he_tile.cpp",
    "src/decoders/olympus14.cpp",
    "src/decoders/decoders_libraw.cpp",
    "src/decoders/dng.cpp",
    "src/decoders/fp_dng.cpp",
    "src/decoders/fuji_compressed.cpp",
    "src/decoders/generic.cpp",
    "src/decoders/kodak_decoders.cpp",
    "src/decoders/load_mfbacks.cpp",
    "src/decoders/smal.cpp",
    "src/decoders/unpack_thumb.cpp",
    "src/decoders/unpack.cpp",
    "src/demosaic/aahd_demosaic.cpp",
    "src/demosaic/ahd_demosaic.cpp",
    "src/demosaic/dcb_demosaic.cpp",
    "src/demosaic/dht_demosaic.cpp",
    "src/demosaic/misc_demosaic.cpp",
    "src/demosaic/xtrans_demosaic.cpp",
    "src/integration/dngsdk_glue.cpp",
    "src/integration/rawspeed_glue.cpp",
    "src/metadata/adobepano.cpp",
    "src/metadata/canon.cpp",
    "src/metadata/ciff.cpp",
    "src/metadata/cr3_parser.cpp",
    "src/metadata/epson.cpp",
    "src/metadata/exif_gps.cpp",
    "src/metadata/fuji.cpp",
    "src/metadata/identify_tools.cpp",
    "src/metadata/identify.cpp",
    "src/metadata/kodak.cpp",
    "src/metadata/leica.cpp",
    "src/metadata/makernotes.cpp",
    "src/metadata/mediumformat.cpp",
    "src/metadata/minolta.cpp",
    "src/metadata/misc_parsers.cpp",
    "src/metadata/nikon.cpp",
    "src/metadata/normalize_model.cpp",
    "src/metadata/olympus.cpp",
    "src/metadata/hasselblad_model.cpp",
    "src/metadata/p1.cpp",
    "src/metadata/pentax.cpp",
    "src/metadata/samsung.cpp",
    "src/metadata/sony.cpp",
    "src/metadata/tiff.cpp",
    "src/postprocessing/aspect_ratio.cpp",
    "src/postprocessing/dcraw_process.cpp",
    "src/postprocessing/mem_image.cpp",
    "src/postprocessing/postprocessing_aux.cpp",
    "src/postprocessing/postprocessing_utils_dcrdefs.cpp",
    "src/postprocessing/postprocessing_utils.cpp",
    "src/preprocessing/ext_preprocess.cpp",
    "src/preprocessing/raw2image.cpp",
    "src/preprocessing/subtract_black.cpp",
    "src/tables/cameralist.cpp",
    "src/tables/colorconst.cpp",
    "src/tables/colordata.cpp",
    "src/tables/wblists.cpp",
    "src/utils/curves.cpp",
    "src/utils/decoder_info.cpp",
    "src/utils/init_close_utils.cpp",
    "src/utils/open.cpp",
    "src/utils/phaseone_processing.cpp",
    "src/utils/read_utils.cpp",
    "src/utils/thumb_utils.cpp",
    "src/utils/utils_dcraw.cpp",
    "src/utils/utils_libraw.cpp",
    "src/write/apply_profile.cpp",
    "src/write/file_write.cpp",
    "src/write/tiff_writer.cpp",
    "src/x3f/x3f_parse_process.cpp",
    "src/x3f/x3f_utils_patched.cpp",
];

fn main() {
    let libraw = Path::new(LIBRAW_DIR);
    if !libraw.join("libraw/libraw.h").exists() {
        panic!(
            "vendor/LibRaw submodule not checked out -- run `git submodule update --init \
             spikes/retina/vendor/LibRaw` (retina is a spike crate, not part of the default \
             build; see CLAUDE.md's CI path-gating note for retina)"
        );
    }

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .include(libraw)
        .include(libraw.join("libraw"))
        .file("shim.cpp")
        .warnings(false) // upstream LibRaw itself builds with `-w`, see Makefile.am
        .flag_if_supported("-std=c++17");

    for src in SOURCES {
        build.file(libraw.join(src));
    }

    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "x86_64-pc-windows-gnu" {
        // Without this, `cc` bakes in a bare `-lstdc++` (dynamic) *before* the
        // `-static-libstdc++`/`-static-libgcc` flags this build.rs adds below get a chance to
        // apply -- `ld` resolves `-l` flags in command order, so the later static request loses
        // to the earlier dynamic one. Confirmed by a real WSL-interop run of the resulting .exe:
        // it failed immediately (exit code 53, "module not found") until this was disabled and
        // the static flags took over the C++ runtime link entirely.
        build.cpp_link_stdlib(None);
    }

    // Reentrant semantics (matches Makefile.am's `libraw_r` variant, not the NOTHREADS one):
    // retina decodes files concurrently across rayon threads, each with its own `LibRaw`
    // instance, so the internal locking NOTHREADS disables must stay enabled.
    if !target.contains("windows") {
        build.flag("-pthread");
    }
    // Both Windows toolchains' <cmath> gate M_PI/M_SQRT1_2 etc. behind _USE_MATH_DEFINES (MinGW's
    // libstdc++ does too, not just MSVC's STL) -- LibRaw uses them assuming GCC/Clang's default
    // (unguarded) behavior on Linux, which doesn't hold on either Windows target. Confirmed by a
    // real cross-compile failure: decoders_dcraw.cpp's ljpeg_idct fails on x86_64-pc-windows-gnu
    // without this.
    if target.contains("windows") {
        build.define("_USE_MATH_DEFINES", None);
        // `libraw.h`'s public API is declared `DllDef` (libraw_types.h: `__declspec(dllexport)`
        // when LIBRAW_BUILDLIB is set, `dllimport` otherwise), for LibRaw's own DLL build. retina
        // links LibRaw's C++ source directly into a static archive, never as a DLL -- without
        // this, MSVC hard-errors ("definition of dllimport function not allowed", confirmed by a
        // real windows-latest CI run) since it refuses to *define* a function declared
        // `dllimport`. MinGW's `-w`-suppressed build tolerated the mismatch silently, which is
        // exactly why this got missed by local cross-compile testing and only surfaced on the
        // real MSVC job.
        build.define("LIBRAW_NODLL", None);
    }
    if target.contains("msvc") {
        build.define("_CRT_SECURE_NO_WARNINGS", None);
    }

    build.compile("retina_libraw");

    // MinGW dynamically links libstdc++/libgcc/winpthread by default -- a real cross-compiled
    // .exe run via WSL interop failed with no error message (exit code 53, "no such DLL") until
    // this was fixed, because none of those runtime DLLs exist on a stock Windows install.
    //
    // Two dead ends tried first, kept here as a note since both look plausible and aren't:
    // (1) `-static-libgcc`/`-static-libstdc++` as `cargo:rustc-link-arg` values -- these are
    //     gcc/g++-*frontend* flags, but rustc's linker-driver invocation for this target is plain
    //     `gcc`, not `g++` (confirmed via `cargo rustc -- --print link-args`), and plain gcc's
    //     default specs never link libstdc++ at all (that's a g++-only default), so there was
    //     nothing for these flags to substitute -- no effect either way.
    // (2) `cc::Build::cpp_link_stdlib(None)` (stops `cc` emitting its own dynamic `-lstdc++`)
    //     combined with the same two flags -- confirmed the flags really do nothing on their own
    //     (real link failure: `undefined reference to operator new` once the automatic
    //     `-lstdc++` was removed and nothing replaced it).
    // Fix: explicitly link the *static* archives by exact filename (`-l:foo.a`), which works
    // regardless of which frontend drives the link and needs no automatic-linking behavior from
    // either gcc or g++.
    if target == "x86_64-pc-windows-gnu" {
        println!("cargo:rustc-link-arg=-l:libstdc++.a");
        println!("cargo:rustc-link-arg=-l:libgcc.a");
        println!("cargo:rustc-link-arg=-l:libgcc_eh.a");
        println!("cargo:rustc-link-arg=-l:libwinpthread.a");
        // libstdc++.a itself needs CRT/kernel32 symbols (sprintf, FormatMessageA, LocalFree,
        // getenv, fputs, rand_s, read) that appear *earlier* in rustc's own link command (before
        // `-nodefaultlibs`) -- `ld` doesn't rescan earlier archives for a later object's needs,
        // so they have to be repeated after our static libs too.
        println!("cargo:rustc-link-arg=-lmsvcrt");
        println!("cargo:rustc-link-arg=-lmingwex");
        println!("cargo:rustc-link-arg=-lmingw32");
        println!("cargo:rustc-link-arg=-lkernel32");
        println!("cargo:rustc-link-arg=-ladvapi32");
    }

    // Printing ANY `cargo:rerun-if-changed` opts this build script out of Cargo's default
    // "rerun if anything in the package changes" behavior -- it switches to watching *only* the
    // paths named here (confirmed: `cc::Build` itself never emits its own rerun-if-changed for
    // the ~95 files passed to `.file()` above, so without this fix, editing a vendored LibRaw
    // source -- e.g. syncing a future upstream fix into the submodule -- would silently not
    // trigger a rebuild, caught by CodeRabbit). Watching the submodule directory itself (not each
    // of the 95 files individually) is enough -- Cargo watches a named directory recursively.
    println!("cargo:rerun-if-changed=shim.cpp");
    println!("cargo:rerun-if-changed=shim.h");
    println!("cargo:rerun-if-changed={}", libraw.display());
}
