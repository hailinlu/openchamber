/**
 * GridForge migration command — copies GridForge data directory to GridForge
 * format, transforming JSON keys as needed.
 *
 * Usage: gridforge migrate [--dry-run] [--source <dir>] [--dest <dir>]
 *
 * Default source: ~/.config/openchamber
 * Default dest:   ~/.config/gridforge
 */

import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import {
  intro as clackIntro,
  outro as clackOutro,
  isJsonMode,
  isQuietMode,
  printJson,
  logStatus,
  cancel as clackCancel,
} from '../cli-output.js';

const homeDir = () => os.homedir();
const DEFAULT_SOURCE = () => path.join(homeDir(), '.config', 'openchamber');
const DEFAULT_DEST = () => path.join(homeDir(), '.config', 'gridforge');

// Files and directories to migrate (relative to source root).
const MIGRATE_ENTRIES = [
  // Direct config files
  { type: 'file', name: 'settings.json' },
  { type: 'file', name: 'cloudflare-managed-remote-tunnels.json' },
  { type: 'file', name: 'push-subscriptions.json' },
  { type: 'file', name: 'apns-tokens.json' },
  // Data directories
  { type: 'dir', name: 'projects' },
  { type: 'dir', name: 'goals' },
  { type: 'dir', name: 'quota' },
  { type: 'dir', name: 'themes' },
];

// JSON keys that need in-content transformation.
const KEY_TRANSFORMS = [
  ['metadata.openchamber.', 'metadata.gridforge.'],
];

function* iterFiles(rootDir, entry) {
  const fullPath = path.join(rootDir, entry.name);
  if (entry.type === 'file') {
    if (fs.existsSync(fullPath) && fs.statSync(fullPath).isFile()) {
      yield { relPath: entry.name, srcPath: fullPath, isFile: true };
    }
    return;
  }
  // Directory — walk recursively.
  if (!fs.existsSync(fullPath) || !fs.statSync(fullPath).isDirectory()) return;

  for (const dirEntry of fs.readdirSync(fullPath, { recursive: true, withFileTypes: true })) {
    const relPath = path.join(entry.name, dirEntry.name);
    const srcPath = path.join(rootDir, relPath);
    if (dirEntry.isFile()) {
      yield { relPath, srcPath, isFile: true };
    } else if (dirEntry.isDirectory()) {
      yield { relPath, srcPath, isFile: false };
    }
  }
}

const transformJsonContent = (text) => {
  let result = text;
  for (const [from, to] of KEY_TRANSFORMS) {
    result = result.split(from).join(to);
  }
  return result;
};

const readDirExcludingDirs = (rootDir) => {
  if (!fs.existsSync(rootDir)) return [];
  try {
    return fs.readdirSync(rootDir);
  } catch {
    return [];
  }
};

