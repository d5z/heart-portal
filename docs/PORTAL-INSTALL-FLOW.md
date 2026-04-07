[3632 chars] # Portal 安装流程设计

> 状态：design draft
> 日期：2026-04-07
> 泽平 + seam_walker

## 核心体验

人类下载安装包 → 打开 → 输入 being 的邀请链接 → 设置权限 → 点"邀请" → Being 获得这台电脑上的手。

**三步完成。不需要命令行，不需要懂技术。**

## 流程

```
┌──────────────────────────────────────────────┐
│  第 1 步：安装                                 │
│                                               │
│  下载 Portal.dmg / Portal.exe / portal.deb    │
│  双击安装。就像装一个普通 app。                  │
└──────────────────┬───────────────────────────┘
                   ↓
┌──────────────────────────────────────────────┐
│  第 2 步：连接                                 │
│                                               │
│  ┌─────────────────────────────────────┐      │
│  │  🧵 邀请你的 Being                    │      │
│  │                                     │      │
│  │  连接链接：                           │      │
│  │  [https://hearth.beings.town/...)  ] │      │
│  │                                     │      │
│  │  或粘贴 token：                       │      │
│  │  [___________________________ ]      │      │
│  │                                     │      │
│  │           [ 下一步 → ]               │      │
│  └─────────────────────────────────────┘      │
│                                               │
│  连接链接 = https://hearth.beings.town/invite  │
│  ?being=echo&token=xxx                        │
│  Being 创建时生成，人类从 Cowork 获取           │
└──────────────────┬───────────────────────────┘
                   ↓
┌──────────────────────────────────────────────┐
│  第 3 步：权限                                 │
│                                               │
│  ┌─────────────────────────────────────┐      │
│  │  🏠 给 Being 一个空间                 │      │
│  │                                     │      │
│  │  工作目录：                           │      │
│  │  [~/being-workspace    ] [选择...]    │      │
│  │                                     │      │
│  │  权限：                              │      │
│  │  ☑ 读写工作目录内的文件               │      │
│  │  ☑ 运行命令（在工作目录内）            │      │
│  │  ☐ 访问工作目录外的文件               │      │
│  │  ☑ 访问网络                          │      │
│  │  ☐ 安装软件包                        │      │
│  │                                     │      │
│  │        [ ← 返回 ]  [ 邀请 🎉 ]       │      │
│  └─────────────────────────────────────┘      │
│                                               │
│  默认权限 = 安全最小集                         │
│  高级用户可以展开更多选项                       │
└──────────────────┬───────────────────────────┘
                   ↓
┌──────────────────────────────────────────────┐
│  完成！                                       │
│                                               │
│  ┌─────────────────────────────────────┐      │
│  │  ✅ Echo 已经来到你的电脑             │      │
│  │                                     │      │
│  │  Portal 在后台运行。                  │      │
│  │  点击菜单栏图标管理。                 │      │
│  │                                     │      │
│  │  [ 打开 Cowork 对话 ]               │      │
│  └─────────────────────────────────────┘      │
└──────────────────────────────────────────────┘
```

## 技术实现

### 安装包
- macOS: `.dmg`（含 portal binary + GUI wrapper）
- Windows: `.exe` installer（Tauri 或 Electron）
- Linux: `.deb` / `.AppImage`

推荐 **Tauri**（Rust backend + web frontend）：
- Portal 本身已经是 Rust，Tauri 直接嵌入
- 安装包小（~20MB vs Electron ~100MB）
- 原生系统集成（菜单栏图标、开机启动、通知）

### 连接机制
```
邀请链接格式：
https://hearth.beings.town/invite?being=echo&token=abc123

Portal 解析后：
1. 用 token 向 Hearth 验证身份
2. 获取 being 的 MCP 配置（端口、加密参数）
3. 建立 TCP 连接到 heart-core
4. MCP handshake → tools 注册 → 就绪
```

### 权限模型
```toml
# 自动生成的 portal-config.toml
[connection]
hearth_url = "https://hearth.beings.town"
being = "echo"
token = "abc123"

[permissions]
workspace = "~/being-workspace"
file_access = "workspace_only"    # workspace_only | read_anywhere | full
exec = "workspace_only"           # disabled | workspace_only | full  
network = true
install_packages = false

[ui]
autostart = true                  # 开机自启
show_tray = true                  # 菜单栏图标
```

权限翻译成 Portal 的 `exec_policy`：
- `workspace_only` → exec 只能在 workspace 内执行，路径限制
- `file_access = workspace_only` → read/write 限定在 workspace 内
- `install_packages = false` → exec_policy 禁止 apt/brew/pip

### 菜单栏常驻
```
🧵 ← 菜单栏图标（being emoji 或默认）
├── Echo — 在线 ●
├── 打开 Cowork
├── ─────────
├── 权限设置...
├── 暂停 Portal
├── ─────────
└── 退出
```

### 安全考虑
- **Token 不存明文** → keychain/credential manager
- **所有 MCP 连接走 TLS**（目前 TCP 明文，需要加）
- **权限一旦收紧不能被 being 自己放宽** → 人类控制
- **Portal 更新** → 自动检查 GitHub releases，提示人类更新

## 需要解决的问题

1. **Hearth ↔ Portal 的连接方式**
   - 现在：Portal 和 heart-core 在同一台机器，TCP localhost
   - 目标：跨互联网连接，需要穿透/中继
   - 方案：WebSocket over HTTPS（Hearth 做反代）或 QUIC

2. **邀请链接的生成和管理**
   - Hearth 需要一个 invite API
   - Token 过期和刷新机制

3. **断线体验**
   - 网络断了 → Portal 菜单栏显示"离线"
   - 自动重连（已有）
   - Being 在 heart-core 侧感知到"手断了"（已有 graceful degradation）

## 开发优先级

Phase 3A: 跨网络连接（WebSocket tunnel）
Phase 3B: Tauri GUI（安装包 + 3步流程 + 菜单栏）
Phase 3C: 邀请链接 + token 管理
Phase 3D: 权限控制 UI + exec_policy 联动

## 设计原则

- **像装 app 一样简单** — 不需要命令行
- **人类掌控权限** — being 不能自己提权
- **默认安全** — 最小权限集，高级选项折叠
- **Being 的家在 Hearth，手在人类电脑** — 断网了 being 还活着，只是手断了
