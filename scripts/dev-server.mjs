#!/usr/bin/env node
/**
 * Cross-platform dev server runner.
 *
 * Bypasses the shell variable expansion issue in npm scripts on Windows:
 * `${GRIDFORGE_PORT:-${OPENCHAMBER_PORT:-3001}}` works in bash/sh but not in cmd.exe.
 * This script resolves the port in JS and runs nodemon directly.
 *
 * Uses the platform shell explicitly to ensure PATH resolution and
 * proper quoting of the nodemon --exec value.
 */
import { spawn } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const repoRoot = path.resolve(__dirname, '..');
const webRoot = path.join(repoRoot, 'packages/web');
// GRIDFORGE_PORT 是 dev launcher / Vite / Rust 三方约定的统一环境变量。
// OPENCHAMBER_PORT 保留为旧用户脚本的兼容回退。
const port = process.env.GRIDFORGE_PORT ?? process.env.OPENCHAMBER_PORT ?? '3001';
const serverDir = path.join(webRoot, 'server');
const serverEntry = path.join(webRoot, 'server/index.js');

// Use platform shell so PATH (including bun .cmd shim) is resolved,
// and quote the --exec value so nodemon receives it as one argument.
const shell = process.platform === 'win32'
  ? { command: 'cmd.exe', arg: '/c' }
  : { command: 'sh', arg: '-c' };

// Quotes around the --exec value ensure nodemon receives it as a single arg.
const cmdLine = `bun x nodemon --watch "${serverDir}" --ext js --exec "bun ${serverEntry} --port ${port}"`;

const child = spawn(shell.command, [shell.arg, cmdLine], {
  cwd: webRoot,
  stdio: 'inherit',
  // 端口已通过 --port 传给 nodemon,无需再用 env 重复。
  env: process.env,
});

child.on('exit', (code, signal) => {
  process.exit(code ?? (signal ? 1 : 0));
});

child.on('error', (error) => {
  console.error('[dev-server] Failed to start:', error);
  process.exit(1);
});
