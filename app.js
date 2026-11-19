const input = document.querySelector("#file-input");
const dropzone = document.querySelector("#dropzone");
const status = document.querySelector("#status");
const results = document.querySelector("#results");
const fileList = document.querySelector("#file-list");
const resultTitle = document.querySelector("#result-title");
const downloadAll = document.querySelector("#download-all");
const memoryLimit = document.querySelector("#memory-limit");
const savedMemoryLimit = localStorage.getItem("extraction-memory-limit");
if (savedMemoryLimit && [...memoryLimit.options].some((option) => option.value === savedMemoryLimit)) {
  memoryLimit.value = savedMemoryLimit;
}
memoryLimit.addEventListener("change", () => localStorage.setItem("extraction-memory-limit", memoryLimit.value));
let extractedFiles = [];
let sourceFilename = "";
let extractorWorker;
let nextJobId = 0;
let activeJobId = 0;
const pendingJobs = new Map();

function getExtractorWorker() {
  if (extractorWorker) return extractorWorker;
  extractorWorker = new Worker("./extract-worker.js?v=1.0.0", { type: "module" });
  extractorWorker.addEventListener("message", ({ data }) => {
    const job = pendingJobs.get(data.id);
    if (!job) return;
    if (data.type === "status") {
      console.info("[extract-ui] Worker status", { id: data.id, message: data.message });
      job.onStatus(data.message);
    } else if (data.type === "result") {
      pendingJobs.delete(data.id);
      console.info("[extract-ui] Received extracted files", {
        id: data.id,
        files: data.files.length,
        bytes: data.files.reduce((total, file) => total + file.size, 0),
      });
      job.resolve(data.files);
    } else if (data.type === "error") {
      pendingJobs.delete(data.id);
      console.error("[extract-ui] Worker reported an extraction error", { id: data.id, message: data.message });
      job.reject(new Error(data.message));
    }
  });
  extractorWorker.addEventListener("error", (event) => {
    const message = event.message || "The extraction worker stopped unexpectedly.";
    console.error("[extract-ui] Extraction worker error", { message, filename: event.filename, lineno: event.lineno });
    for (const job of pendingJobs.values()) job.reject(new Error(message));
    pendingJobs.clear();
    extractorWorker?.terminate();
    extractorWorker = null;
  });
  return extractorWorker;
}

function extractInWorker(buffer, format, filename, maxBytes, onStatus) {
  const id = ++nextJobId;
  return new Promise((resolve, reject) => {
    pendingJobs.set(id, { resolve, reject, onStatus });
    getExtractorWorker().postMessage({ id, buffer, format, filename, maxBytes }, [buffer]);
  });
}

function readableSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function safeFilename(path) {
  // Keep directory structure while removing roots and traversal segments.
  const segments = String(path).replaceAll("\\", "/").split("/")
    .filter((part) => part && part !== "." && part !== ".." && !part.includes(":"));
  return segments.join("/") || "unnamed-file";
}

function saveFile(file) {
  const blob = new Blob([file.data], { type: "application/octet-stream" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = safeFilename(file.name).split("/").at(-1);
  link.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

const crcTable = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let value = n;
    for (let bit = 0; bit < 8; bit++) value = value & 1 ? 0xedb88320 ^ (value >>> 1) : value >>> 1;
    table[n] = value >>> 0;
  }
  return table;
})();

async function crc32(bytes) {
  let crc = 0xffffffff;
  const chunkSize = 16 * 1024 * 1024;
  for (let start = 0; start < bytes.length; start += chunkSize) {
    const end = Math.min(start + chunkSize, bytes.length);
    for (let i = start; i < end; i++) crc = crcTable[(crc ^ bytes[i]) & 0xff] ^ (crc >>> 8);
    if (end < bytes.length) await new Promise((resolve) => setTimeout(resolve, 0));
  }
  return (crc ^ 0xffffffff) >>> 0;
}