async function migrateCommand(options = {}) {
  const sourceDir = options.source || DEFAULT_SOURCE();
  const destDir = options.dest || DEFAULT_DEST();
  const dryRun = options.dryRun === true;

  if (!fs.existsSync(sourceDir)) {
    const msg = `Source directory does not exist: ${sourceDir}`;
    if (isJsonMode(options)) {
      printJson({ ok: false, error: msg });
      process.exit(1);
    }
    logStatus('error', msg);
    process.exit(1);
  }

  if (sourceDir === destDir) {
    const msg = 'Source and destination directories must be different';
    if (isJsonMode(options)) {
      printJson({ ok: false, error: msg });
      process.exit(1);
    }
    logStatus('error', msg);
    process.exit(1);
  }

  // Plan phase — enumerate all files to migrate.
  const filesToCopy = [];
  const dirsToCreate = [];

  for (const entry of MIGRATE_ENTRIES) {
    const srcRoot = path.join(sourceDir, entry.name);
    if (!fs.existsSync(srcRoot)) continue;

    if (entry.type === 'file') {
      if (fs.statSync(srcRoot).isFile()) {
        filesToCopy.push({ relPath: entry.name, srcPath: srcRoot });
      }
    } else {
      // Directory — walk.
      if (fs.statSync(srcRoot).isDirectory()) {
        for (const fileEntry of fs.readdirSync(srcRoot, { recursive: true, withFileTypes: true })) {
          const relPath = path.join(entry.name, fileEntry.name);
          const srcPath = path.join(srcRoot, fileEntry.name);
          if (fileEntry.isDirectory()) {
            dirsToCreate.push(relPath);
          } else if (fileEntry.isFile()) {
            dirsToCreate.push(path.dirname(relPath));
            filesToCopy.push({ relPath, srcPath });
          }
        }
      }
    }
  }

  // Deduplicate dirs.
  const uniqueDirs = [...new Set(dirsToCreate)].sort();

  const plan = {
    sourceDir,
    destDir,
    directories: uniqueDirs.length,
    files: filesToCopy.length,
    totalEntries: filesToCopy.length,
    dryRun,
  };

  if (isJsonMode(options)) {
    if (dryRun) {
      printJson({ ok: true, ...plan, entries: filesToCopy.map(f => f.relPath) });
      return;
    }

    // Execute migration.
    const results = [];
    for (const dirRel of uniqueDirs) {
      const destPath = path.join(destDir, dirRel);
      if (!fs.existsSync(destPath)) {
        fs.mkdirSync(destPath, { recursive: true });
        results.push({ action: 'mkdir', path: dirRel });
      }
    }

    for (const file of filesToCopy) {
      const destPath = path.join(destDir, file.relPath);
      // Ensure parent directory exists.
      const parentDir = path.dirname(destPath);
      if (!fs.existsSync(parentDir)) {
        fs.mkdirSync(parentDir, { recursive: true });
      }

      const isJsonFile = file.relPath.endsWith('.json');
      if (isJsonFile) {
        const raw = fs.readFileSync(file.srcPath, 'utf-8');
        const transformed = transformJsonContent(raw);
        fs.writeFileSync(destPath, transformed, 'utf-8');
        results.push({ action: 'copy+transform', path: file.relPath });
      } else {
        fs.cpSync(file.srcPath, destPath, { recursive: false, errorOnExist: false });
        results.push({ action: 'copy', path: file.relPath });
      }
    }

    printJson({ ok: true, ...plan, entries: results });
    return;
  }

  // Human output.
  if (isQuietMode(options)) {
    if (dryRun) {
      process.stdout.write(`migrate: would copy ${filesToCopy.length} files from ${sourceDir} to ${destDir}\n`);
      return;
    }
    process.stdout.write(`migrate: copied ${filesToCopy.length} files from ${sourceDir} to ${destDir}\n`);
    // Execute same as JSON but without output.
    for (const dirRel of uniqueDirs) {
      const destPath = path.join(destDir, dirRel);
      if (!fs.existsSync(destPath)) fs.mkdirSync(destPath, { recursive: true });
    }
    for (const file of filesToCopy) {
      const destPath = path.join(destDir, file.relPath);
      const parentDir = path.dirname(destPath);
      if (!fs.existsSync(parentDir)) fs.mkdirSync(parentDir, { recursive: true });
      if (file.relPath.endsWith('.json')) {
        const raw = fs.readFileSync(file.srcPath, 'utf-8');
        fs.writeFileSync(destPath, transformJsonContent(raw), 'utf-8');
      } else {
        fs.cpSync(file.srcPath, destPath, { recursive: false, errorOnExist: false });
      }
    }
    return;
  }

  clackIntro('GridForge Migration');

  if (filesToCopy.length === 0) {
    logStatus('info', 'No files to migrate from', sourceDir);
    clackOutro('Nothing to migrate');
    return;
  }

  logStatus('info', 'Source', sourceDir);
  logStatus('info', 'Destination', destDir);
  logStatus('info', 'Files to migrate', String(filesToCopy.length));
  logStatus('info', 'JSON key transform', `${String(KEY_TRANSFORMS.length)} patterns`);

  if (dryRun) {
    logStatus('info', '(dry run — no files written)');
    for (const entry of filesToCopy) {
      process.stdout.write(`  ${entry.relPath}\n`);
    }
    clackOutro(`Dry run: ${filesToCopy.length} files would be migrated`);
    return;
  }

  // Execute migration.
  let copied = 0;
  let transformed = 0;

  for (const dirRel of uniqueDirs) {
    const destPath = path.join(destDir, dirRel);
    if (!fs.existsSync(destPath)) {
      fs.mkdirSync(destPath, { recursive: true });
    }
  }

  for (const file of filesToCopy) {
    const destPath = path.join(destDir, file.relPath);
    const parentDir = path.dirname(destPath);
    if (!fs.existsSync(parentDir)) fs.mkdirSync(parentDir, { recursive: true });

    if (file.relPath.endsWith('.json')) {
      const raw = fs.readFileSync(file.srcPath, 'utf-8');
      const transformedContent = transformJsonContent(raw);
      fs.writeFileSync(destPath, transformedContent, 'utf-8');
      if (transformedContent !== raw) {
        transformed++;
      }
      process.stdout.write(`  ✓ ${file.relPath} (transformed)\n`);
    } else {
      fs.cpSync(file.srcPath, destPath, { recursive: false, errorOnExist: false });
      process.stdout.write(`  ✓ ${file.relPath}\n`);
    }
    copied++;
  }

  const transformNote = transformed > 0 ? ` (${transformed} with key transforms)` : '';
  clackOutro(`Migrated ${copied} files from ${sourceDir} to ${destDir}${transformNote}`);
}

export { migrateCommand };
