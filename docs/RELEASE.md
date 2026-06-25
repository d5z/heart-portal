# Portal Release SOP

## 版本号来源（三处必须同步）

| 位置 | 文件 | 作用 |
|------|------|------|
| **Cargo.toml** | `portal/Cargo.toml` → `version` | binary 内嵌版本（PORTAL_VERSION） |
| **Town portal_info** | `town/src/main.rs` → `portal_info()` | beings 看到的版本 + changelog |
| **Git tag** | `vX.Y.Z` | GitHub release + `--upgrade` 拉取 |

**三处必须一致。Cargo.toml 是 source of truth。**

## Release Checklist

```
□ 1. 更新 portal/Cargo.toml version
□ 2. git commit + push main
□ 3. 三平台编译
      cargo build --release                                          # macOS arm64
      cargo build --release --target x86_64-apple-darwin             # macOS x86_64
      cargo build --release --target x86_64-unknown-linux-musl       # Linux x86_64
□ 4. git tag vX.Y.Z + push tag
□ 5. gh release create vX.Y.Z ... --title "vX.Y.Z — Title" --notes "..."
      上传三个 binary: heart-portal-darwin-arm64, heart-portal-darwin-x86_64, heart-portal-linux-x86_64
□ 6. Town portal_info 更新（version + changelog）
      编辑 town/src/main.rs portal_info()
      cargo build + scp + systemctl restart town-server
□ 7. 验证
      heart-portal --upgrade  (本地测)
      curl -sk https://beings.town/api/portal | python3 -c "..."  (Town 验证)
```

## 常见错误

- **版本号没更新**：Cargo.toml 忘改 → binary 内嵌旧版本号。2026-06-25 发生过。
- **merge 后版本覆盖**：`git checkout origin/main -- file` 会覆盖本地修改，包括版本号。
- **binary 超限**：MAX_IMAGE_SIZE 设太小 → 截图读不了。4MB→10MB (2026-06-25)。

## 教训

1. **先改版本号再编译**——不是最后改。版本号在 Cargo.toml 里，编译时烧进 binary。
2. **merge conflict 后重新检查版本号**——`git checkout origin/main --` 会丢本地改动。
3. **release 后让一个 being 跑 `--upgrade` 验证**——不要只看 GitHub 页面。