async function makeZip(files) {
  if (files.length > 0xffff) throw new Error("ZIP supports at most 65,535 files in this browser extractor.");
  const totalBytes = files.reduce((total, file) => total + file.data.length, 0);
  if (totalBytes > 0xffffffff) throw new Error("This archive is larger than the ZIP format limit supported here (4 GiB).");

  const now = new Date();
  const dosTime = (now.getHours() << 11) | (now.getMinutes() << 5) | Math.floor(now.getSeconds() / 2);
  const dosDate = ((Math.max(1980, now.getFullYear()) - 1980) << 9) | ((now.getMonth() + 1) << 5) | now.getDate();
  const parts = [];
  const directory = [];
  let offset = 0;
  let completed = 0;

  for (const file of files) {
    const path = safeFilename(file.name);
    const nameBytes = new TextEncoder().encode(path);
    const bytes = file.data;
    if (nameBytes.length > 0xffff || bytes.length > 0xffffffff || offset > 0xffffffff) {
      throw new Error(`“${path}” exceeds the ZIP format limit supported here.`);
    }
    const crc = await crc32(bytes);
    // Local file header. ZIP method 0 stores the already extracted bytes as-is.
    const local = new Uint8Array(30 + nameBytes.length);
    const localView = new DataView(local.buffer);
    localView.setUint32(0, 0x04034b50, true);
    localView.setUint16(4, 20, true);
    localView.setUint16(6, 0x0800, true); // UTF-8 file name
    localView.setUint16(8, 0, true);
    localView.setUint16(10, dosTime, true);
    localView.setUint16(12, dosDate, true);
    localView.setUint32(14, crc, true);
    localView.setUint32(18, bytes.length, true);
    localView.setUint32(22, bytes.length, true);
    localView.setUint16(26, nameBytes.length, true);
    local.set(nameBytes, 30);
    parts.push(local, bytes);

    const central = new Uint8Array(46 + nameBytes.length);
    const centralView = new DataView(central.buffer);
    centralView.setUint32(0, 0x02014b50, true);
    centralView.setUint16(4, 20, true);
    centralView.setUint16(6, 20, true);
    centralView.setUint16(8, 0x0800, true);
    centralView.setUint16(10, 0, true);
    centralView.setUint16(12, dosTime, true);
    centralView.setUint16(14, dosDate, true);
    centralView.setUint32(16, crc, true);
    centralView.setUint32(20, bytes.length, true);
    centralView.setUint32(24, bytes.length, true);
    centralView.setUint16(28, nameBytes.length, true);
    centralView.setUint32(38, 0, true);
    centralView.setUint32(42, offset, true);
    central.set(nameBytes, 46);
    directory.push(central);
    offset += local.length + bytes.length;
    completed++;
    status.textContent = `Preparing ZIP… ${completed} of ${files.length} files`;
  }

  const directoryOffset = offset;
  for (const entry of directory) {
    parts.push(entry);
    offset += entry.length;
  }
  const directorySize = offset - directoryOffset;
  if (offset > 0xffffffff || directorySize > 0xffffffff) {
    throw new Error("This archive is larger than the ZIP format limit supported here (4 GiB).");
  }
  const end = new Uint8Array(22);
  const endView = new DataView(end.buffer);
  endView.setUint32(0, 0x06054b50, true);
  endView.setUint16(8, files.length, true);
  endView.setUint16(10, files.length, true);
  endView.setUint32(12, directorySize, true);
  endView.setUint32(16, directoryOffset, true);
  parts.push(end);
  return new Blob(parts, { type: "application/zip" });
}

