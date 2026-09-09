# 2026-09-09 CodexFF Pro 交接记录

## 目的与状态

- 目的：修复官方/第三方 Recents 已隔离后，侧边栏仍残留空项目卡片的问题，并修复切换第三方渠道后 Codex 界面语言回退为英文的问题。
- 分支：`feature/dns-premium`
- 提交：`14968a5`（随后各分支以 cherry-pick 方式同步）。
- 本次只改轻量项目/配置元数据投影；不读取、不迁移、不重写会话正文或 rollout 文件。

## 改动文件与关键定位

### `src-tauri/src/session_unify.rs`

- `rebuild_project_index_from_catalog`：
  - 从 Desktop `local_thread_catalog` 的 `cwd` 增量补齐缺失项目节点。
  - 对相同规范化 `rootPaths` 的重复节点去重，优先保留原始稳定项目 ID，移除旧版本生成的重复恢复节点。
  - 同步清理 `project-order` 中对应重复项；若当前选中项指向重复节点则清除该轻量选择字段，避免继续指向空卡片。
- `provider_project_cwds(provider, hidden)`：
  - 当前渠道只读取 `state_5.sqlite.threads` 主表。
  - 另一渠道只读取 `threads_codexff_recents_hidden` 隐藏表。
  - 禁止将主表和隐藏表合并，否则同一 `cwd` 的旧隐藏索引会把应隐藏项目误判为当前渠道可见。
  - 对 subagent 线程沿 `source.subagent.thread_spawn.parent_thread_id` 追溯有效 provider，避免官方父线程派生出的 custom 子线程把官方项目错误保留在中转侧。
- `sync_project_visibility`：
  - 在 Recents 渠道投影完成后，根据当前/另一渠道的 `cwd` 做可逆项目索引投影。
  - 仅隐藏能明确归属于另一渠道的项目；无可靠归属的项目保持可见。
  - 保留 `project-visibility-index/<provider>.json` 轻量备份，不包含会话正文。
  - 同步 `local_thread_catalog.project_id` 与 `state_5.sqlite.threads.project_id` 等项目归属元数据。
- `provider_project_ids`：
  - 补充按 SQLite `project_id` 判断当前/另一渠道项目归属。
  - 判断优先级为 `project_id`，再结合有效 provider，最后使用 `cwd` 兜底，避免官方项目名因派生线程重新出现在中转侧。

### `src-tauri/src/profiles.rs`

- 官方切换：先执行 Recents SQLite/catalog 渠道投影，再执行项目索引投影。
- 第三方切换：同样先执行 Recents 投影，再执行项目索引投影；第三方供应商之间热切换仍执行 Recents 校正。
- 失败回滚路径按原渠道重新执行 Recents 投影，避免项目索引和线程索引处于不同渠道状态。

### `src-tauri/src/session_manager.rs`

- Recents 主表与隐藏表的线程归属均沿 `source.subagent.thread_spawn.parent_thread_id` 追溯到根线程。
- 派生线程不再仅按自身 `model_provider` 判定可见性，避免官方项目被一条 `custom` 子线程带回中转侧。

### `src-tauri/src/lib.rs`

- App 启动时在 Codex/ChatGPT 未运行的条件下，先同步 Recents，再同步项目索引，避免启动阶段读取切换前的旧表状态。

### `src-tauri/src/codex_config.rs`

- `build_relay_config` 的运行时语言字段白名单补充 `localeOverride`，并保留磁盘当前值，不使用旧供应商底稿覆盖。
- 回归测试覆盖旧底稿为 `en-US`、磁盘实时配置为 `zh-CN` 时，中转配置仍保留当前语言。

### `src/App.tsx`

- 安全守护 Tab 的用户可见标题修改为“网络守护”；内部 Tab 标识与功能逻辑不变。

## 行为、兼容性与安全边界

- 项目过滤只使用 `cwd`、`project_id`、`model_provider`、归属表和索引 ID；不读取标题、预览、首条消息、消息内容或 rollout 正文。
- 相同项目根目录只保留一个稳定项目节点，避免“一个有会话、一个显示 No chats”的重复卡片。
- Recents 仍由现有可逆隐藏表负责隔离；本次没有把官方线程恢复到第三方，也没有改变线程 provider。
- 项目节点隐藏前会写入轻量索引备份，切回渠道时可恢复项目名、路径和顺序。
- `project_id` 只用于轻量索引关联；不存在可靠归属时不删除项目。

## Windows 平台差异与禁止项

- Windows 使用相同的 Rust 分支逻辑，但路径规范化应兼容反斜杠；本次未修改 Windows DNS、服务运行时或安装器逻辑。
- 未在 Windows runner 上执行编译；不得将本次 macOS 本地验证结果当作 Windows 构建结果。
- 不得通过读取会话正文或批量重写 rollout 来“修复”项目显示。

## 实际验证

已执行并通过：

```text
cargo fmt --manifest-path src-tauri/Cargo.toml
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml session_manager::tests --lib
cargo test --manifest-path src-tauri/Cargo.toml session_unify::tests --lib
cargo test --manifest-path src-tauri/Cargo.toml codex_config::tests --lib
npm run build
git diff --check
```

结果：

- Rust 编译通过。
- `session_manager` 定向测试 11 项通过。
- `session_unify` 定向测试 6 项通过，新增 subagent 父线程 provider 归属回归。
- `codex_config` 定向测试 6 项通过。
- TypeScript/Vite 生产构建通过。
- `git diff --check` 通过。
