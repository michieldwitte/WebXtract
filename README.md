# WebXtract

[https://michieldwitte.github.io/WebXtract/](https://michieldwitte.github.io/WebXtract/)

WebXtract extracts files from archives, installers, packages, and disk images in your browser. It uses Rust compiled to WebAssembly, with extraction work running in a Web Worker.

**Your files stay on your device.** The selected file is processed locally and is not uploaded to a server. The website does need to download its WebAssembly module when it loads.

## Features

- Choose a file or drop it into the page, then inspect and download extracted files.
- Download individual files or bundle all results into a ZIP while keeping their folder paths.
- Set a total extraction output limit from 64 MB to 3 GB (1 GB by default).
- View progress and diagnostic messages in the browser console.
- Works as a static website; no application server or backend is needed.

## Supported formats

Support is best-effort and varies by format. Some inputs may contain unsupported compression, encryption, or installer-specific features.

- **Archives and packages:** ZIP, TAR and compressed TAR, 7Z, RAR, ARJ, CAB, LHA/LZH, ZOO, ARC, ALZ, EGG, LZ4, Unix compress, Lzip/LZMA, `ar`, cpio, xar, Debian `.deb` (application payload), and ZIP-based packages.
- **Installers:** NSIS, Inno Setup, PyInstaller (top-level CArchive entries), WiX Burn bundles, Windows Installer `.msi` and `.msm`, Windows Update `.msu`, and directly embedded CAB payloads in `.msp` patches. ZIP/7Z/ARJ self-extracting executables are also recognized.
- **Disk images and filesystems:** ISO9660, UDF, AppImage (Type 2), DMG, WIM, VHD/VHDX, QCOW2, VMDK, FAT/exFAT/NTFS, ext2/3/4, SquashFS, cramfs, romfs, EROFS, HFS+, and APFS.
- **Other containers and data:** PDF attachments and supported page images (JPEG and 8-bit grayscale/RGB), Android boot images (header versions 0–4), CHM, OLE2, MIME/TNEF, HWP3, RTF, SWF, AutoIt, SZDD/KWAJ, BinHex, uuencode, LNK, Python `.pyc`, OneNote, and selected static PE packers (UPX, Petite, FSG, NsPack).

## Rust libraries

Special thanks to these libraries.

- [cab](https://crates.io/crates/cab) — Microsoft Cabinet archives.
- [exav-unpack](https://crates.io/crates/exav-unpack) — additional archive, installer, disk image, and filesystem formats.
- [flate2](https://crates.io/crates/flate2) — DEFLATE compression support.
- [goblin](https://crates.io/crates/goblin) — executable and binary parsing.
- [gpt_disk_io](https://crates.io/crates/gpt_disk_io) — GPT disk image structures.
- [innospect](https://crates.io/crates/innospect) — Inno Setup installers.
- [iso9660-rs](https://crates.io/crates/iso9660-rs) — ISO9660 filesystem images.
- [js-sys](https://crates.io/crates/js-sys) — JavaScript standard library bindings for WebAssembly.
- [lopdf](https://crates.io/crates/lopdf) — PDF parsing and embedded content.
- [msi](https://crates.io/crates/msi) — Windows Installer databases and packages.
- [nsis](https://crates.io/crates/nsis) — NSIS installers.
- [quick-xml](https://crates.io/crates/quick-xml) — XML parsing.
- [unarc-rs](https://crates.io/crates/unarc-rs) — archive extraction.
- [wasm-bindgen](https://crates.io/crates/wasm-bindgen) — Rust and JavaScript WebAssembly bindings.
- [web-sys](https://crates.io/crates/web-sys) — browser Web APIs.
- [squashfs_reader](https://github.com/Macdu/squashfs_reader) — vendored SquashFS reader, built with the `only_rust` feature.

### Known limitations

- Password-protected Inno Setup installers are not supported.
- PyInstaller embedded PYZ modules are listed as archive entries but are not decoded.
- AppImage symlinks are skipped; regular files are extracted.
- `.deb` extraction returns the `data.tar.*` application payload, not package metadata as files.
- `.msp` extraction returns directly embedded CAB payloads. Patch transforms and binary deltas are not reconstructed.
- InstallShield `.z` archives are supported, but InstallScript CAB and InstallShield MSI payloads are only recognized, not extracted. ACE and StuffIt are recognized but unsupported.
- PDF support is for embedded attachments and selected embedded images; WebXtract is not a PDF viewer, and it does not extract text or inline images.
- The extraction output limit caps extracted data, not all browser memory use. The input file and browser/WASM overhead also consume memory.
- WebXtract extracts files; it does not run installers or emulate Windows programs.

## Run locally

Install Rust, the `wasm32-unknown-unknown` target, and [`wasm-pack`](https://rustwasm.github.io/wasm-pack/installer/). From the project directory:

```sh
rustup target add wasm32-unknown-unknown
wasm-pack build --target web --out-dir pkg --no-opt
python3 -m http.server 8000
```

Open <http://localhost:8000>. Serve the page over HTTP; browser ES modules and WebAssembly do not load correctly from `file://` URLs.

## Host on GitHub Pages

The repository includes a GitHub Actions workflow at `.github/workflows/deploy-pages.yml`. It builds the WASM module and deploys the static site whenever code is pushed to `main`, and supports manual runs from the Actions tab.

1. Push the repository to GitHub.
2. In the repository, open **Settings → Pages** and set **Build and deployment → Source** to **GitHub Actions**.
3. Push to `main` or run **Deploy to GitHub Pages** from the **Actions** tab.

GitHub Pages will show the published URL in the workflow deployment environment. For a project repository, the URL is typically `https://OWNER.github.io/REPOSITORY/`; a repository named `OWNER.github.io` can be published at the site root. The site uses relative asset URLs, so it supports either location.

## License

The Rust package declares the MIT license in `Cargo.toml`. The vendored `squashfs_reader` component includes its own MIT and Apache-2.0 license files under `vendor/squashfs_reader/`.