function showFiles(files, sourceName) {
  fileList.replaceChildren();
  extractedFiles = files;
  sourceFilename = sourceName;
  if (files.length === 0) {
    status.textContent = "This file did not contain files the extractor could list.";
    status.className = "status error";
    results.hidden = true;
    return;
  }
  resultTitle.textContent = `${files.length} ${files.length === 1 ? "file" : "files"} found`;
  downloadAll.disabled = false;
  for (const file of files) {
    const row = document.createElement("article");
    row.className = "file-row";
    const details = document.createElement("div");
    details.className = "file-details";
    const name = document.createElement("span");
    name.className = "file-name";
    name.textContent = safeFilename(file.name);
    name.title = name.textContent;
    const size = document.createElement("span");
    size.className = "file-size";
    size.textContent = readableSize(file.size);
    const button = document.createElement("button");
    button.className = "download-button";
    button.type = "button";
    button.textContent = "Download";
    button.addEventListener("click", () => saveFile(file));
    details.append(name, size);
    row.append(details, button);
    fileList.append(row);
  }
  status.textContent = `Read ${sourceName} in this browser.`;
  status.className = "status success";
  results.hidden = false;
}

async function processFile(file) {
  if (!file) return;
  console.info("[extract-ui] Selected file", { name: file.name, bytes: file.size });
  const jobId = ++nextJobId;
  activeJobId = jobId;
  results.hidden = true;
  extractedFiles = [];
  fileList.replaceChildren();
  status.className = "status loading";
  status.textContent = "Opening installer…";
  let progressTimer;
  try {
    const buffer = await file.arrayBuffer();
    if (jobId !== activeJobId) return;
    const bytes = new Uint8Array(buffer);
    const isMsi = /\.(msi|msp|msm)$/i.test(file.name) ||
      (bytes.length >= 8 && bytes[0] === 0xd0 && bytes[1] === 0xcf && bytes[2] === 0x11 && bytes[3] === 0xe0 &&
       bytes[4] === 0xa1 && bytes[5] === 0xb1 && bytes[6] === 0x1a && bytes[7] === 0xe1);
    const isDeb = /\.deb$/i.test(file.name);
    const isAppImage = /\.appimage$/i.test(file.name) ||
      (bytes.length >= 11 && bytes[0] === 0x7f && bytes[1] === 0x45 && bytes[2] === 0x4c && bytes[3] === 0x46 &&
       bytes[8] === 0x41 && bytes[9] === 0x49 && bytes[10] === 0x02);
    const archiveExtension = /\.(zip|jar|apk|aab|epub|nupkg|whl|tar|tgz|tbz2?|txz|tzst|gz|bz2|xz|zst|7z|arj|cab|msu)$/i.test(file.name);
    const hasArchiveSignature =
      (bytes.length >= 4 && bytes[0] === 0x50 && bytes[1] === 0x4b && [0x03, 0x05, 0x07].includes(bytes[2]) && [0x04, 0x06, 0x08].includes(bytes[3])) ||
      (bytes.length >= 6 && bytes.subarray(0, 6).every((byte, i) => byte === [0x37, 0x7a, 0xbc, 0xaf, 0x27, 0x1c][i])) ||
      (bytes.length >= 2 && bytes[0] === 0x1f && bytes[1] === 0x8b) ||
      (bytes.length >= 3 && bytes[0] === 0x42 && bytes[1] === 0x5a && bytes[2] === 0x68) ||
      (bytes.length >= 6 && bytes[0] === 0xfd && bytes[1] === 0x37 && bytes[2] === 0x7a && bytes[3] === 0x58 && bytes[4] === 0x5a && bytes[5] === 0x00) ||
      (bytes.length >= 4 && bytes[0] === 0x28 && bytes[1] === 0xb5 && bytes[2] === 0x2f && bytes[3] === 0xfd);
    const isIso = /\.iso$/i.test(file.name) ||
      (bytes.length >= 32_774 && bytes[32_769] === 0x43 && bytes[32_770] === 0x44 && bytes[32_771] === 0x30 && bytes[32_772] === 0x30 && bytes[32_773] === 0x31);
    const isAndroidBoot = /\.boot$/i.test(file.name) ||
      (bytes.length >= 8 && String.fromCharCode(...bytes.subarray(0, 8)) === "ANDROID!");
    const isPdf = /\.pdf$/i.test(file.name) ||
      (bytes.length >= 5 && String.fromCharCode(...bytes.subarray(0, 5)) === "%PDF-");
    const isExe = /\.exe$/i.test(file.name) ||
      (bytes.length >= 2 && bytes[0] === 0x4d && bytes[1] === 0x5a);
    const format = isPdf ? "pdf" : isMsi ? "msi" : isAppImage ? "appimage" : isDeb ? "deb" : isAndroidBoot ? "android-boot" : isIso ? "iso" : archiveExtension || hasArchiveSignature ? "archive" : isExe ? "installer" : "exav";
    console.info("[extract-ui] Detected file format", { name: file.name, format });
    status.textContent = format === "msi"
      ? "Reading MSI and decompressing embedded files in the background…"
      : format === "appimage"
        ? "Reading AppImage SquashFS contents in the background…"
      : format === "deb"
        ? "Unpacking Debian package contents in the background…"
      : format === "archive"
        ? "Reading archive and decompressing files in the background…"
        : format === "android-boot"
          ? "Reading Android boot image and extracting its components…"
        : format === "iso"
          ? "Reading ISO image and extracting its files in the background…"
        : format === "pdf"
          ? "Reading PDF and extracting embedded attachments…"
        : format === "exav"
          ? "Checking additional archive and disk image formats…"
          : format === "installer"
            ? "Checking installer format and extracting files in the background…"
        : "Reading installer in the background…";
    const startedAt = Date.now();
    progressTimer = setInterval(() => {
      if (jobId === activeJobId) {
        const seconds = Math.floor((Date.now() - startedAt) / 1000);
        status.textContent = `Still working in the background (${seconds}s). You can keep using this page.`;
      }
    }, 10_000);
    const files = await extractInWorker(buffer, format, file.name, Number(memoryLimit.value), (message) => {
      if (jobId === activeJobId) status.textContent = message;
    });
    if (jobId !== activeJobId) return;
    showFiles(files, file.name);
  } catch (error) {
    console.error("[extract-ui] Could not read selected installer", { name: file.name, error });
    if (jobId === activeJobId) {
      status.textContent = error?.message || String(error);
      status.className = "status error";
    }
  } finally {
    clearInterval(progressTimer);
  }
}

