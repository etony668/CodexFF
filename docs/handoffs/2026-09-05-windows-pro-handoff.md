# 2026-09-05 Windows Pro / Free 交接记录

## 免费版补齐当天非收费 App 功能

- 日期：2026-09-05。
- 分支：`main-free`。
- 提交：未提交。
- 目的：补齐当天首次同步遗漏的非收费功能，保持收费版与免费版的共享功能结构一致。
- 改动文件和关键定位：
  - `src-tauri/src/lib.rs`：`AppStatus`、`get_status`、
    `prepare_official_account_login`、`switch_official_account`。
  - `src/api.ts`：官方账号状态类型及账号准备、切换命令。
  - `src/pages/ProfilesPage.tsx`：官方多账号卡片、账号分页、额度隔离与
    出口 IP 类型刷新。
  - `src/pages/CodexEnvironmentCard.tsx`、`src/pages/SettingsTab.tsx`：
    将 Codex 桌面端/CLI 安装与更新管理迁移到独立设置页。
  - `src/App.tsx`、`src/App.css`：设置 Tab、切换目标状态及共享 UI 样式。
- 行为变化：
  - 官方账号凭证仍只保存在加密 Vault；前端只接收脱敏账号标识。
  - 任意时刻只恢复当前选中的官方账号到 `auth.json`，不允许官方账号并行登录。
  - Codex 运行环境不再占用官方订阅卡片，免费版新增独立“设置”Tab。
  - 出口 IP 改变、窗口重新聚焦或手动刷新后，重新检测 ASN/组织与 IP 类型；
    请求序号阻止旧出口的慢响应覆盖新结果。
  - 官方额度随账号切换清空并强制刷新；失败后按上限 60 分钟退避。
- 收费边界：
  - 未同步 CodexFF Pro 的 Cloudflare 版本检测、六小时轮询和更新提示。
  - 未同步 DNS 守护、任务看板、授权、体验卡或收费门控。
- Windows 差异与禁止项：
  - 多账号和设置页逻辑为跨平台共享实现。
  - Windows 仍必须使用已有的隐藏控制台进程启动逻辑；不得重新引入可见终端窗口。
  - 不得把 Pro 产品标识、包标识或收费资源复制到免费版。
- 验证：

```bash
cargo fmt --manifest-path src-tauri/Cargo.toml
npm run build
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml vault::auth_tests
git diff --check
```

- 结果：前端构建、Rust 检查与格式检查已通过；Vault 定向测试待执行。
