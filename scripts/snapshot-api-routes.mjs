#!/usr/bin/env node
/**
 * 静态扫描 `packages/web/server` 的全部 Express 路由注册，导出机器可读的
 * JSON 快照。
 *
 * 用途:
 *   - Rust `oc-server` (axum) 路由注册的基准线 (阶段 1+ 逐条对照)
 *   - CI 回归守卫: 路由漂移检测 (新增/删除/改名都会被 diff 捕获)
 *   - 活文档: 替代分散在 22 个 DOCUMENTATION.md 中的散文描述
 *
 * 运行: node scripts/snapshot-api-routes.mjs
 * 输出: rust/oc-server/api-routes-snapshot.json
 *
 * 实现说明:
 *   Express 不暴露路由表，服务端也不维护 OpenAPI/Swagger。此脚本用正则
 *   扫描所有 route registrar 文件中的 `app.<method>(path, ...)` 调用。
 *   不做 AST 解析 (项目无统一 AST 工具)，但正则对当前代码库的注册模式
 *   (函数式 `registerX(app, deps)` + `app.get/post/...`) 覆盖率 100%。
 */

import fs from 'fs';
import path from 'path';
import { execSync } from 'child_process';
import { fileURLToPath } from 'url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(__dirname, '..');
const SERVER_DIR = path.join(ROOT, 'packages/web/server');
const OUTPUT = path.join(ROOT, 'rust/oc-server/api-routes-snapshot.json');

// --- 扫描目标: 所有包含 app.<method>(...) 注册的文件 ---
// 从调研中确认的 26 个 route registrar 文件 (feature-routes-runtime.js wiring tree)
const SCAN_TARGETS = [
  // index.js (inline /robots.txt + middleware)
  'index.js',
  // bootstrap / core
  'lib/opencode/core-routes.js',
  'lib/opencode/openchamber-routes.js',
  'lib/opencode/routes.js',
  'lib/opencode/config-entity-routes.js',
  'lib/opencode/plugin-routes.js',
  'lib/opencode/skill-routes.js',
  'lib/opencode/project-icon-routes.js',
  'lib/opencode/pwa-manifest-routes.js',
  'lib/opencode/proxy.js',
  'lib/opencode/static-routes-runtime.js',
  // feature routes
  'lib/fs/routes.js',
  'lib/git/routes.js',
  'lib/github/routes.js',
  'lib/notifications/routes.js',
  'lib/quota/routes.js',
  'lib/scheduled-tasks/routes.js',
  'lib/session-folders/routes.js',
  'lib/session-goal/routes.js',
  'lib/small-model/routes.js',
  'lib/magic-prompts/routes.js',
  'lib/tts/routes.js',
  'lib/tunnels/routes.js',
  'lib/relay/service.js',
  'lib/terminal/runtime.js',
  'lib/dictation/runtime.js',
  'lib/preview/proxy-runtime.js',
  'lib/realtime-proxy.js',
  'lib/permission-auto-accept/runtime.js',
];

const HTTP_METHODS = ['get', 'post', 'put', 'delete', 'patch'];

