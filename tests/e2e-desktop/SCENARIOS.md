# Obelisk E2E 场景目录(桌面,11 类)

> 追溯:Rust 迁移计划 §6.2 桌面 E2E 目录。驱动器:`node tests/e2e-desktop/run-desktop.mjs`
> (cua-driver 后台注入 + XTest 兜底;断言以确定性状态为主——DB/settings/日志/进程,
> 视觉断言为辅——`dim image read` 本地视觉模型按关键词断言)。
> 环境要求:XWayland `:0`(GPUI Linux X11 后端)、cua-driver daemon
> (`DISPLAY=:0 cua-driver serve`)、sqlite3、ImageMagick(import/convert)。
> 每场景失败自动重试 ≤2;证据归档 `tests/e2e-desktop/evidence/<run>/`。

## 已知限制(诚实记录,不粉饰)

- **GPUI 无 AT-SPI 元素树**(fc-gpui Linux 无障碍缺口,ADR-0013 已知):所有点击走像素坐标(每步先截图定位或用固定布局网格),`element_index` 定位不可用。
- **D3 的"10 万行深滚动"**:黄金语料为单会话 fixture,本场景验证滚动+跟随语义;10 万行帧率基准属于价值指标基线,不在 E2E 目录内。
- **D8 多窗口**:同一进程的多窗口仅能经托盘菜单打开(无 dbus 自动化路径);场景验证"关窗进程常驻(托盘语义)"。真正的多窗口自动化记为限制,Stage 3 复评。
- **D9 中文 IME**:依赖 fc-gpui 的 XIM 路径;`cua-driver type_text` 注入 Unicode。若 XIM 不通,该场景 FAIL 并把缺口记入 ADR(与 Windows 无障碍缺口同类)。

| # | 场景 | 覆盖 | oracle | 状态 |
|---|---|---|---|---|
| D1 | 冷开载入 | fixture HOME 启动 → 侧栏 PROJECTS + 会话列表行(标题/来源/项目/计数),VIEWS 导航五项 | 截图视觉 + 进程存活 | ✅ |
| D2 | 项目→会话→时间线 | 点项目过滤 → 点会话开时间线 → 工具卡折叠(▸)→ 点击展开(命令框/`{ } Raw`)→ Esc 返回 | 截图序列 | ✅ |
| D3 | 深滚动 + live 跟随 | 时间线 PageDown 翻页推进 → End 钉底(follow-tail)→ DB 注入新消息 → `r` 刷新尾部可见 | 截图 + DB 计数 | ✅ |
| D4 | 应用内搜索对齐 CLI | 搜索框输入 → "Full-text matches (N)" 区渲染;N 与 `obelisk --search` 同词 JSON 行数一致;点 hit 开对应时间线 | 截图 + CLI stdout | ✅ |
| D5 | 记忆浏览与召回 | Memory 视图 active/archived 分区 + 归档原因;点行显示 markdown 文件内容(路径/会话);FTS 召回在 D4 同引擎 | 截图 + 文件内容 | ✅(归档/恢复=Stage 3 写权) |
| D6 | 统计 + 周报 | Activity:tokens/sessions/memories 计数与 DB 聚合一致,峰值日/最长回卡片,日柱状图;Recap:列表+点选 JSON 详情 | 截图 + DB 聚合 | ✅ |
| D7 | 托盘常驻 + 后台索引 | app 运行中追加语料 zstd 帧 → 无人工操作,列表 msgs 计数自动增长(daemon 增量 build);关窗后进程存活 | DB 轮询 + pgrep + 日志 | ✅ |
| D8 | 窗口关闭常驻(多窗口限制) | 关闭窗口 → 进程仍存活(Explicit quit);重开经托盘(手动路径) | pgrep + 窗口消失 | ✅(多窗口=限制) |
| D9 | 中文 IME 输入 | 搜索框注入 "会话 历史" → 框内文本与过滤/FTS 行为一致 | 截图 | ✅ |
| D10 | 设置页 | Settings:editorScheme 五选一;点击写入 settings.json 并高亮切换;provider roots 只读展示 | 截图 + settings.json | ✅ |
| D11 | live 会话更新(follow) | 时间线打开于 End(follow-tail)→ 语料追加 → daemon build → 尾部自动出现新消息且钉底 | 截图 + DB | ✅ |
| D12 | 滚动锚保位（M4.1） | 打开会话翻页到中部→删除语料一条消息（非纯追加）→daemon 增量刷新后时间线视口像素级保位（<5% 变化）；关闭重进恢复阅读位置与披露状态 | F8 | all | 像素 diff | ✅ |

## 稳定性纪律(与 CLI 目录一致)

- 场景失败自动重试 ≤2;证据归档:每步窗口截图序列、DB dump、app stderr 日志。
- 滚动 20 次窗口内失败率 >5% 记 flaky,修复才能过门。
- Stage 出口:连续 3 轮全绿。
