# Installation

Prebuilt binaries are attached to every [release](https://github.com/KimiaLabMayo/ThinSlide/releases/latest).
Download the one for your platform, make it executable, and put it on your `PATH`:

| Platform | Asset | Includes GUI | Dependencies |
|----------|-------|:---:|---|
| Linux x86_64 | `thinslide-linux-x86_64-musl` | — | none (static musl) |
| macOS arm64 | `thinslide-macos-arm64` | ✓ | none (static) |
| Windows x86_64 | `thinslide-windows-x86_64.exe` | ✓ | none (static) |

```sh
# Linux / macOS
curl -L -o thinslide https://github.com/KimiaLabMayo/ThinSlide/releases/latest/download/thinslide-linux-x86_64-musl
chmod +x thinslide
sudo mv thinslide /usr/local/bin/
```

On Windows, download `thinslide-windows-x86_64.exe` and add its folder to `PATH`.

## From crates.io or source

Requires a [Rust toolchain](https://rustup.rs) (edition 2024) and the **development**
headers for [libtiff](http://www.libtiff.org/) and [Little CMS 2](https://www.littlecms.com/):

```sh
brew install libtiff little-cms2          # macOS
sudo apt install libtiff-dev liblcms2-dev # Debian / Ubuntu
sudo dnf install libtiff-devel lcms2-devel # Fedora / RHEL
```

Install from crates.io:

```sh
cargo install thinslide
```

Or build from source:

```sh
git clone https://github.com/KimiaLabMayo/ThinSlide.git
cd ThinSlide

# CLI only
cargo build --release --bin thinslide

# CLI + GUI
cargo build --release

# Binaries are placed at target/release/thinslide (and thinslide-gui)
```