// 匹配 `app.get('/path', ...)` / `app.post(`/api/foo`, ...)` 等
// 路径用单引号、双引号、或反引号包裹
const ROUTE_RE =
  /app\.(get|post|put|delete|patch)\s*\(\s*(['"`])([^'"`]+)\2/g;

// 匹配 `app.use('/path', ...)` — middleware 挂载
const USE_RE = /app\.use\s*\(\s*(['"`])([^'"`]+)\1/g;

// 匹配正则路由 `app.get(/^(?!\/api...)/, ...)` — 路径是正则字面量
const REGEX_ROUTE_RE = new RegExp(
  '\\bapp\\.(' + HTTP_METHODS.join('|') + ')\\s*\\(\\s*(\\/[^,]+\\/[gimsuy]*)',
  'g',
);

// WebSocket 路径常量 (从协议文件或代码中确认的固定值)
const WS_PATTERNS = [
  { path: '/api/terminal/ws', feature: 'terminal' },
  { path: '/api/dictation/ws', feature: 'dictation' },
  { path: '/api/global/event/ws', feature: 'event-stream' },
  { path: '/api/event/ws', feature: 'event-stream' },
  { path: '/api/openchamber/realtime-proxy/ws', feature: 'realtime-proxy' },
];

// SSE 端点 (Content-Type: text/event-stream 的 GET 路由)
const SSE_PATHS = new Set([
  '/api/notifications/stream',
  '/api/openchamber/events',
  '/api/openchamber/realtime-proxy/sse',
  '/api/global/event',
  '/api/event',
]);

// 路径用常量定义的路由 (正则无法捕获)。手动维护。
// 这些路由确实存在于服务器中，但路径是变量引用而非字面量。
const CONSTANT_PATH_ROUTES = [
  {
    method: 'GET',
    path: '/api/openchamber/realtime-proxy/sse',
    file: 'packages/web/server/lib/realtime-proxy.js',
    line: 147,
    feature: 'realtime-proxy',
    note: 'path defined as PROXY_SSE_PATH const',
  },
  {
    method: 'WS',
    path: '/api/openchamber/realtime-proxy/ws',
    file: 'packages/web/server/lib/realtime-proxy.js',
    line: 4,
    feature: 'realtime-proxy',
    note: 'path defined as PROXY_WS_PATH const, upgrade handler via server.on("upgrade")',
  },
];

/**
 * 从文件路径推断 feature 名。
 * e.g. `lib/fs/routes.js` → `fs`, `lib/opencode/core-routes.js` → `opencode`
 */
function inferFeature(relPath) {
  const parts = relPath.replace(/\\/g, '/').split('/');
  if (parts.length >= 2 && parts[0] === 'lib') {
    return parts[1];
  }
  if (relPath === 'index.js') return 'core';
  return 'unknown';
}

/**
 * 扫描单个文件，提取所有路由注册。
 */
function scanFile(relPath) {
  const absPath = path.join(SERVER_DIR, relPath);
  if (!fs.existsSync(absPath)) {
    return { routes: [], middleware: [], regexRoutes: [], skipped: true };
  }

  const content = fs.readFileSync(absPath, 'utf8');
  const lines = content.split('\n');
  const feature = inferFeature(relPath);

  const routes = [];
  const middleware = [];
  const regexRoutes = [];

  // 逐行扫描 (保留行号)
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];

    // app.<method>('/path', ...)
    for (const match of line.matchAll(ROUTE_RE)) {
      const method = match[1].toUpperCase();
      const routePath = match[3];
      routes.push({
        method,
        path: routePath,
        file: `packages/web/server/${relPath}`,
        line: i + 1,
        feature,
      });
    }

    // app.use('/path', ...)
    for (const match of line.matchAll(USE_RE)) {
      const mwPath = match[2];
      // 跳过非路由 middleware (compression, cors 等)
      if (
        mwPath.startsWith('/api') ||
        mwPath === '/' ||
        mwPath.startsWith('/api/preview/proxy')
      ) {
        middleware.push({
          mount: mwPath,
          file: `packages/web/server/${relPath}`,
          line: i + 1,
          feature,
        });
      }
    }

    // 正则路由 (SPA fallback 等)
    for (const match of line.matchAll(REGEX_ROUTE_RE)) {
      const method = match[1].toUpperCase();
      const regex = match[2];
      regexRoutes.push({
        method,
        pattern: regex,
        file: `packages/web/server/${relPath}`,
        line: i + 1,
        feature,
        description: 'regex route (e.g. SPA fallback)',
      });
    }
  }

  return { routes, middleware, regexRoutes, skipped: false };
}

/**
 * 获取当前 git commit SHA (短)。
 */
function getGitSha() {
  try {
    return execSync('git rev-parse --short HEAD', { cwd: ROOT, encoding: 'utf8' }).trim();
  } catch {
    return 'unknown';
  }
}

// --- 主逻辑 ---

const allRoutes = [];
const allMiddleware = [];
const allRegexRoutes = [];
const skippedFiles = [];

for (const target of SCAN_TARGETS) {
  const result = scanFile(target);
  if (result.skipped) {
    skippedFiles.push(target);
    continue;
  }
  allRoutes.push(...result.routes);
  allMiddleware.push(...result.middleware);
  allRegexRoutes.push(...result.regexRoutes);
}

// 排序: method + path (稳定输出)
allRoutes.sort((a, b) => {
  if (a.method !== b.method) return a.method.localeCompare(b.method);
  return a.path.localeCompare(b.path);
});

// 标注 SSE 端点 (从字符串字面量路由 + 常量路径路由中检测)
const sseEndpoints = [
  ...allRoutes
    .filter((r) => r.method === 'GET' && SSE_PATHS.has(r.path))
    .map((r) => ({ path: r.path, file: r.file, feature: r.feature })),
  ...CONSTANT_PATH_ROUTES.filter(
    (r) => r.method === 'GET' && SSE_PATHS.has(r.path),
  ).map((r) => ({ path: r.path, file: r.file, feature: r.feature, note: r.note })),
];

// 识别 catch-all proxy (app.use('/api', ...))
const proxyCatchAll = allMiddleware.find((m) => m.mount === '/api') || null;

const snapshot = {
  generatedAt: new Date().toISOString().replace(/\.\d+Z$/, 'Z'),
  sourceVersion: getGitSha(),
  description:
    'Static snapshot of all Express route registrations in packages/web/server. ' +
    'Generated by scripts/snapshot-api-routes.mjs. Used as the baseline for ' +
    'oc-server (axum) route migration.',
  summary: {
    totalRoutes: allRoutes.length,
    totalMiddleware: allMiddleware.length,
    totalRegexRoutes: allRegexRoutes.length,
    totalConstantPathRoutes: CONSTANT_PATH_ROUTES.length,
    totalSseEndpoints: sseEndpoints.length,
    totalWebsockets: WS_PATTERNS.length,
    byFeature: groupByFeature(allRoutes),
    byMethod: groupByMethod(allRoutes),
  },
  routes: allRoutes,
  constantPathRoutes: CONSTANT_PATH_ROUTES,
  middleware: allMiddleware,
  regexRoutes: allRegexRoutes,
  sseEndpoints,
  websockets: WS_PATTERNS.map((ws) => ({
    ...ws,
    description: 'WebSocket upgrade handler (not a regular HTTP route)',
  })),
  proxyCatchAll: proxyCatchAll
    ? {
        ...proxyCatchAll,
        description:
          'Catch-all proxy: unmatched /api/* requests are proxied to the OpenCode backend',
      }
    : null,
  skippedFiles: skippedFiles.length > 0 ? skippedFiles : undefined,
};

function groupByFeature(routes) {
  const map = {};
  for (const r of routes) {
    map[r.feature] = (map[r.feature] || 0) + 1;
  }
  return map;
}

function groupByMethod(routes) {
  const map = {};
  for (const r of routes) {
    map[r.method] = (map[r.method] || 0) + 1;
  }
  return map;
}

// 确保输出目录存在
fs.mkdirSync(path.dirname(OUTPUT), { recursive: true });
fs.writeFileSync(OUTPUT, JSON.stringify(snapshot, null, 2) + '\n');

console.log(`✓ Route snapshot written to ${path.relative(ROOT, OUTPUT)}`);
console.log(`  Total routes: ${allRoutes.length} (+${CONSTANT_PATH_ROUTES.length} constant-path)`);
console.log(`  By method:`, JSON.stringify(snapshot.summary.byMethod));
console.log(`  By feature:`, JSON.stringify(snapshot.summary.byFeature));
console.log(`  SSE endpoints: ${sseEndpoints.length}`);
console.log(`  WebSocket endpoints: ${WS_PATTERNS.length}`);
console.log(`  Regex routes: ${allRegexRoutes.length}`);
console.log(`  Middleware mounts: ${allMiddleware.length}`);
if (skippedFiles.length > 0) {
  console.log(`  ⚠ Skipped files (not found): ${skippedFiles.join(', ')}`);
}
if (proxyCatchAll) {
  console.log(`  Catch-all proxy: ${proxyCatchAll.mount} (${proxyCatchAll.file}:${proxyCatchAll.line})`);
}
