# Building Axmzip from Source

## Prerequisites

Install Rust (one command, works on Windows and Linux):
```
https://rustup.rs
```
Then restart your terminal. Verify with:
```
rustc --version   # should print 1.75+
cargo --version
```

---

## Build (all platforms)

```bash
# Clone / enter the project
cd axmzip

# Build everything in release mode
cargo build --release --workspace

# Binaries will be in:
#   target/release/axmzip          (CLI — Linux)
#   target/release/axmzip.exe      (CLI — Windows)
#   target/release/axmzip-gui      (GUI — Linux)
#   target/release/axmzip-gui.exe  (GUI — Windows)
```

---

## Run tests

```bash
cargo test --workspace
```

---

## Cross-compile Windows → Linux or Linux → Windows

Install the cross-compilation target:
```bash
# On Linux, build for Windows:
rustup target add x86_64-pc-windows-gnu
cargo build --release --workspace --target x86_64-pc-windows-gnu

# On Windows, build for Linux:
rustup target add x86_64-unknown-linux-gnu
cargo build --release --workspace --target x86_64-unknown-linux-gnu
```

---

## CLI Usage

```bash
# Compress (lossless)
./axmzip compress input.bin output.axm

# Compress (lossy, quality 90, RGB image)
./axmzip compress photo.raw photo.axm --quality 90 --channels 3

# Decompress
./axmzip decompress output.axm recovered.bin

# Info — inspect a compressed file without decompressing
./axmzip info output.axm

# Benchmark — try all quality levels on a file
./axmzip bench sensor_data.bin
```

---

## GUI Usage

Just run `axmzip-gui` (or `axmzip-gui.exe` on Windows).

- **Drag and drop** any file onto the window, OR click the drop zone to browse
- Click **Compress** or **Decompress**
- Click **Advanced options** to:
  - Adjust quality (100 = lossless, lower = smaller file)
  - Set channel count (1=gray, 3=RGB, 4=RGBA)
- Click **Open output folder** after compression to find the output file

---

## Release checklist

- [ ] Replace `yourusername` in GUI footer and Cargo.toml repository field
- [ ] Test on clean Windows machine (no Rust installed — binary should run standalone)
- [ ] Test on clean Linux machine
- [ ] Upload both binaries to GitHub Releases
- [ ] Tag the release: `git tag v0.5.0 && git push --tags`

---

## Project structure

```
axmzip/
├── Cargo.toml          ← workspace root
├── core/               ← compression library (no runtime deps)
│   ├── Cargo.toml
│   └── src/lib.rs      ← full v5 algorithm in Rust
├── cli/                ← command-line tool
│   ├── Cargo.toml
│   └── src/main.rs
├── gui/                ← egui desktop app
│   ├── Cargo.toml
│   └── src/main.rs
├── README.md
├── PAPER.md
└── LICENSE
```
