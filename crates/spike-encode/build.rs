//! Point the linker at libvpx.
//!
//! Replaces env-libvpx-sys, which needed bindgen, which needed LLVM, which
//! needed a Visual Studio environment for clang to find `stdint.h`. All that
//! was in service of generating declarations for nine functions — which are now
//! written out in src/vpx/ffi.rs instead.
//!
//! What is left is two lines of linker configuration and a check that the
//! library is actually where it was said to be.

use std::path::Path;

fn main() {
    for var in ["VPX_LIB_DIR", "VPX_STATIC"] {
        println!("cargo:rerun-if-env-changed={var}");
    }

    // Nothing to link when the encoder is not being built.
    if std::env::var_os("CARGO_FEATURE_VPX").is_none() {
        return;
    }

    let Some(lib_dir) = std::env::var_os("VPX_LIB_DIR") else {
        panic!(
            "\n\nVPX_LIB_DIR не задана — не сказано, где искать libvpx.\n\
             См. раздел «Кодирование: libvpx» в README.\n"
        );
    };
    let dir = Path::new(&lib_dir);

    // Prebuilt Windows SDKs ship the static library as libvpx.lib and an import
    // library for the DLL as vpx.lib beside it. The static one is what we want,
    // so it is looked for first.
    let candidates: [(&str, &str); 3] =
        [("libvpx.lib", "libvpx"), ("libvpx.a", "vpx"), ("vpx.lib", "vpx")];
    let Some((file, link_name)) = candidates.iter().find(|(f, _)| dir.join(f).exists()) else {
        panic!(
            "\n\nВ {} нет libvpx: искали libvpx.lib, libvpx.a, vpx.lib.\n\
             VPX_LIB_DIR должна указывать на каталог с самой библиотекой —\n\
             в готовых сборках это lib\\x64, а не корень архива.\n",
            dir.display()
        );
    };

    println!("cargo:rustc-link-search=native={}", dir.display());
    let dynamic = std::env::var_os("VPX_DYNAMIC").is_some();
    if dynamic {
        println!("cargo:rustc-link-lib={link_name}");
    } else {
        println!("cargo:rustc-link-lib=static={link_name}");
    }
    eprintln!("libvpx: {} из {}", file, dir.display());
}
