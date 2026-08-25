//! Check the libvpx setup before the compiler has to.
//!
//! bindgen fails quietly. When clang cannot find the libvpx headers it does not
//! stop — it emits incomplete types, and `vpx_codec_enc_cfg` arrives with a
//! single `_address` field. The build then dies in fifteen "no field `g_w` on
//! type" errors that say nothing about the actual cause, which is an include
//! path.
//!
//! This runs after env-libvpx-sys has already produced its bindings, so it
//! cannot prevent that. What it can do is put one sentence naming the real
//! problem directly above the wall of errors.

use std::path::Path;

fn main() {
    for var in ["VPX_LIB_DIR", "VPX_INCLUDE_DIR", "VPX_VERSION", "LIBCLANG_PATH"] {
        println!("cargo:rerun-if-env-changed={var}");
    }

    // Nothing to check when the encoder is not being built.
    if std::env::var_os("CARGO_FEATURE_VPX").is_none() {
        return;
    }

    let mut problems = Vec::new();

    match std::env::var_os("VPX_INCLUDE_DIR") {
        None => problems.push(
            "VPX_INCLUDE_DIR не задана. Без неё bindgen запускается без путей \
             поиска, не находит vpx/vpx_encoder.h и молча выдаёт пустые типы"
                .to_owned(),
        ),
        Some(dir) => {
            let header = Path::new(&dir).join("vpx").join("vpx_encoder.h");
            if !header.exists() {
                problems.push(format!(
                    "не найден {}. VPX_INCLUDE_DIR должна указывать на каталог, \
                     ВНУТРИ которого лежит подкаталог vpx/",
                    header.display()
                ));
            }
        }
    }

    match std::env::var_os("VPX_LIB_DIR") {
        None => problems.push("VPX_LIB_DIR не задана".to_owned()),
        Some(dir) => {
            let dir = Path::new(&dir);
            // env-libvpx-sys links `static=libvpx` on Windows, so this exact
            // name is what matters — not the import library `vpx.lib` that sits
            // next to it in the prebuilt archives.
            let names = ["libvpx.lib", "libvpx.a", "vpx.lib"];
            if !names.iter().any(|n| dir.join(n).exists()) {
                problems.push(format!(
                    "в {} нет ни одного из {names:?}",
                    dir.display()
                ));
            }
        }
    }

    if std::env::var_os("VPX_VERSION").is_none() {
        problems.push(
            "VPX_VERSION не задана. Она уходит в проверку ABI при инициализации \
             кодера, и должна совпадать с установленной libvpx"
                .to_owned(),
        );
    }

    if problems.is_empty() {
        return;
    }

    eprintln!("\n=== libvpx настроена неправильно ===");
    for p in &problems {
        eprintln!("  · {p}");
    }
    eprintln!(
        "\nЕсли пути верны, а типы всё равно пустые — clang не нашёл системные \n\
         заголовки Windows. Соберите из Developer PowerShell for VS 2022.\n\
         Подробно — раздел «Кодирование: libvpx» в README.\n"
    );
    panic!("libvpx настроена неправильно, см. выше");
}
