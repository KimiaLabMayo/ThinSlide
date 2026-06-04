fn main() {
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // Highest priority: explicit env var (used by CI for musl/Windows builds).
    if let Ok(lib_dir) = std::env::var("TIFF_LIB_DIR") {
        println!("cargo:rustc-link-search=native={lib_dir}");
        if std::env::var("TIFF_STATIC").is_ok() {
            println!("cargo:rustc-link-lib=static=tiff");
            // libtiff's ZIP and PixarLog codecs depend on zlib; link it explicitly.
            let zlib_dir = std::env::var("ZLIB_LIB_DIR").unwrap_or(lib_dir);
            println!("cargo:rustc-link-search=native={zlib_dir}");
            if target_os == "windows" {
                // vcpkg names it `zlib.lib`, upstream CMake `zlibstatic.lib`;
                // let CI report the actual name via ZLIB_LIB_NAME.
                let zlib_name =
                    std::env::var("ZLIB_LIB_NAME").unwrap_or_else(|_| "zlib".to_string());
                println!("cargo:rustc-link-lib=static={zlib_name}");
            } else {
                println!("cargo:rustc-link-lib=static=z");
            }
        } else {
            println!("cargo:rustc-link-lib=tiff");
        }
        return;
    }

    // pkg-config (Linux glibc and macOS with pkg-config installed).
    if target_env != "musl" && target_env != "msvc" {
        if pkg_config::probe_library("libtiff-4").is_ok() {
            return;
        }
    }

    // Fallback: Homebrew on Apple Silicon / Intel macOS.
    if target_os == "macos" {
        for path in &["/opt/homebrew/lib", "/usr/local/lib"] {
            if std::path::Path::new(path).exists() {
                println!("cargo:rustc-link-search=native={path}");
                break;
            }
        }
        println!("cargo:rustc-link-lib=tiff");
        return;
    }

    panic!("libtiff not found. Set TIFF_LIB_DIR (and TIFF_STATIC for static linkage).");
}