// build.rs
fn main() {
    // 1. Homebrewのライブラリパスを検索対象に追加
    println!("cargo:rustc-link-search=native=/opt/homebrew/lib");

    // 2. 「tiff」という名前のライブラリをリンクすることを指示
    println!("cargo:rustc-link-lib=tiff");
}