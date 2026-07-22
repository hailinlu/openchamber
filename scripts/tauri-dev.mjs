#!/usr/bin/env node
// 一键启动 Tauri 桌面壳开发环境 (对标 packages/electron/scripts/electron-dev.mjs)。
//
// 流程:
//   1. 起 Vite HMR 开发服务器 (不启动 Node Express 后端)
//      → UI HMR on :5180, 由 Rust oc-server 处理所有 API 请求
//   2. 等 :5180 就绪
//   3. 起 `cargo tauri dev` (默认进程内嵌 oc-server; OPENCHAMBER_SIDECAR=1 走 sidecar 回退)
//      → Rust oc-server 负责管理 OpenCode + 处理所有 API
//   4. 退出时整树清理 (SIGINT/SIGTERM/SIGHUP 或任一子进程退出)
//
// 用法:
//   node scripts/tauri-dev.mjs              # 默认 (进程内嵌 oc-server)
//   OPENCHAMBER_SIDECAR=1 node scripts/tauri-dev.mjs   # sidecar 回退路径
//
// 端口:
//   UI:  OPENCHAMBER_HMR_UI_PORT  (默认 5180, 必须与 tauri.conf.json devUrl 一致)
//   API: OPENCHAMBER_HMR_API_PORT (默认 3902, Tauri 模式下仅 Rust 侧使用)

import { spawn, spawnSync } from 'node:child_process';
import { rmSync } from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const repoRoot = path.resolve(__dirname, '..');
const tauriSrcDir = path.join(repoRoot, 'rust/oc-tauri/src-tauri');
const webRoot = path.join(repoRoot, 'packages/web');

// 默认 5180, 必须与 tauri.conf.json 的 devUrl 对齐。
const uiPort = process.env.OPENCHAMBER_HMR_UI_PORT || '5180';
const hmrHost = process.env.OPENCHAMBER_HMR_HOST || '127.0.0.1';
// Rust oc-server 绑定端口。与 Vite proxy target 一致：
// Vite 的 proxy 把 /api/* /auth/* /health 转发到此端口，
// Rust oc-server 通过 OPENCHAMBER_PORT 环境变量绑定同一端口。
const apiPort = process.env.OPENCHAMBER_PORT || '3001';
const useDetachedChildren = process.platform !== 'win32';

// 用当前 node 的绝对路径 (process.execPath) 起 node 子进程,
// 避免在 detached 子进程里 PATH 解析不到 `node` (nvm 等场景)。
const nodeBin = process.execPath;

// Windows 下 spawn() 配合 shell:true 会把命令交给 cmd.exe, 而 cmd.exe 会在空格处
// 切分命令名 —— 若 command 是含空格的绝对路径 (例如 process.execPath = "C:\Program Files\nodejs\node.exe"),
// 就会变成 `'C:\Program' is not recognized`。这里复用 packages/electron/scripts/electron-dev.mjs
// 已验证的模式: 解析命令 → 仅对 .cmd/.bat shim 走显式 cmd.exe + 正确引号, 其余直接 spawn。
const quoteWindowsCommandArg = (value) => `"${String(value).replace(/"/g, '""')}"`;

function resolveWindowsCommand(command) {
  if (process.platform !== 'win32' || path.isAbsolute(command)) {
    return command;
  }

  const result = spawnSync('where.exe', [command], { encoding: 'utf8', windowsHide: true });
  if (result.error || result.status !== 0) {
    return command;
  }

  const candidates = String(result.stdout || '').split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
  return candidates.find((entry) => /\.(exe|cmd|bat)$/i.test(entry)) || candidates[0] || command;
}

function spawnProcess(command, args, options = {}) {
  const { env: extraEnv = {}, cwd: cwdOverride, ...rest } = options;

  const resolvedCommand = resolveWindowsCommand(command);
  const isWindowsCommandScript = process.platform === 'win32' && /\.(cmd|bat)$/i.test(resolvedCommand);
  const spawnCommand = isWindowsCommandScript ? (process.env.ComSpec || 'cmd.exe') : resolvedCommand;
  const spawnArgs = isWindowsCommandScript
    ? ['/d', '/s', '/c', ['call', quoteWindowsCommandArg(resolvedCommand), ...args.map(quoteWindowsCommandArg)].join(' ')]
    : args;

  return spawn(spawnCommand, spawnArgs, {
    cwd: cwdOverride || repoRoot,
    stdio: 'inherit',
    // 注意: extraEnv 必须展开合并, 不能让整个 options.env 覆盖掉 process.env
    // (否则子进程丢失 PATH → spawn bun/node ENOENT)。
    env: { ...process.env, OPENCHAMBER_TAURI_DEV: '1', ...extraEnv },
    detached: useDetachedChildren,
    windowsVerbatimArguments: isWindowsCommandScript,
    ...rest,
  });
}

function waitForExit(child, timeoutMs) {
  return new Promise((resolve) => {
    if (!child || child.exitCode !== null || child.signalCode !== null) {
      resolve();
      return;
    }

    const onExit = () => {
      clearTimeout(timer);
      resolve();
    };

    const timer = setTimeout(() => {
      child.off('exit', onExit);
      resolve();
    }, timeoutMs);

    child.once('exit', onExit);
  });
}

