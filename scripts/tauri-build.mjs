#!/usr/bin/env node
// 一键 Tauri 桌面打包脚本 (对标 scripts/tauri-dev.mjs 的 dev 对称物)。
//
// 流程:
//   1. bun run build (packages/web) → 产出 packages/web/dist
//   2. 原子暂存 dist → rust/oc-tauri/ui-dist
//      (tauri.conf.json 的 frontendDist: "../ui-dist" 相对 src-tauri 解析,
//       即 rust/oc-tauri/ui-dist, 不是 rust/ui-dist)
//   3. cargo tauri build (cwd: rust/oc-tauri/src-tauri) → MSI + NSIS
//
// 用法:
//   node ./scripts/tauri-build.mjs                  # 完整打包 (构建 UI + 暂存 + cargo tauri build)
//   node ./scripts/tauri-build.mjs --skip-web-build # 复用已构建的 UI, 仅暂存 + cargo tauri build
//   node ./scripts/tauri-build.mjs --debug          # debug 构建 (透传给 cargo tauri build)
//   node ./scripts/tauri-build.mjs --bundles nsis   # 只打 NSIS (透传给 cargo tauri build)
//
// 注意:
//   - helper (run/resolveBun/removeDir/copyDir) 与 packages/electron/scripts/build-web-assets.mjs
//     对齐, 均为 Windows 验证过的模式 (.cmd/.bat 走 cmd.exe /d /s /c call + 正确引号;
//     spawnSync 探测带 windowsHide:true; removeDir 容忍 ENOTEMPTY/EBUSY/EPERM 重试)。
//   - 不在 tauri.conf.json 加 beforeBuildCommand: 它以 src-tauri 为 cwd, 跨平台相对路径脆弱。
//     本仓库 dev/打包 都走 scripts/*.mjs 编排, 保持一致。

import fs from 'node:fs/promises';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

// 仓库路径 (与 scripts/tauri-dev.mjs:27-31 对齐)。
const repoRoot = path.resolve(__dirname, '..');
const webDir = path.join(repoRoot, 'packages', 'web');
const tauriSrcDir = path.join(repoRoot, 'rust', 'oc-tauri', 'src-tauri');
// frontendDist: "../ui-dist" 相对 src-tauri → rust/oc-tauri/ui-dist (不是 rust/ui-dist)。
const uiDistDir = path.join(repoRoot, 'rust', 'oc-tauri', 'ui-dist');
const webDistDir = path.join(webDir, 'dist');

// ---------------------------------------------------------------------------
// helpers (与 packages/electron/scripts/build-web-assets.mjs 对齐, Windows 验证过)
// ---------------------------------------------------------------------------

const quoteWindowsCommandArg = (value) => `"${String(value).replace(/"/g, '""')}"`;

// 同步执行一条命令; .cmd/.bat shim 走 cmd.exe 正确引号; 非零退出即抛错。
const run = (cmd, args, cwd, label = cmd) => {
  const isWindowsCommandScript = process.platform === 'win32' && /\.(cmd|bat)$/i.test(cmd);
  const result = isWindowsCommandScript
    ? spawnSync(
        process.env.ComSpec || 'cmd.exe',
        ['/d', '/s', '/c', ['call', quoteWindowsCommandArg(cmd), ...args.map(quoteWindowsCommandArg)].join(' ')],
        { cwd, stdio: 'inherit', windowsVerbatimArguments: true },
      )
    : spawnSync(cmd, args, { cwd, stdio: 'inherit' });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`Command failed (${label}): ${cmd} ${args.join(' ')}`);
  }
};

// 探测 bun 可执行路径; Windows 用 where.exe (windowsHide), POSIX 用 command -v。
const resolveBun = () => {
  if (typeof process.env.BUN === 'string' && process.env.BUN.trim()) {
    return process.env.BUN.trim();
  }
  if (process.platform === 'win32') {
    const result = spawnSync('where.exe', ['bun'], { encoding: 'utf8', windowsHide: true });
    const candidates = String(result.stdout || '').split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
    const resolved = candidates.find((entry) => /\.(exe|cmd|bat)$/i.test(entry)) || candidates[0];
    return resolved || 'bun';
  }
  const result = spawnSync('/bin/bash', ['-lc', 'command -v bun'], { encoding: 'utf8', windowsHide: true });
  const resolved = (result.stdout || '').trim();
  return resolved || 'bun';
};

