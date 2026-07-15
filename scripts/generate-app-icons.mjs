#!/usr/bin/env node
/**
 * Generate all app icon formats from a single 1024×1024 SVG source.
 *
 * Produces:
 *   - Electron: icon.png (1024), icon.ico (multi-size)
 *   - Web PWA: pwa-192.png, pwa-512.png, pwa-maskable-192.png, pwa-maskable-512.png
 *   - Web favicon: favicon.png (64), favicon-32.png, favicon-16.png
 *   - Web apple-touch: apple-touch-icon.png (180), -120, -152, -167
 *   - Web themed logo: logo-dark-512x512.svg (copy of source)
 *   - Preview PNG
 *
 * Usage:  node scripts/generate-app-icons.mjs
 *
 * Requires: sharp (already a workspace dependency)
 */
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import sharp from 'sharp';

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(__dirname, '..');

// --- Source SVGs ---
const ICON_SVG = join(ROOT, 'packages/electron/resources/icons/app-icon.svg');
// Favicon source (uses currentColor for light/dark adaptation)
const FAVICON_SVG = join(ROOT, 'packages/web/public/favicon.svg');

// --- Output dirs ---
const ELECTRON_ICONS = join(ROOT, 'packages/electron/resources/icons');
const WEB_PUBLIC = join(ROOT, 'packages/web/public');

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Render an SVG buffer to a PNG of the given size. */
async function svgToPng(svgBuffer, size) {
  return sharp(svgBuffer, { density: 384 })
    .resize(size, size, { fit: 'contain', background: { r: 0, g: 0, b: 0, alpha: 0 } })
    .png()
    .toBuffer();
}

/**
 * Build a multi-resolution .ico from an array of PNG buffers.
 * Format reference: https://en.wikipedia.org/wiki/ICO_(file_format)
 */
function buildIco(pngBuffers /* Array<{ size: number, data: Buffer }> */) {
  // Header: 6 bytes
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);     // reserved
  header.writeUInt16LE(1, 2);     // type = 1 (icon)
  header.writeUInt16LE(pngBuffers.length, 4);

  const dirEntries = [];
  const imageData = [];
  let offset = 6 + pngBuffers.length * 16;

  for (const { size, data } of pngBuffers) {
    const entry = Buffer.alloc(16);
    // Width / height: 0 means 256
    entry.writeUInt8(size >= 256 ? 0 : size, 0);
    entry.writeUInt8(size >= 256 ? 0 : size, 1);
    entry.writeUInt8(0, 2);   // color palette
    entry.writeUInt8(0, 3);   // reserved
    entry.writeUInt16LE(1, 4);  // color planes
    entry.writeUInt16LE(32, 6); // bits per pixel
    entry.writeUInt32LE(data.length, 8);  // image size
    entry.writeUInt32LE(offset, 12);      // image offset
    dirEntries.push(entry);
    imageData.push(data);
    offset += data.length;
  }

  return Buffer.concat([header, ...dirEntries, ...imageData]);
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main() {
  console.log('📖 Reading source SVGs...');
  const iconSvg = readFileSync(ICON_SVG);

  // --- Electron: icon.png (1024) ---
  console.log('🖼️  Generating Electron icon.png (1024)...');
  const iconPng1024 = await svgToPng(iconSvg, 1024);
  writeFileSync(join(ELECTRON_ICONS, 'icon.png'), iconPng1024);

  // --- Electron: icon.ico (multi-size: 16,32,48,64,128,256) ---
  console.log('🖼️  Generating Electron icon.ico (multi-size)...');
  const icoSizes = [16, 32, 48, 64, 128, 256];
  const icoPngs = [];
  for (const size of icoSizes) {
    icoPngs.push({ size, data: await svgToPng(iconSvg, size) });
  }
  const icoBuffer = buildIco(icoPngs);
  writeFileSync(join(ELECTRON_ICONS, 'icon.ico'), icoBuffer);

  // --- Web PWA icons ---
  console.log('🌐 Generating PWA icons...');
  for (const [file, size] of [
    ['pwa-192.png', 192],
    ['pwa-512.png', 512],
  ]) {
    writeFileSync(join(WEB_PUBLIC, file), await svgToPng(iconSvg, size));
  }

  // --- Maskable icons (with padding for safe zone) ---
  // We render the cube slightly smaller and centered so the adaptive mask doesn't crop it.
  console.log('🎭 Generating maskable PWA icons...');
  const maskableSvg = makeMaskableSvg(iconSvg);
  for (const [file, size] of [
    ['pwa-maskable-192.png', 192],
    ['pwa-maskable-512.png', 512],
  ]) {
    writeFileSync(join(WEB_PUBLIC, file), await svgToPng(Buffer.from(maskableSvg), size));
  }

  // --- Apple touch icons ---
  console.log('🍎 Generating Apple touch icons...');
  for (const [file, size] of [
    ['apple-touch-icon.png', 180],
    ['apple-touch-icon-120x120.png', 120],
    ['apple-touch-icon-152x152.png', 152],
    ['apple-touch-icon-167x167.png', 167],
    ['apple-touch-icon-180x180.png', 180],
  ]) {
    writeFileSync(join(WEB_PUBLIC, file), await svgToPng(iconSvg, size));
  }

  // --- Favicon PNGs (from favicon.svg source) ---
  console.log('🔖 Generating favicon PNGs...');
  // favicon.svg uses currentColor — render a dark-bg version (white glyph)
  const faviconSvgDark = readFileSync(FAVICON_SVG);
  for (const [file, size] of [
    ['favicon.png', 64],
    ['favicon-32.png', 32],
    ['favicon-16.png', 16],
  ]) {
    // Render on transparent background with white currentColor
    const styled = wrapFaviconForRender(faviconSvgDark, size);
    writeFileSync(join(WEB_PUBLIC, file), await svgToPng(Buffer.from(styled), size));
  }

  // --- Themed logo SVGs (copy source) ---
  console.log('📋 Copying themed logo SVGs...');
  writeFileSync(join(WEB_PUBLIC, 'logo-dark-512x512.svg'), iconSvg);
  // Light version: we keep the same SVG for now (dark bg works on light too as a badge)
  writeFileSync(join(WEB_PUBLIC, 'logo-light-512x512.svg'), iconSvg);

  console.log('✅ All icons generated successfully!');
}

/**
 * Resize the favicon SVG to the target PNG dimensions.
 * The new favicon uses fixed brand colors (no currentColor), so we just
 * update the width/height for proper rasterization at the target size.
 */
function wrapFaviconForRender(faviconSvgBuffer, size) {
  const svgStr = faviconSvgBuffer.toString('utf-8');
  return svgStr.replace(
    /width="\d+"\s+height="\d+"/,
    `width="${size}" height="${size}"`
  );
}

/**
 * Create a maskable variant: full-bleed background with the glyph scaled to ~75%.
 * The source SVG now uses a 600×600 viewBox with full-canvas background rect.
 * We wrap all content after the background in a centered scale group for safe-zone compliance.
 */
function makeMaskableSvg(iconSvgBuffer) {
  const svgStr = iconSvgBuffer.toString('utf-8');
  return svgStr
    // Background already fills the canvas; wrap everything else in a scale group
    .replace(
      '<rect width="600" height="600" fill="url(#bgGrad)" />',
      '<rect width="600" height="600" fill="url(#bgGrad)" /><g transform="translate(75, 75) scale(0.75)">'
    )
    .replace('</svg>', '</g></svg>');
}

main().catch((err) => {
  console.error('❌ Icon generation failed:', err);
  process.exit(1);
});