input.addEventListener("change", () => processFile(input.files?.[0]));
dropzone.addEventListener("dragover", (event) => {
  event.preventDefault();
  dropzone.classList.add("dragging");
});
dropzone.addEventListener("dragleave", () => dropzone.classList.remove("dragging"));
dropzone.addEventListener("drop", (event) => {
  event.preventDefault();
  dropzone.classList.remove("dragging");
  processFile(event.dataTransfer?.files?.[0]);
});
dropzone.addEventListener("keydown", (event) => {
  if (event.key === "Enter" || event.key === " ") {
    event.preventDefault();
    input.click();
  }
});
document.querySelector("#clear-button").addEventListener("click", () => {
  activeJobId = ++nextJobId;
  input.value = "";
  fileList.replaceChildren();
  results.hidden = true;
  status.textContent = "";
  input.click();
});

downloadAll.addEventListener("click", async () => {
  if (extractedFiles.length === 0 || downloadAll.disabled) return;
  const records = extractedFiles;
  downloadAll.disabled = true;
  downloadAll.textContent = "Preparing ZIP…";
  status.className = "status loading";
  try {
    const blob = await makeZip(records);
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = `${safeFilename(sourceFilename || "extracted-files").split("/").at(-1).replace(/\.exe$/i, "") || "extracted-files"}.zip`;
    link.click();
    setTimeout(() => URL.revokeObjectURL(url), 30_000);
    status.className = "status success";
    status.textContent = `ZIP ready: ${records.length} ${records.length === 1 ? "file" : "files"}.`;
  } catch (error) {
    status.className = "status error";
    status.textContent = error?.message || String(error);
  } finally {
    downloadAll.disabled = false;
    downloadAll.textContent = "Download all as ZIP";
  }
});

