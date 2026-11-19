import init, * as wasm from "./pkg/webxtract.js?v=1.0.0";

let wasmReady;

self.addEventListener("message", async ({ data }) => {
  const { id, buffer, filename, maxBytes } = data;
  let { format } = data;
  const signature = new Uint8Array(buffer, 0, Math.min(buffer.byteLength, 5));
  if (signature.length === 5 && String.fromCharCode(...signature) === "%PDF-") {
    format = "pdf";
  }
  const startedAt = performance.now();
  console.info("[extract-worker] Job started", {
    id,
    format,
    inputBytes: buffer.byteLength,
  });
  try {
    self.postMessage({ id, type: "status", message: "Loading the extractor…" });
    wasmReady ??= init();
    await wasmReady;
    wasm.set_extraction_limit(maxBytes);
    console.info("[extract-worker] WASM module ready", { id });

    self.postMessage({
      id,
      type: "status",
      message: format === "msi"
        ? "Reading MSI database and decompressing embedded files in the background…"
        : format === "appimage"
          ? "Reading AppImage SquashFS contents in the background…"
        : format === "deb"
          ? "Unpacking Debian package contents in the background…"
        : format === "archive"
          ? "Reading archive and decompressing files in the background…"
          : format === "android-boot"
            ? "Reading Android boot image components in the background…"
          : format === "iso"
            ? "Reading ISO image and extracting its files in the background…"
            : format === "pdf"
              ? "Reading PDF and extracting embedded attachments in the background…"
            : format === "exav"
              ? "Checking additional archive and disk image formats…"
            : format === "installer"
              ? "Checking installer format and extracting files in the background…"
          : "Reading NSIS installer in the background…",
    });

    const input = new Uint8Array(buffer);
    const extractor = format === "msi"
      ? wasm.extract_msi
      : format === "appimage"
        ? wasm.extract_appimage
      : format === "deb"
        ? wasm.extract_deb
      : format === "android-boot"
        ? wasm.extract_android_boot
      : format === "archive"
        ? wasm.extract_archive
        : format === "iso"
          ? wasm.extract_iso
          : format === "pdf"
            ? wasm.extract_pdf
          : format === "exav"
            ? wasm.extract_exav
          : format === "installer"
            ? wasm.extract_exe_installer
            : wasm.extract_nsis;
    if (!extractor) {
      throw new Error("The WebAssembly module is out of date. Rebuild it with wasm-pack before extracting this format.");
    }
    let extracted;
    if (format === "archive") {
      try {
        extracted = extractor(input, filename);
      } catch (primaryError) {
        console.warn("[extract-worker] Existing archive reader failed; trying exav-unpack", { id, primaryError });
        try {
          extracted = wasm.extract_exav(input);
        } catch {
          throw primaryError;
        }
      }
      if (extracted.length === 0) {
        try {
          const additional = wasm.extract_exav(input);
          if (additional.length > 0) extracted = additional;
        } catch {
          // Keep the existing reader's empty result when exav does not recognize it.
        }
      }
    } else {
      extracted = extractor(input);
    }
    const files = Array.from(extracted, ({ name, size, data: bytes }) => ({ name, size, data: bytes }));
    const transfer = files.map(({ data }) => data.buffer);
    const outputBytes = files.reduce((total, file) => total + file.size, 0);
    console.info("[extract-worker] Extraction finished", {
      id,
      files: files.length,
      outputBytes,
      elapsedSeconds: ((performance.now() - startedAt) / 1000).toFixed(1),
    });
    self.postMessage({ id, type: "result", files }, transfer);
  } catch (error) {
    console.error("[extract-worker] Extraction failed", {
      id,
      elapsedSeconds: ((performance.now() - startedAt) / 1000).toFixed(1),
      error,
    });
    self.postMessage({ id, type: "error", message: error?.message || String(error) });
  }
});
