# Obelisk E2E 场景目录（CLI，13 类）

> 追溯矩阵：README agent 流程、SKILL.md 示例中的每条业务操作 ↔ 至少 1 个
> E2E 场景。覆盖率门：本矩阵零未覆盖行；文档新增操作必须同步新增场景。
> 驱动：`node tests/e2e/run-e2e.mjs --binary <path> [--compare <path>]`
> （tmux 驱动真实 TTY；断言只用可观察输出：stdout JSON、DB 行、文件、退出码）。

| # | 场景 | 覆盖 | F# | 平台 | oracle | 状态 |
|---|---|---|---|---|---|---|
| C1 | 全新安装→首建 | 干净 HOME + 五 provider 语料 → `--build` → `{"ok":true,"db":...}`、DB 12 表存在、再次 build 幂等 | F1/F9 | all | stdout+DB | ✅ |
| C2 | skill 安装 | `obelisk install` → 假 npx(PATH shim)记录参数 `--yes skills add tommy0103/obelisk-skill`；npx 缺失→stderr+exit 1 | F7 | all | 进程参数 | ✅ |
| C3 | agent 首查（mktemp/heredoc） | `mktemp`→heredoc 写 query 文件→`--query <file>` 返回 overview JSON 形状（current/projects/totals 键） | F3/F7 | all | stdout JSON | ✅ |
| C4 | `--search` + nonce 归属 | 建 fixture 会话（含 nonce 的 tool-call 行）→ `--search "text" --nonce <t>` → 结果含 `session.is_invoking=true` | F5 | all | stdout JSON | ✅ |
| C5 | 增量新鲜（被动拉取） | 首建后向语料追加新会话 → `--search 新词` 立即命中（查询前刷新）；无改动时二连查输出一致 | F2/F6 | all | stdout JSON | ✅ |
| C6 | daemon 共存 | DB 注入 `__app_heartbeat__`（mtime=now）→ `--build` skip(daemon_active) 但 `--search` 仍可用；心跳过期后 build 恢复 | F6 | all | DB+stdout | ✅ |
| C7 | 记忆 remember→召回→forget | attune 写 markdown 路径记忆 → query memories() 召回 → forget 幂等（二次 already_deleted）→ FTS 召回不再命中 | F4 | all | DB+stdout | ✅ |
| C8 | sql() 写拒绝 | `sql("DELETE FROM messages")`/`INSERT`/`UPDATE`/多语句/PRAGMA → 每个返回 error JSON，行数不变；SELECT 正常 | F3 | all | stdout+DB | ✅ |
| C9 | 失控脚本 30s | 同步死循环与 `await` 后死循环均在 ~30s 被杀，返回 `{"error":"Script execution timed out after 30000 ms",...}`；Rust 侧双杀；TS 侧同步杀、await 后挂死为已知缺口（ADR-0013） | F3 | all | stdout+耗时 | ✅ |
| C10 | 五 provider 混合索引 | 黄金语料 scale-1 全量建库 → 各 source 会话数/消息数与 corpus-manifest 一致；FTS 跨 source 命中 | F1 | all | DB 对照 | ✅ |
| C11 | 错误路径 | 坏旗标→usage+exit 1；`--search`（无文本）→usage；空库 `--search`→空数组 JSON；坏 JSONL 行不炸（build 完成且好行入库）；`--query` 不存在文件→error JSON | F1/F7 | all | stdout+exit | ✅ |
| C12 | TS 建库→Rust 接续 | TS 二进制 build+dump → Rust 二进制对同一 HOME build（增量）→ dump 字节一致（游标/行不变）；反向同验 | F6/F1 | all | dump diff | ✅（需 Rust CLI；TS 退役后跳过） |
| C13 | 真实 daemon 心跳仲裁（M3.1） | 启动真实 GPUI app daemon（隔离 HOME）→ 启动即写 `__app_heartbeat__`；CLI `--build`→`daemon_active` 而 `--search` 可用；daemon 退出后标记仍新鲜（60s 窗口内继续 skip）；过期后 build 恢复 | F6 | all | stdout+DB | ✅（需 release app 二进制 + DISPLAY，无则 skip） |

## 稳定性纪律

- 场景失败自动重试 ≤2；证据归档：tmux capture-pane、DB dump、退出码。
- 滚动 20 次窗口内失败率 >5% 记为 flaky，必须修复才能过门。
- Stage 出口：连续 3 轮全绿 + TS/Rust 双跑对照一致（JSON 规范化后）。

## 双跑对照的规范化规则

对照前对两侧 stdout JSON 做相同处理：剥离 `stack` 字段；时间戳/耗时字段
（C9 的 wall-clock）按场景声明豁免；路径前缀（tmp HOME）归一化为 `<HOME>`。
