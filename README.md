# FerrumRecovery

[![Build](https://github.com/dyrb/ferrumrecovery/actions/workflows/ci.yml/badge.svg)](https://github.com/dyrb/ferrumrecovery/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/dyrb/ferrumrecovery?include_prereleases)](https://github.com/dyrb/ferrumrecovery/releases)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
![Platform](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078D6)
![Rust](https://img.shields.io/badge/made%20with-Rust-B7410E)

Deleted-file recovery for Windows, written in Rust with a native Win32 interface and
command-line tools. Reads NTFS, FAT12/16/32 and exFAT from volumes, physical disks and
images. This first public release is a **beta**.

[Русская версия](README.ru.md)

![Recovery results and image preview](docs/recovery-en.png)

## Features

- Deleted NTFS records, resident data, fragmented and sparse streams, LZNT1 compression,
  attribute lists and alternate data streams.
- Deleted FAT and exFAT directory entries, disconnected folders and surviving structures
  from older file systems.
- Recycle Bin original names and paths, NTFS change-journal and directory-index traces,
  and existing Windows shadow-copy sources.
- MBR/GPT partition discovery, including backup tables and file-system boot sectors.
- Signature search for images, documents, archives, audio and video, including CAB and
  PCX/DCX. Custom signatures are available through the CLI.
- Folder tree, virtual file list, masks, type/date/size/state filters, checked selection,
  CSV export and saved scans.
- Image, text and hex previews. Windows decoders handle PNG, JPEG, BMP, GIF, TIFF and
  ICO; multi-frame formats show their first frame. Image input is limited to 64 MB and
  100 million pixels; text and hex show the first 1 MB.
- Resumable source imaging, scan pause/cancellation, English and Russian, system light
  and dark themes.
- Portable GUI and CLI. Preferences are saved in `FerrumRecovery.ini` beside the program.
  Images, progress maps, saved scans and reports are written only to chosen destinations.

The screenshot uses a synthetic disk image with sample documents and pictures.

## Usage

Download the ZIP from [Releases](https://github.com/dyrb/ferrumrecovery/releases), extract
it and run `ferrumrecovery-gui.exe`. Requires Windows 10 or 11, x64. The GUI requests
administrator rights; the CLI needs elevation for direct disk access.

1. Choose a volume, physical disk or image. Raw images and fixed VHD are supported.
2. Select a mode in **View > Search mode**, then press **Scan**.
3. Select or check files, press **Recover** and choose a folder on another volume.

| Mode | Reads |
| --- | --- |
| Quick | File-system metadata |
| Deep | Metadata and free space, including signature search |
| Full | Metadata and the entire selected range, including allocated space and old structures |

For partitioned disks, choose one partition or the entire source. Full search reads the
whole selected range and then resolves candidates; elapsed time includes these stages.

Checked files take precedence over selected rows. Filters retain checkmarks. Existing
output files are never overwritten. **View** contains language, theme, preview mode and
analysis filters. **File > Create source image** makes an image with a resumable `.map`.

Source handles are opened read-only. Store the program and recovered files on another
drive. For damaged media, work from an image on a healthy disk. There is no automatic
scan cache, monitoring, telemetry or application log.

### Command line

Replace the source and output paths with your own. In these examples `disk1` is the
source and `D:` is a separate destination volume.

```powershell
.\ferrumrecovery.exe disks
.\ferrumrecovery.exe partitions disk1
.\ferrumrecovery.exe scan disk1 --part 1 --full --check --save D:\scan.frs
.\ferrumrecovery.exe scan D:\scan.frs --name "*.jpg" --limit 0
.\ferrumrecovery.exe recover D:\scan.frs --name "*.jpg" --to D:\recovered
.\ferrumrecovery.exe image disk1 --to D:\disk1.img
```

Run `ferrumrecovery.exe --help` for all commands and filters. Saved scans can be opened
without repeating the scan; preview and recovery still require the original source.

## Recovery limits

- **Good** means no detected allocation conflict under the available metadata. Sampled
  content checks do not verify every byte. An SSD may already have discarded deleted
  data; unreadable ranges and zeros are reported, not reconstructed.
- Counts include candidates, overlapping alternatives and text fragments. They are not
  counts of unique, intact recoverable files. Raw signature matches have unknown deletion
  status. Text fragments can come from existing files and lack original boundaries.
- Signature carving assumes contiguous content. Fragmented recovery needs surviving
  metadata. Deep tests cluster boundaries; Full tests 512-byte boundaries. Other start
  alignments can be missed. Some formats use estimated ends or remain unsupported.
- Historical reconstruction depends on surviving structures and bounded geometry
  hypotheses. Candidate limits are reported. Unknown allocation data stays **Unknown**.
  FAT deletion often destroys cluster chains, so contiguous recovery may be incomplete.
- **Lost files** have no reliable original parent path. This does not determine the
  condition of their data. A live source can change after a scan or after saving results.
- CAB recovery retains the cabinet container; it does not reconstruct missing volumes
  or repair damaged compressed members.
- EFS decryption, ReFS, RAID assembly and Linux/macOS file systems are not implemented.
  Dynamic VHD and VHDX must be attached through Windows first. Recovery on failing
  physical media has not been validated for this release.

## Building from source

1. Install [Build Tools for Visual Studio](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
   with **Desktop development with C++**, the Windows SDK and MSVC linker.
2. Install Rust through [rustup.rs](https://rustup.rs) using the MSVC toolchain. Rust 1.88
   or newer is required.
3. Download the source ZIP or clone this repository, then run in its folder:

   ```powershell
   cargo build --release --workspace --locked
   ```

The executables appear in `target\release`. `scripts\build-release.ps1` builds a portable
ZIP and SHA-256 checksum in `dist`, excluding personal settings and build-machine paths.
All engine components and test fixtures needed by the normal test suite are in this
repository. External crates are `windows-sys`, `windows-link` and `memchr`.

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Tests include synthetic filesystem images, parser mutation cases, cancellation, imaging,
output-path checks and byte comparisons. Two hardware fault tests are ignored by default;
they require explicitly prepared disposable VHDs and the guard scripts in `scripts`.

## Architecture

`fr-core` handles read-only I/O, data streams, signature carving, saved scans, imaging and
recovery. `fr-ntfs` and `fr-fat` parse file systems; `fr-scan` coordinates scan stages and
merges findings. The GUI and CLI share these components. Full search overlaps queued
reads and parsing with bounded reusable buffers, then resolves file boundaries.

## Credits

Developed by [dyrb](https://github.com/dyrb), with contributions from Claude and Codex.
Disk-reading, NTFS and interface code is adapted from
[FerrumTree](https://github.com/dyrb/ferrumtree).
See also [FerrumCompare](https://github.com/dyrb/ferrumcompare).

## License

Licensed under either [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.

Third-party license notices: [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
