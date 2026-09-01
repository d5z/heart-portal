# Portal Async Callback — PRD v2

> Version: v2 (incorporates Opus-5 review of 21 issues)
> Status: approved
> Author: seam_walker
> Date: 2026-09-01

## 背景

Heart 已支持 Callback API（`POST /api/callback`）——外部系统推异步结果回 being，唤醒它并给予完整工具能力（v3 breathe_callback，Day 238 验证）。

Portal 的 `portal_exec` 有 `background: true` 模式（ProcessManager spawn + session_id 返回），但任务完成后**无法通知 being**。

### 历史：relay 通知路径（死代码）

Commit `348280b` 曾实现 `notifications/portal/task_complete` 通过 relay WSS 推送。代码完整：`task_complete_notification()` + `set_notification_sender()` + 两个通过的 unit test。但在 merge `dd47ce0` 中被意外断开——`set_notification_sender` 从未被调用。编译器确认 dead code。

### 设计决策：HTTP callback 而非 relay 通知

**选 HTTP，删 relay 通知代码。** 理由：
1. HTTP POST 写 inbox（持久化）——relay 断开不丢结果
2. 直接触发 `breathe_callback`——being 醒来有工具
3. relay 通知需要 Heart MCP 端处理 unsolicited frames（不存在的基础设施）
4. relay 频繁重连（heartbeat 90s + backoff 30s），通知窗口丢失率高
5. `notification_tx` 是单 `Option<Sender>`，每次重连需替换——状态管理复杂

## 目标

后台任务完成 → Portal 自动 POST Heart `/api/callback` → being 唤醒处理结果。

## 设计

### 信息流

```
Being breathe
  → tool_call: portal_exec(command="make test", background=true)
  → relay WSS → Portal
  → ProcessManager.spawn() → 返回 {session_id, status: "running"}
  → tool_result → breath 结束, 注意力释放

... N 分钟后 ...

ProcessManager 退出 watcher:
  → 构造 payload, drop locks, notify_waiters()
  → tokio::spawn detached POST task
  → POST https://host/being/api/callback
      Authorization: Bearer <loom_token>
      {source, task_id, summary, result}
  → Heart inbox + breathe_callback
```

### 1. Callback URL 推导

从 `--connect` Loom link 推导。`parse_loom_link` 返回 `(host, being_id, token)`。

```
Loom:     https://echo.beings.town/alice/?token=abc123
Callback: https://echo.beings.town/alice/api/callback
```

- scheme 跟随原 Loom link（不硬编码 https）
- localhost/127.* 用 http，其余用原 scheme
- token 通过 `Authorization: Bearer` header 传递（不放 query string，避免日志泄漏）
- `--connect` 未设置 → callback_url = None → 不回调（standalone 模式）

### 2. ProcessManager 改动

**新增 `set_callback_config(url: String, token: String)`**（类似已有的 `set_notification_sender` 模式）：
- 存为 `callback_config: Arc<Mutex<Option<(String, String)>>>`
- 从 `main.rs` 的 `--connect` 分支调用，在 relay handshake 之前

**退出 watcher 改动**（`process_manager.rs` spawn 块内）：
1. `child.wait()` → 收集 exit code
2. 构造 payload（持有 output lock）
3. **drop output lock**
4. `n_exit.notify_waiters()`（先通知 pollers）
5. 检查 `killed` flag → true 则跳过 callback
6. 检查 `callback_config` → Some 则 `tokio::spawn` detached POST

**killed flag**：`ManagedProcess` 新增 `killed: AtomicBool`。`kill()` 和 `kill_all()` 设 true。退出 watcher 检查后跳过 callback。解决 shutdown 假醒问题。

**HTTP client**：在 ProcessManager 构造时创建一个 `reqwest::Client`（复用连接池）：
```rust
reqwest::Client::builder()
    .timeout(Duration::from_secs(30))
    .connect_timeout(Duration::from_secs(10))
    .build()
```

### 3. Retry 策略

- **3 次尝试**（1 + 2 retries），指数退避 2s / 4s
- **只重试**：5xx、connect error、timeout。**不重试** 4xx（401/413 等）
- Heart 的 inbox `UNIQUE(source, source_seq)` 以 task_id 去重——重试不会产生重复唤醒
- 重试总预算 ~36s（足以覆盖短暂不可达，不覆盖长时间部署）
- 所有尝试失败 → `warn!("callback delivery failed for session {session_id}: {err}")` — URL 从日志中 redact token

### 4. Payload

```json
{
  "source": "portal",
  "task_id": "<session_id>",
  "summary": "portal_exec completed: '<command>' (exit <code>)",
  "result": {
    "session_id": "sess_xxx",
    "exit_code": 0,
    "output": "<tail output>",
    "command": "make test",
    "workdir": "/home/alice/project",
    "elapsed_secs": 42,
    "portal_name": "alice-laptop",
    "truncated": false,
    "total_output_bytes": 1234
  }
}
```

**Output 截取**：tail（不是 head）。`OutputBuffer` 是 ring buffer（1MB），取最后 200KB（留 56KB 给其他字段，总 payload < 256KB）。JSON escape 后超 256KB → 再截断到 128KB tail。

### 5. 清理死代码

删除：
- `task_complete_notification()` 函数
- `notification_tx` 字段及 `set_notification_sender()` 方法
- 退出 watcher 中 `notification_tx.lock()...send()` 块
- 对应的 2 个 unit test
- `main.rs` 中 `notification_rx` 相关代码（如果存在于当前版本）

### 6. exec.rs 改动

**零改动。** callback_url 不走 exec，走 ProcessManager 的 setter。

## 边界

### 做
- [x] 从 Loom link 推导 callback URL + auth header
- [x] ProcessManager spawn 完成后自动 HTTP callback
- [x] killed flag 防 shutdown/kill 假醒
- [x] 3 次尝试 + 指数退避 + 4xx 不重试
- [x] tail output + truncated/total_output_bytes 字段
- [x] 删除 relay notification 死代码
- [x] reqwest Client 复用（构造时创建）

### 不做（V1）
- ❌ 大文件传输（超限截断）
- ❌ 双向流（streaming output）
- ❌ Portal 主动推送（非 exec 事件）
- ❌ 完成批量聚合（10 个同时完成 = 10 个 callback）
- ❌ 持久化未送达 callback（失败就失败，being 可 portal_process poll 查）
- ❌ 同步 exec 路径的 orphan 进程修复（out of scope，但 tool description 引导用 background）

## 验收标准

1. `portal_exec(command="sleep 3 && echo done", background=true)` → 立刻返回 session_id
2. 3 秒后 being 的 inbox 出现 source="portal" 的 callback
3. being 空闲 → breathe_callback 触发，能 learn/remember
4. callback_url 不可达 → 3 次尝试后 WARN 日志（无 token 泄漏），Portal 不崩
5. `--connect` 未设置 → 不回调，行为和当前一样
6. `portal_process kill <session>` 后进程退出 → **不触发 callback**
7. Portal Ctrl+C（kill_all）→ **不触发 callback**
8. Heart 返回 401 → 不重试，直接 WARN
9. output > 200KB → 截取 tail，payload 包含 truncated=true

## 影响

| 范围 | 影响 |
|------|------|
| Binary 大小 | 零（reqwest 已有） |
| 网络 | 每次后台任务完成 +1 HTTPS POST |
| Heart | 零改动（callback API + inbox dedup 已就绪） |
| 向后兼容 | 完全兼容 |
| Being 认知 | tool description 更新即可 |

## 估算

~200-250 行（含测试），0 新依赖。

## 版本

Portal 0.8.0
