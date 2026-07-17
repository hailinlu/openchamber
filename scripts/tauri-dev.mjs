#!/usr/bin/env node
// 一键启动 Tauri 桌面壳开发环境 (对标 packages/electron/scripts/electron-dev.mjs)。
//
// 流程:
//   1. 起 web dev server (scripts/dev-web-hmr.mjs) → UI HMR + Node 后端 on :5180
//   2. 等 :5180 就绪
//   3. 起 `cargo tauri dev` (默认进程内嵌 oc-server; OPENCHAMBER_SIDECAR=1 走 sidecar 回退)
//   4. 退出时整树清理 (SIGINT/SIGTERM/SIGHUP 或任一子进程退出)
//
// 已知良性噪音 (非 bug):
//   Ctrl+C 停止时, 终端可能打印 `error: script "dev:server:watch" exited with code 130`。
//   这是 bun 的固有行为 —— teardown 链 (tauri:dev → dev-web-hmr.mjs → nodemon) 把 SIGINT
//   逐级转发, nodemon 被信号杀死后以 130 退出, bun run 把非零退出码当 error 报。
//   属于用户主动停止的正常副作用, 不影响清理完整性 (stopChildTree 保证进程树回收)。
//
// 用法:
//   node scripts/tauri-dev.mjs              # 默认 (进程内嵌 oc-server)
//   OPENCHAMBER_SIDECAR=1 node scripts/tauri-dev.mjs   # sidecar 回退路径
//
// 端口:
//   UI:  OPENCHAMBER_HMR_UI_PORT  (默认 5180, 必须与 tauri.conf.json devUrl 一致)
//   API: OPENCHAMBER_HMR_API_PORT (默认 3902)

import { spawn } from 'node:child_process';
import net from 'node:net';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const repoRoot = path.resolve(__dirname, '..');
const tauriSrcDir = path.join(repoRoot, 'rust/oc-tauri/src-tauri');

// dev-web-hmr 默认 5180, 必须与 tauri.conf.json 的 devUrl 对齐。
const uiPort = process.env.OPENCHAMBER_HMR_UI_PORT || '5180';
const useDetachedChildren = process.platform !== 'win32';

// 用当前 node 的绝对路径 (process.execPath) 起 node 子进程,
// 避免在 detached 子进程里 PATH 解析不到 `node` (nvm 等场景)。
const nodeBin = process.execPath;

function spawnProcess(command, args, options = {}) {
  const { env: extraEnv = {}, ...rest } = options;
  return spawn(command, args, {
    cwd: repoRoot,
    stdio: 'inherit',
    // 注意: extraEnv 必须展开合并, 不能让整个 options.env 覆盖掉 process.env
    // (否则子进程丢失 PATH → spawn bun/node ENOENT)。
    env: { ...process.env, OPENCHAMBER_TAURI_DEV: '1', ...extraEnv },
    detached: useDetachedChildren,
    shell: process.platform === 'win32',
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
// 不依赖子进程 stdout 解析 (dev-web-hmr.mjs 输出格式可能变)。
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
  console.log(`[tauri:dev] starting web dev server (UI on :${uiPort})...`);
  const devServer = spawnProcess(nodeBin, ['./scripts/dev-web-hmr.mjs'], {
    env: {
      OPENCHAMBER_DISABLE_PWA_DEV: '1',
    },
  });

  devServer.on('error', (error) => {
    console.error('[tauri:dev] failed to start dev server:', error);
    process.exit(1);
  });

  console.log(`[tauri:dev] waiting for UI on :${uiPort} (up to 60s)...`);
  const ready = await waitForPort(Number(uiPort), 60_000);
  if (!ready) {
    console.error(`[tauri:dev] UI did not come up on :${uiPort} within 60s, aborting.`);
    await stopChildTree(devServer);
    process.exit(1);
  }
  console.log(`[tauri:dev] UI ready on :${uiPort}, launching tauri...`);

  const backendMode = process.env.OPENCHAMBER_SIDECAR === '1' ? 'sidecar' : 'in-process';
  console.log(`[tauri:dev] backend mode: ${backendMode}`);

  // cargo tauri dev 会自己 cargo run, 不需要我们 build。
  // cwd 指向 src-tauri 让 tauri-cli 找到 tauri.conf.json。
  const tauri = spawnProcess('cargo', ['tauri', 'dev'], {
    cwd: tauriSrcDir,
  });

  tauri.on('error', (error) => {
    console.error('[tauri:dev] failed to start tauri:', error);
  });

  let cleaning = false;
  const teardown = async (code) => {
    if (cleaning) {
      return;
    }
    cleaning = true;
    // 先停 tauri (它持有后端句柄), 再停 dev server。
    // tauri 内部的 cleanup 已由本次修复保证 (ExitRequested + signal handler)。
    await stopChildTree(tauri);
    await stopChildTree(devServer);
    process.exit(typeof code === 'number' ? code : 0);
  };

  const onChildExit = (label) => (code, signal) => {
    if (cleaning) return;
    if (code !== 0 || signal) {
      console.warn(`[tauri:dev] ${label} exited with code ${code ?? 'null'} signal ${signal ?? 'none'}.`);
    }
    void teardown(code ?? 1);
  };

  devServer.on('exit', onChildExit('dev server'));
  tauri.on('exit', onChildExit('tauri'));

  for (const [signal, exitCode] of Object.entries({ SIGINT: 130, SIGTERM: 143, SIGHUP: 129 })) {
    process.on(signal, () => {
      void teardown(exitCode);
    });
  }
}

main().catch((error) => {
  console.error('[tauri:dev] unexpected error:', error);
  process.exit(1);
});
