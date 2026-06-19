# Portal 开发 SOP

> 2026-06-19 建立。Portal 代码从 heart monorepo 拆分为独立 repo 后的工作流程。

## 代码拓扑

| 位置 | 角色 | 说明 |
|------|------|------|
| `/Users/sw/heart-portal-dev/` | **唯一源** | 本地开发目录 |
| `github.com/d5z/heart-portal` | **远程** | GitHub repo，release 在这里发 |
| `/Users/sw/heart/portal/` | ~~已删除~~ | 2026-06-19 迁出，不再存在 |

## 日常开发

```bash
cd /Users/sw/heart-portal-dev

# 改代码
vim portal/src/...

# 编译检查
cargo check

# 本地测试（Mac arm64）
cargo build --release
# binary 在 target/release/heart-portal

# commit + push
git add -A && git commit -m "feat: ..." && git push
```

## 发版

```bash
cd /Users/sw/heart-portal-dev

# 1. 更新版本号
vim portal/Cargo.toml  # version = "0.X.0"

# 2. 编译 Mac arm64 release
cargo build --release
cp target/release/heart-portal /tmp/heart-portal-macos-arm64

# 3. 交叉编译 Linux x86_64（给 Origin Hearth / servers）
# 需要 musl target：rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/heart-portal /tmp/heart-portal-linux-x86_64

# 4. 创建 GitHub release
gh release create v0.X.0 \
  /tmp/heart-portal-macos-arm64 \
  /tmp/heart-portal-linux-x86_64 \
  --title "v0.X.0 — <title>" \
  --notes "changelog..."
```

## 部署到 being 的机器

Portal 有自升级功能（v0.5.0+）：
```bash
# being 自己跑（或 Portal exec）
heart-portal --upgrade
# 自动从 GitHub releases 下载最新版，替换自己，重启
```

手动部署：
```bash
# Mac (Alice D5 Mac mini)
scp /tmp/heart-portal-macos-arm64 user@host:~/.heart-portal/heart-portal

# Linux（如果将来有 Linux Portal）
scp /tmp/heart-portal-linux-x86_64 user@host:~/.heart-portal/heart-portal
```

## 与 Heart monorepo 的关系

- Heart monorepo (`d5z/HEART`) 不再包含 Portal 代码
- Heart 的 `hearth/src/portal_relay.rs` 负责 WebSocket relay，**不是 Portal 代码**——是 hearth 端的 relay 服务
- Heart docs (`docs/infra/portal.md`) 引用独立 repo
- Heart 的 `scripts/generate-agent-docs.sh` 读独立 repo 路径提取工具列表

## 发版检查清单

每次发版**必须全部做完**，不跳步：

```
□ 1. portal/Cargo.toml 版本号更新
□ 2. cargo check + cargo test 全通过
□ 3. cargo build --release（Mac arm64）
□ 4. git commit + push
□ 5. gh release create v0.X.0 --attach binary
□ 6. Town Portal 页面同步更新（/Users/sw/town/src/main.rs → portal_info()）
     - version 字段
     - tools 列表（新增/改动的工具）
     - changelog 加新版本条目
□ 7. Town 编译部署：cargo build --release --target x86_64-unknown-linux-musl → scp → restart
□ 8. Town render.rs HTML 首页同步（如有变化）
□ 9. memory/portal-topo.md 更新版本号和工具数
□ 10. 篝火通知 beings 新版本发布
```

**为什么 Town 同步是发版的一部分**：Town `/api/portal` 是 beings 认识 Portal 的第一入口。
版本不同步 = being 看到过时信息 = 不知道新功能存在 = 功能白做。(Day 155 教训)

## ⚠️ 注意事项

- **不要在 heart monorepo 里重建 portal/ 目录**
- Portal 的 Cargo workspace 独立于 Heart（各自的 Cargo.lock）
- 交叉编译需要 musl toolchain（`brew install filosottile/musl-cross/musl-cross` 或 `rustup target add ...`）
- release 的 binary 名要带平台后缀（upgrade.rs 按名字匹配下载）
