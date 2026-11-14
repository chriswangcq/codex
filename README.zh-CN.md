# Command Monitor：为 Codex CLI 加上事件驱动监控

**让本地命令的输出，触发 Agent 继续工作。**

[English](README.md) · [试用演示](docs/monitor-demo.md) · [反馈](https://github.com/chriswangcq/codex/issues) · [实现与验证记录](FORK_NOTES.md)

这是一个独立维护的 Codex CLI 社区 fork，增加了 `monitor` 工具：启动本地
命令后，对话可以回到空闲；stdout 行输出到达时，触发同一会话的后续
处理。如果 Agent 正在执行，通知会进入现有输入队列。筛选哪些信息值得输出由
被监控命令负责，monitor 不做语义分类。

适合应用日志、已有测试/构建 watcher、长时间运行的本地任务。监控机制本身
不要求模型反复查询状态；处理事件仍会调用模型，目前没有经过测量的 token
节省比例。

## 当前状态

- 基于 **Codex CLI 0.169.0**；monitor 迁移提交为 `443d3df224`。
- **Apple Silicon macOS 可下载预览版**，其他平台尚未完成实机验证。
- 已通过选定范围内的 **2,825 项 Rust 测试、9 项 Python 契约测试**，以及
  schema、格式和 16 个相关 crate 的 Clippy 检查；不代表整个工作区或所有平台通过。
- [0.169.0 预览 Release](https://github.com/chriswangcq/codex/releases/tag/monitor-v0.169.0-20261114)
  提供完整 CLI 包，采用剥离符号的未优化 `dev` 构建，未进行 Apple Developer ID 签名或公证。
- 尚未完成 0.169.0 的真实模型端到端验证；自动化场景使用模型夹具。
  仓库已有的 0.161.0 Release 是另一个版本。
- 本项目不是 OpenAI 官方发行版，也不提供模型服务或订阅权益。

## 下载试用

下载 macOS ARM64 包，核对校验和后解压运行：

```sh
mkdir codex-monitor-0.169.0-preview
cd codex-monitor-0.169.0-preview
curl -fLO https://github.com/chriswangcq/codex/releases/download/monitor-v0.169.0-20261114/codex-monitor-0.169.0-macos-arm64-preview.tar.gz
curl -fLO https://github.com/chriswangcq/codex/releases/download/monitor-v0.169.0-20261114/codex-monitor-0.169.0-macos-arm64-preview.tar.gz.sha256
shasum -a 256 -c codex-monitor-0.169.0-macos-arm64-preview.tar.gz.sha256
tar -xzf codex-monitor-0.169.0-macos-arm64-preview.tar.gz
./bin/codex --version
./bin/codex
```

请完整保留解压目录。`BUILD.json` 记录实际 fork 提交；`--version` 和包 manifest
显示上游基线版本 `0.169.0`。本预览包不提供上游自动更新，请从本仓库下载后续版本。
macOS 下载批准方式、删除方式及验证范围见[发布说明](docs/release-0.169.0.md)。

## 从源码构建

官方 npm、Homebrew 和 OpenAI 安装器安装的是上游版本。构建本 fork：

```sh
git clone --branch master https://github.com/chriswangcq/codex.git command-monitor
cd command-monitor
just assemble-codex-package \
  --target aarch64-apple-darwin \
  --cargo-profile dev-small \
  --package-dir "$PWD/dist/command-monitor"

./dist/command-monitor/bin/codex
```

需要 Git、Python 3.11+、rustup、`just` 和 Xcode 命令行工具。首次构建需要
下载经过校验的原生依赖，可能较慢。完整保留生成的目录，它包含 CLI、Code Mode
host、executor 和运行资源。二进制版本仍显示 `0.169.0`；反馈时附上
`git rev-parse HEAD`，用来识别实际 fork 版本。

接下来运行[双终端日志演示](docs/monitor-demo.md)：启动 monitor，等待会话空闲，
在另一终端追加日志，观察自动跟进，再查看和停止监控。

## 功能边界

- 复用官方命令审批、沙箱、进程管理、输入队列和历史事件。
- stdout 按物理行触发事件，并经过合批、限流和长度限制；stderr 仅归档。
  需要 stderr 也触发事件时，由你明确使用 `2>&1`。
- `persistent: true` 仅表示当前会话内取消普通超时，关闭或重启会话后不会恢复。
- `/ps` 查看后台工作，Down 键打开进程管理；`task_stop` 停止指定 monitor，
  `/stop` 停止所有后台终端和 monitor。
- 仅支持本地非交互命令；不支持远程执行环境或原生 WebSocket 监控。
- app-server 接口为实验性接口；不会安装或替换官方桌面应用和 IDE 扩展。

欢迎有本地日志、测试 watcher 或长任务需求的用户[提交反馈](https://github.com/chriswangcq/codex/issues)。
请提供平台、fork 提交、命令、预期行为和实际结果，并移除敏感内容。

采用 [Apache-2.0](LICENSE) 许可，保留 [NOTICE](NOTICE) 中的上游与第三方归属说明。
