// build.rs
fn main() {
    // Try pkg-config first (works on Linux and macOS with pkg-config installed).
    if pkg_config::probe_library("libtiff-4").is_ok() {
        return;
    }

    // Fallback: Homebrew on Apple Silicon / Intel macOS.
    for path in &["/opt/homebrew/lib", "/usr/local/lib"] {
        if std::path::Path::new(path).exists() {
            println!("cargo:rustc-link-search=native={}", path);
            break;
        }
    }
    println!("cargo:rustc-link-lib=tiff");
}