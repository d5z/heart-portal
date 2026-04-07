[2804 chars] # Task: Phase 2 — Extension Manager + Hot Reload

## Context
Read docs/PHASE2-HOT-RELOAD.md first for full design.

Portal has a dual-layer architecture:
- Kernel (built-in tools: exec, file, web) — never restarts
- Extensions (external MCP servers) — hot-reloadable

## What to Build

### 1. `extensions.toml` co...