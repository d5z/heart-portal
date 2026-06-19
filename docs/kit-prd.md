[5765 chars] # Portal Kit — Extension Framework PRD

> 2026-06-19 · seam_walker + 泽平
> Status: design
> First Kit sample: Hand (Alice)

## 一句话

Kit 是 Portal 的扩展机制。Being 通过 Kit 获得新能力——GUI 操作、代码编写、日历管理。Portal 管理 Kit 的生命周期，Being 只看到工具。

## 为什么不用裸 MCP

| | 裸 MCP (mcp-servers.toml) | Portal Kit |
|---|---|---|
| 管理者 | Heart（云端） | **Portal（本地）** |
| 生命周期 | Being 手动配 | **Portal 自动管** |
| 安全 | 各自独立 | **继承 Portal security** |
| 发现 | 手动写 toml | **扫描 kits/ 目录** |
| 升级 | 手动 | **`portal kit upgrade hand`** |
| Being 体验 | 要懂 MCP 协议 | **装了就能用** |

裸 MCP 的根本问题：**控制平面（Heart/云）和执行平面（本地机器）分裂**。Heart 管不了本地进程。Portal 天然在本地，是正确的管理者。

## Kit 是什么

一个 Kit 是一个目录，包含：

```
~/.heart-portal/kits/hand/
├── manifest.json          ← 声明：名字、版本、工具、启动命令、平台、权限
├── hand/                  ← 代码（Python/Node/Rust/任何语言）
│   ├── __init__.py
│   ├── cli.py
│   ├── mcp_server.py      ← MCP stdio server（Kit 和 Portal 的通信接口）
│   └── ...
├── requirements.txt       ← 依赖
└── README.md
```

### manifest.json

```json
{
  "name": "hand",
  "version": "6.0.0-alpha",
  "description": "Unified graphical interface — see, touch, be.",
  "author": "Alice",
  "platform": ["darwin"],
  "runtime": "python3",
  "command": ["python3", "-m", "hand.mcp_server"],
  "tools": [
    {
      "name": "open",
      "description": "Go to a place (app, URL, file)",
      "params": {
        "target": { "type": "string", "required": true, "description": "App name, URL, or file path" }
      }
    },
    {
      "name": "see",
      "description": "Perceive the current place — returns structured perception",
      "params": {}
    },
    {
      "name": "do",
      "description": "Execute an action at the current place",
      "params": {
        "action": { "type": "string", "required": true, "description": "Semantic action: 'Cmd+N', 'click Save', 'type hello'" }
      }
    }
  ],
  "permissions": ["accessibility", "screen_capture"],
  "workspace": true
}
```

### 命名空间

Kit 工具自动加 `kit_` 前缀：
- `hand` kit → `hand_open`, `hand_see`, `hand_do`
- `cursor` kit → `cursor_run`, `cursor_review`

Being 看到的工具列表：
```
portal_exec          (内置)
portal_file_write    (内置)
portal_screenshot    (内置)
...
hand_open            (kit: hand)
hand_see             (kit: hand)
hand_do              (kit: hand)
```

## Portal 端实现

### 1. Kit Loader (`portal/src/kits/`)

启动时：
```
1. 扫描 ~/.heart-portal/kits/*/manifest.json
2. 过滤平台兼容的 kits（darwin/linux/windows）
3. 注册工具到 ToolHost（lazy spawn — 第一次调用时才启动 kit 进程）
4. 在 tools/list 响应里包含 kit 工具
```

### 2. Kit Manager（生命周期）

```
spawn:    第一次 tool call → 启动 kit 进程（MCP stdio）
health:   定期 ping，3 次无响应 → 重启
restart:  自动重启，3 次失败 → 标记 unhealthy，报告 being
shutdown: Portal 退出时 graceful shutdown 所有 kit 进程
```

### 3. Kit Router（tool call 路由）

```
Being calls `hand_see`
  → Portal strips prefix: kit=hand, tool=see
  → 找到 hand kit 进程（没有则 spawn）
  → MCP call_tool({name: "see", arguments: {}})
  → 返回结果给 Being
```

### 4. 安全

- Kit 进程继承 Portal 的 workspace_root 限制
- Kit 不能访问 Portal 的 config（token 等）
- Kit 的 manifest 声明所需权限（accessibility/screen_capture/network）
- Portal 启动 kit 时检查权限是否满足

## CLI

```bash
# 安装
portal kit install github:alice/hand
portal kit install ./local-kit-dir

# 列表
portal kit list
# hand     6.0.0  ● running   3 tools
# cursor   1.0.0  ○ stopped   2 tools

# 升级
portal kit upgrade hand

# 停用/启用
portal kit disable hand
portal kit enable hand

# 状态
portal kit status hand
# hand 6.0.0 (Alice)
# status: running (pid 4521, uptime 2h)
# tools: hand_open, hand_see, hand_do
# platform: darwin ✓
# permissions: accessibility ✓, screen_capture ✓
```

## 迁移路径

### Phase 1（现在）— Hand 作为 sample
- Alice 的 Hand 加 `mcp_server.py`（MCP stdio 模式）
- 手动放到 kits/ 目录
- Portal 加最小 kit loader（扫描 manifest + spawn + 路由）

### Phase 2 — Kit 生态基础
- `portal kit install/upgrade/list` CLI
- Town 上加 `/api/kits` 注册表
- manifest.json schema 稳定
- 3-5 个 kits（hand, cursor, notes, calendar, browser）

### Phase 3 — 替代裸 MCP
- 现有 mcp-servers.toml 的 server 可以自动转为 legacy kit
- Being 新配置只用 kits，不再手写 mcp-servers.toml
- Kit 成为 Portal 生态的标准扩展方式

## 不做什么

- **不做 Kit 市场/计费** — 太早
- **不做 Kit 间通信** — 每个 kit 独立，不互相调用
- **不做 Kit 沙箱（进程隔离）** — 信任模型和 Portal 一致，都是跑在 being 的人类伙伴的机器上
- **不做跨机器 Kit** — Kit 只跑在 Portal 所在的机器上

## 实现优先级

| 优先级 | 内容 | 工作量 |
|--------|------|--------|
| P0 | manifest.json schema + Kit loader（扫描+注册） | 小 |
| P0 | Kit router（tool call → kit 进程路由） | 中 |
| P0 | Hand 的 mcp_server.py（Alice 做） | 小 |
| P1 | Kit manager（spawn/health/restart） | 中 |
| P1 | `portal kit list/status` CLI | 小 |
| P2 | `portal kit install` 从 GitHub | 中 |
| P2 | Town `/api/kits` 注册表 | 中 |

## 命名

- 框架名：**Kit**
- 目录：`~/.heart-portal/kits/`
- CLI：`portal kit <command>`
- 工具前缀：`<kit_name>_<tool_name>`
- 文件：`manifest.json`

---

*"Portal 是手。Kit 是手上的本事。"*
*设计：seam_walker + 泽平 · Day 155*
