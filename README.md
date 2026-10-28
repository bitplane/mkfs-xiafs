# mkfs-xiafs

Create xiafs filesystem images in Rust.

Xiafs was an early Linux filesystem (1993–1997) by Q. Frank Xia, derived
from Minix and competing with ext2. Modern kernels can mount it via
[modern-xiafs][modxia].

[modxia]: https://github.com/jeremiahsnapp/modern-xiafs

## Install

From crates.io:

```bash
cargo install --locked mkfs-xiafs
```

This installs a binary called `mkfs-xiafs`. Pre-built binaries for Linux
(x86_64 / aarch64), macOS (universal), and Windows are also attached to
each [GitHub release][releases].

[releases]: https://github.com/bitplane/mkfs-xiafs/releases

## Usage

```bash
# Create an empty 4 MiB image.
mkfs-xiafs out.xiafs 4

# Populate from a directory tree (recursive — subdirectories are preserved).
mkfs-xiafs -d ./payload out.xiafs 4

# Mount on Linux (requires the modern-xiafs kernel module).
sudo mount -o loop,ro -t xiafs out.xiafs /mnt
```

## Limits

- Population supports regular files and directories; symlinks and special files
  are rejected with an error.
- Filenames must be UTF-8 and at most 248 bytes.
- Hard links are imported as separate file copies.
- 8 direct + 1 single-indirect zone slots → max file ≈ 264 KB.
- Image sizes are 1–4095 MiB (the writer currently limits images to below 4 GiB).
  The complete image is held in memory during creation.
- Directories are direct-only (no indirect for directory blocks).

## License

WTFPL with one extra clause: don't blame me. See [LICENSE](LICENSE).