function signalChild(child, signal) {
  if (!child || child.exitCode !== null || child.signalCode !== null) {
    return;
  }

  // Windows: 用 taskkill /T 杀掉整个进程树, 确保 Vite/nodemon 等孙子进程也被清理。
  // 不能只 child.kill(signal), 因为 Windows 上那只是 TerminateProcess 立即子进程,
  // 孙子进程 (Vite, nodemon, 后端 watch 等) 会变成孤儿继续跑。
  if (process.platform === 'win32') {
    try {
      spawnSync('taskkill', ['/F', '/T', '/PID', String(child.pid)], {
        windowsHide: true,
        timeout: 3000,
      });
      return;
    } catch {
      // fall through to child.kill
    }
  }

  try {
    if (useDetachedChildren) {
      process.kill(-child.pid, signal);
      return;
    }
  } catch {
  }

  try {
    child.kill(signal);
  } catch {
  }
}

async function stopChildTree(child) {
  if (!child || child.exitCode !== null || child.signalCode !== null) {
    return;
  }

  signalChild(child, 'SIGINT');
  await waitForExit(child, 2500);

  if (child.exitCode === null && child.signalCode === null) {
    signalChild(child, 'SIGTERM');
    await waitForExit(child, 2500);
  }

  if (child.exitCode === null && child.signalCode === null) {
    signalChild(child, 'SIGKILL');
    await waitForExit(child, 1000);
  }
}

// 轮询端口就绪: vite 启动后 :uiPort 才会监听。
function isPortListening(port) {
  return new Promise((resolve) => {
    const socket = new net.Socket();
    socket.setTimeout(500);
    socket.once('connect', () => {
      socket.destroy();
      resolve(true);
    });
    socket.once('error', () => {
      socket.destroy();
      resolve(false);
    });
    socket.once('timeout', () => {
      socket.destroy();
      resolve(false);
    });
    socket.connect(port, '127.0.0.1');
  });
}

async function waitForPort(port, timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await isPortListening(port)) {
      return true;
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  return false;
}

async function main() {
  // 清除 Vite 缓存 (同 dev-web-hmr.mjs 的行为)
  const cacheDirs = [
    path.join(webRoot, 'node_modules/.vite'),
    path.join(webRoot, 'node_modules/.vite-temp'),
  ];
  for (const dir of cacheDirs) {
    try { rmSync(dir, { recursive: true, force: true }); } catch {}
  }

  console.log(`[tauri:dev] starting Vite dev server (UI on :${uiPort})...`);
  // Tauri 模式下只起 Vite HMR, 不走 dev-web-hmr.mjs (后者还会启动 Node Express 后端 + OpenCode)。
  // Rust oc-server 将处理所有 API 请求并管理 OpenCode。
  const vite = spawnProcess('bun', ['x', 'vite', '--force', '--host', hmrHost, '--port', uiPort, '--strictPort'], {
    cwd: webRoot,
    env: {
      OPENCHAMBER_DISABLE_PWA_DEV: '1',
      OPENCHAMBER_PORT: apiPort,
    },
  });

  vite.on('error', (error) => {
    console.error('[tauri:dev] failed to start Vite:', error);
    process.exit(1);
  });

  console.log(`[tauri:dev] waiting for Vite on :${uiPort} (up to 60s)...`);
  const ready = await waitForPort(Number(uiPort), 60_000);
  if (!ready) {
    console.error(`[tauri:dev] Vite did not come up on :${uiPort} within 60s, aborting.`);
    await stopChildTree(vite);
    process.exit(1);
  }
  console.log(`[tauri:dev] Vite ready on :${uiPort}, launching tauri...`);

  const backendMode = process.env.OPENCHAMBER_SIDECAR === '1' ? 'sidecar' : 'in-process';
  console.log(`[tauri:dev] backend mode: ${backendMode}`);

  // cargo tauri dev 会自己 cargo run, 不需要我们 build。
  // cwd 指向 src-tauri 让 tauri-cli 找到 tauri.conf.json。
  const tauri = spawnProcess('cargo', ['tauri', 'dev'], {
    cwd: tauriSrcDir,
    env: {
      GRIDFORGE_HMR_UI_URL: `http://127.0.0.1:${uiPort}`,
      OPENCHAMBER_PORT: apiPort,
    },
  });

  tauri.on('error', (error) => {
    console.error('[tauri:dev] failed to start tauri:', error);
  });

  let cleaning = false;
  const teardown = async (code) => {
    if (cleaning) return;
    cleaning = true;
    // 先停 tauri (它持有后端句柄), 再停 Vite。
    await stopChildTree(tauri);
    await stopChildTree(vite);
    process.exit(typeof code === 'number' ? code : 0);
  };

  const onChildExit = (label) => (code, signal) => {
    if (cleaning) return;
    if (code !== 0 || signal) {
      console.warn(`[tauri:dev] ${label} exited with code ${code ?? 'null'} signal ${signal ?? 'none'}.`);
    }
    void teardown(code ?? 1);
  };

  vite.on('exit', onChildExit('Vite'));
  tauri.on('exit', onChildExit('tauri'));

  for (const [signal, exitCode] of Object.entries({ SIGINT: 0, SIGTERM: 143, SIGHUP: 129 })) {
    process.on(signal, () => {
      void teardown(exitCode);
    });
  }
}

main().catch((error) => {
  console.error('[tauri:dev] unexpected error:', error);
  process.exit(1);
});
