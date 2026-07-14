# OpenChamber Rust workspace (迁移中)

渐进式迁移 `packages/web/server` (Express → axum) 与 `packages/electron`
(Electron → Tauri) 到 Rust。完整计划见
[`docs/plan/rust-migration-plan.md`](../docs/plan/rust-migration-plan.md)。

## 结构

| Crate | 角色 | 替换目标 |
|---|---|---|
| `oc-core` | 共享类型与错误 | (新建) |
| `oc-opencode-sdk` | OpenCode 服务端客户端 | `@opencode-ai/sdk` (服务端用法) |
| `oc-server` | axum 后端二进制 | `packages/web/server` |
| `oc-tauri` | Tauri 桌面壳 | `packages/electron` |

## 构建

```bash
cd rust
cargo check                 # 类型检查
cargo run -p oc-server      # 启动后端 (阶段 0: 仅 /health)
cargo run -p oc-tauri       # 桌面壳占位 (阶段 4 才接入 Tauri 运行时)
```

## 当前进度

**阶段 0 — 脚手架** (进行中):
- [x] cargo workspace + 4 个 crate 骨架
- [x] Win10 编译验证 (`cargo check`)
- [ ] `/api` 契约快照 (路由清单 + TS schema 导出)
- [ ] 前置解耦: 抽出 `mintOutsideFileGrant` (解锁桌面壳独立迁移)
