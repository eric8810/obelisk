# cua-driver 操作手册（E2E 桌面 harness 驱动器）

定位：Rust 迁移计划 §6.2 桌面 E2E harness 的驱动器（trycua/cua 的 Cua Drivers），
决策依据 [ADR-0013](adr/0013-tech-selection-rust-cli-sandbox-and-desktop.md)。
本文基于本机安装后的实测输出编写，供 M2.6 搭建 harness 时直接使用。

## A1 本机安装状态（实测 2026-09-03）

- 版本 **cua-driver 0.23.2**（stable channel），MIT（github.com/trycua/cua）
- 二进制：`~/.local/bin/cua-driver`
  → `~/.cua-driver/packages/releases/0.23.2-x86_64-unknown-linux-gnu/cua-driver`
- daemon **运行中**（2026-09-03 从 TTY 带 `DISPLAY=:0` 启动并实测通过：
  `get_screen_size` → 2560×1600；`cua-driver stop` 可停）
- 本机图形会话：Hyprland（tty1）+ XWayland `:0` + `wayland-1`
- ⚠️ 环境继承注意：从 TTY/SSH 启动的 shell **不继承**图形会话的
  `DISPLAY`/`WAYLAND_DISPLAY`——与显示器/熄屏无关。解决：给 **daemon 进程**
  显式设 `DISPLAY=:0`（按需再加 `WAYLAND_DISPLAY=wayland-1`）；
  `call`/`mcp` 客户端只连 socket，自身不需要 DISPLAY

## A2 daemon 生命周期

```sh
cua-driver serve          # 起 daemon，默认 socket ~/.cache/cua-driver/cua-driver.sock
cua-driver status         # 查 daemon 是否在跑
cua-driver stop           # 停 daemon
cua-driver doctor --json  # 体检探针（可进 CI 预检）
```

客户端（`call`、`mcp`）只连 socket 不自起 daemon；报
`daemon is not running` 时先 `serve`。

## A3 三种驱动方式

1. **MCP server（E2E 主路径）**：`cua-driver mcp`（stdio）。
   注册到 agent 客户端用 `cua-driver mcp-config --client <name>`
   （claude/codex/cursor/opencode/hermes/…，各自打印现成命令或 JSON，
   Claude Code 走 `claude mcp add-json`）。
2. **CLI 一次性调用**：`cua-driver call <tool> [参数]`；
   参数 schema 用 `cua-driver describe <tool>` 查看（如 `describe click`）。
3. **Agent skill pack（可选）**：`cua-driver skills install`——把官方 skill
   包链接进 Claude Code/Codex 等的 skills 目录；幂等、不覆盖已有链接。

## A4 Linux 输入与定位要点

- 后台注入为默认（`delivery_mode=background`）：X11 经 XSendEvent/XTEST/XI2，
  **不抢焦点、不抬窗**——人正在用机器时测试可并行
- 原生 Wayland 走 libei + xdg-desktop-portal：无 per-window 后台定位、
  仅左键、不支持带修饰键点击 → **GPUI 的 Linux X11 后端（Hyprland 下经
  XWayland）正好避开这些限制**
- 点击优先 `element_index`（AT-SPI 元素）：后台/隐藏窗口可用、句柄稳定，
  还能告诉你点的是什么（role+label）；canvas/自绘区域才用像素坐标
  （先 `zoom`，再传 `from_zoom=true` 自动换算回窗口坐标）
- AT-SPI 元素缓存随每轮 `get_window_state` 重建 → **每步操作前重快照再点**
- 录屏取证先装 ffmpeg：`cua-driver call install_ffmpeg`（Linux 需要，macOS 原生）
- Linux 权限自检：`cua-driver call check_permissions`

## A5 与 E2E 计划的对接映射（§6.2 / §6.4）

| 计划要求 | cua-driver 机制 |
|---|---|
| 双 oracle：确定性状态为主 | `verify_state`（窗口谓词断言）+ `get_window_state`（AT-SPI 结构树）+ harness 侧 DB/进程/托盘断言 |
| 视觉断言为辅 | `zoom`（区域截图）/ `get_desktop_state` + 黄金截图容差比对 |
| 失败证据自动归档 | `start_recording` / `stop_recording`（视频，需 ffmpeg）+ 每步 zoom 序列 |
| 回归复现 | `replay_trajectory`：按录制轨迹重放全部工具调用 |
| CI 安全收权 | `serve --permission-mode bounded --capability-manifest ./cua-capabilities.yaml --approve-capability-manifest`（narrow-only：限工具/应用/来源） |
| 每场景会话隔离 | `start_session` / `end_session`（光标、录制等清理钩子）；紧急撤销 `revoke --session` / `revoke --all` |
| 启动被测应用 | `launch_app` / `list_apps` / `list_windows` / `bring_to_front` |
| 人机并行不干扰 | background 注入 + 会话光标 overlay（agent 虚拟光标可见、不动真实指针；`serve --no-overlay` 可关） |
| 键盘路径 | `press_key` / `type_text` / `hotkey`（XSendEvent 后台送达） |

## A6 版本管理（计划要求锁版本）

- harness 记录驱动版本（当前 **0.23.2**）并锁 stable：
  `cua-driver channel status` / `cua-driver channel set stable`
- 手动升级：`cua-driver check-update` → `cua-driver update --apply`
- CI 预检 `doctor --json`；驱动版本变更 → 重跑全 E2E 目录才能出报告
- Linux 驱动处于 pre-release 阶段（README 口径）→ 已列计划风险表：
  锁版本 + ydotool/XTest 脚本兜底

## A7 已知注意事项（实测/文档确认）

- 从 TTY/SSH 启动 daemon 时必须显式带 `DISPLAY=:0`（本机 XWayland 在 :0），
  否则 X11 工具全部失败；从桌面终端启动则自动继承
- `list_windows` 只列 X11/XWayland 窗口：Hyprland 原生 Wayland 应用不在其中
  （实测当前返回空属正常）；GPUI 应用（X11 后端）启动后才会出现在列表里
- `status` 以 socket 判断；经 MCP 代理拉起的实例若端点不同可能误报——
  诊断以 `doctor --json` 为准
- `permissions` 子命令是 macOS TCC 专用；Linux 用 `check_permissions` 工具
- `autostart` 子命令当前 Windows-only；Linux 开机自启走安装器
  `--autostart`（注册 systemd user unit）
- `browser_*` 工具族是 CDP 驱动浏览器用的，GPUI 桌面测试用不到；
  核心 UI 工具族即 A5 所列
- `verify_state`/`replay_trajectory` 等工具的确切参数以
  `cua-driver describe <tool>` 为准（版本间可能变化）