// 探测 cargo 可执行路径 (tauri CLI 通过 cargo 调用); 同 resolveBun 的探测风格。
const resolveCargo = () => {
  if (typeof process.env.CARGO === 'string' && process.env.CARGO.trim()) {
    return process.env.CARGO.trim();
  }
  if (process.platform === 'win32') {
    const result = spawnSync('where.exe', ['cargo'], { encoding: 'utf8', windowsHide: true });
    const candidates = String(result.stdout || '').split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
    const resolved = candidates.find((entry) => /\.exe$/i.test(entry)) || candidates[0];
    return resolved || 'cargo';
  }
  const result = spawnSync('/bin/bash', ['-lc', 'command -v cargo'], { encoding: 'utf8', windowsHide: true });
  const resolved = (result.stdout || '').trim();
  return resolved || 'cargo';
};

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// 容忍 Windows 上 EBUSY/EPERM/ENOTEMPTY 的递归删除 (5 次重试)。
const removeDir = async (target) => {
  for (let attempt = 0; attempt < 5; attempt += 1) {
    try {
      await fs.rm(target, { recursive: true, force: true });
      return;
    } catch (error) {
      if (attempt === 4) throw error;
      if (!['ENOTEMPTY', 'EBUSY', 'EPERM'].includes(error?.code)) throw error;
      await sleep(100 * (attempt + 1));
    }
  }
};

const copyDir = async (src, dst) => {
  await fs.mkdir(dst, { recursive: true });
  const entries = await fs.readdir(src, { withFileTypes: true });
  for (const entry of entries) {
    const from = path.join(src, entry.name);
    const to = path.join(dst, entry.name);
    if (entry.isDirectory()) {
      await copyDir(from, to);
    } else {
      await fs.copyFile(from, to);
    }
  }
};

// ---------------------------------------------------------------------------
// 参数解析
// ---------------------------------------------------------------------------

const argv = process.argv.slice(2);
const skipWebBuild = argv.includes('--skip-web-build');
// 本脚本自身的 flag 不透传; 其余 (如 --debug, --bundles nsis) 透传给 `cargo tauri build`。
const ownFlags = new Set(['--skip-web-build']);
const tauriBuildArgs = argv.filter((arg) => !ownFlags.has(arg));

// ---------------------------------------------------------------------------
// 主流程 (同步, 抛错即停)
// ---------------------------------------------------------------------------

const bunExe = resolveBun();
const cargoExe = resolveCargo();

// 1. 构建 web UI → packages/web/dist
if (skipWebBuild) {
  // 复用已构建的 UI; 仍需 dist 存在, 否则后续暂存无源可拷。
  try {
    await fs.access(path.join(webDistDir, 'index.html'));
    console.log('[tauri:build] --skip-web-build: reusing existing packages/web/dist');
  } catch {
    throw new Error(
      `--skip-web-build set but packages/web/dist/index.html not found. Run \`bun run build:web\` first.`,
    );
  }
} else {
  console.log('[tauri:build] building web UI dist (vite build)...');
  run(bunExe, ['run', 'build'], webDir, 'bun run build (web)');
}

// 2. 原子暂存 dist → rust/oc-tauri/ui-dist
//    临时目录 + rename: 避免崩溃/中断留下半空 ui-dist 导致下次 Tauri 打包失败。
//    (Windows 上 fs.rename 无法覆盖已存在目录 → copyDir + removeDir 回退)
console.log('[tauri:build] staging UI dist → rust/oc-tauri/ui-dist ...');
const stagingDir = await fs.mkdtemp(path.join(path.dirname(uiDistDir), 'ui-dist-staging-'));
try {
  await copyDir(webDistDir, stagingDir);
  await removeDir(uiDistDir);
  try {
    await fs.rename(stagingDir, uiDistDir);
  } catch {
    // Windows: rename 无法覆盖已存在目录 (上一步 removeDir 后理论上不存在,
    // 但防锁/竞态仍可能失败)。回退为 copyDir + 清理临时目录。
    await copyDir(stagingDir, uiDistDir);
    await removeDir(stagingDir);
  }
} catch (error) {
  // 任何异常都清理临时目录, 不留垃圾。
  await removeDir(stagingDir).catch(() => {});
  throw error;
}
console.log(`[tauri:build] UI staged at: ${uiDistDir}`);

// 3. cargo tauri build (cwd: rust/oc-tauri/src-tauri)
console.log(`[tauri:build] running: cargo tauri build${tauriBuildArgs.length ? ` ${tauriBuildArgs.join(' ')}` : ''} ...`);
run(cargoExe, ['tauri', 'build', ...tauriBuildArgs], tauriSrcDir, 'cargo tauri build');

// 4. 打印产物路径 (bundle/<target>/...)
const bundleDir = path.join(repoRoot, 'rust', 'target', 'release', 'bundle');
console.log('[tauri:build] done. Installer artifacts:');
try {
  for (const kind of ['msi', 'nsis', 'appimage', 'deb', 'app']) {
    const kindDir = path.join(bundleDir, kind);
    let entries = [];
    try {
      entries = await fs.readdir(kindDir);
    } catch {
      continue;
    }
    for (const name of entries) {
      const full = path.join(kindDir, name);
      const stat = await fs.stat(full);
      if (stat.isFile()) {
        const mb = (stat.size / (1024 * 1024)).toFixed(1);
        console.log(`  ${kind}/${name}  (${mb} MB)`);
      }
    }
  }
} catch {
  // 产物枚举失败不视为构建失败 (cargo 已成功退出)。
  console.log(`  (see ${bundleDir})`);
}
