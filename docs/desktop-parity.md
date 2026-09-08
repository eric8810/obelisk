# Obelisk 桌面端功能对齐规格(Desktop Parity Spec)

> **目的**:原版 Electron 桌面应用(Archived 参照:`git show 0988385:app/...`)是 GPUI 重写版的行为规格。本文档逐项记录原版功能、当前状态与验收标准,作为持续开发的单一事实来源——**开发对着它勾,验收逐项核,不再靠记忆**。
>
> **来源**:2026-09-07 六路并行审计(骨架/列表/时间线/Memory/Activity+Recap/Settings),全部结论基于双方源码逐行阅读,源码位置均标注。

## 维护规则

1. **状态标记**:P 对齐 / P- 部分对齐(差在哪必须写) / M 缺失 / X 行为不同。修复一项就把状态改为 P 并注明 commit。
2. **PR 规则**(已写入 CONTRIBUTING):改 `crates/obelisk-app` 的 PR 必须对照本文档更新相关项状态;新增功能先入文档再写代码。
3. **验收标准**:每项的"验收"列是可观察行为(点击什么出现什么),E2E 场景(D#)或人工核对清单据此断言。
4. **X 但更好**的项(如 tray 常驻、FTS 命中区)标注"GPUI 超集",不是缺口。
5. 原版源码参照提取:`git show 0988385:<path> > /tmp/orig-ui/...`(views/components/main/styles/src)。

## 评级统计(2026-09-07)

| 域 | 项数 | P | P- | M | X | P0 | P1 | P2 |
|---|---|---|---|---|---|---|---|---|
| 全局骨架/侧栏 | 47 | 8 | 12 | 18 | 9 | 1* | 13 | 8 |
| Sessions 列表 | 31 | 12 | 9 | 7 | 3 | 0 | 8 | 8 |
| 时间线+滚动 | 54 | 12 | 16 | 4 | 22 | 2 | 12 | 15 |
| Memory | 19 | 1 | 5 | 0 | 13 | 4 | 9 | 3 |
| Activity+Recap | 26 | 3 | 2 | 4 | 17 | 3 | 5 | 4 |
| Settings | 14 | 3 | 3 | 3 | 5 | 3 | 6 | 4 |
| **合计** | **191** | **39** | **47** | **36** | **69** | **13** | **53** | **42** |

*\*条件性 P0:无 tray 桌面上关窗后应用无法重开/退出。*

## P0 汇总(修复路线第一优先)

| # | 域 | 问题 | 验收标准 |
|---|---|---|---|
| P0-1 | 时间线 | 刷新无用户滚动感知;非纯追加变更走 `reset` 跳回顶部、丢全部测量(`main.rs` refresh_timeline_daemon + fc-gpui list reset)。live 会话边跑边读必踩 | 用户滚动期间索引刷新不移动视口;非纯追加补丁提交后,视口顶可见行保持原屏幕偏移(±1px) |
| P0-2 | 时间线 | reader-anchor 与阅读状态缓存缺失:返回列表再进=顶部重开、折叠/展开态全丢 | 重进同一会话恢复滚动位置与全部披露状态 |
| P0-3 | Memory | 归档/恢复操作完全缺失(原版核心交互:行悬浮/详情/`D` 键三入口 + undo) | 行与详情可归档/恢复,5s undo toast 可撤销,DB `deleted_at` 正确落库 |
| P0-4 | Memory | Active/Archived 子视图切换语义缺失:侧栏两入口行为相同、Archived 永不高亮、单页混排两节 | Active 只显示未归档、Archived 只显示已归档,侧栏高亮互斥 |
| P0-5 | Memory | 记忆→会话溯源断链(`session id` 纯文本) | 详情"查看会话"可跳转对应时间线 |
| P0-6 | Activity | 热图、活动账本(三分类/噪音过滤/跳转)整体缺失——原版 Activity 页下半区主体 | 371 格热图可点击选中切日账本;账本三分类分组渲染,行可跳转会话 |
| P0-7 | Recap | 详情为 JSON 原文直出;五张卡牌(Cover/Path/Vibe/Workflow/Closing)与 archetype 主题未渲染 | 选定周报渲染五卡,键盘 ←/→ 翻页,archetype 调色板生效 |
| P0-8 | Settings | 手动 Rebuild index 完全缺失 | About 区按钮触发全量重建,进行中禁用+文案,失败红字 |
| P0-9 | Settings | root 保存无校验/反馈:坏路径静默失效;daemon watcher/registry 不跟随新配置(需重启但状态行宣称"已重建") | 非法路径即时红字提示;保存后 daemon 监听新根(不重启) |
| P0-10 | 骨架 | **✅ 已修复(M4.0)**:启动探测 StatusNotifierWatcher(busctl/gdbus);无 tray 桌面关窗即退(LastWindowClosed),有 tray 保持常驻(Explicit,本机 Hyprland 实测 true,D8 不变) | 探测代码路径已验证;GNOME 实机复测待做 |
| P0-11 | 列表 | **✅ 已修复(M4.0)**:char 级折叠匹配(folded 流 + 原 char 边界映射),单测覆盖 İ 1:N 折叠命中/ß 不命中(parity JS)/reassemble 精确性 | `highlight_segments_*` 2 个单测 |
| P0-12 | 骨架 | 全局键盘层缺失:Cmd/Ctrl+1/2/3、`s` 排序、Esc 清选/清词均无(影响高频操作路径) | 快捷键逐条按下行为符合下表 #25-33 |
| P0-13 | 时间线 | Memory 域键盘导航全缺(j/k/Enter/x/D/u/Cmd+Z)(与 P0-3/4 同批交付) | 键盘可完成浏览/勾选/归档/撤销全流程 |

---

## 域 1:全局骨架与侧栏

原版源码:`App.vue`、`styles/{base,sidebar,toolbar}.css`、`src/{keyboard-shortcuts,router,store,sidebar-projects,source-catalog,main}.mjs`、`main/index.ts`。GPUI:`main.rs`、`theme.rs`、`daemon.rs`。

| # | 功能 | 原版行为 | 状态 | 验收标准 |
|---|---|---|---|---|
| 1 | 应用骨架 | 32px 自绘标题栏 + 220px 侧栏 + 44px 工具栏 + 主区 | P- | GPUI 无全局工具栏行,各视图自带 header(结构差异,可接受) |
| 2 | 自绘标题栏 | hiddenInset + 红绿灯定位 + 居中 scope 标题 | X | P2:原生标题栏可用;如需复刻则按原版布局 |
| 3 | 动态窗口标题 | 按路由写 document.title("Sessions · 标题"等) | M | 多窗口时窗口可区分内容;切视图标题跟随 |
| 4 | 面包屑(列表) | 「Sessions / 项目」根 crumb 点击清项目过滤回全量 | M | 项目过滤态下点面包屑根回到全量列表 |
| 5 | 面包屑(详情) | 各详情页层级面包屑(会话/子代理/记忆/Recap) | M | 随各详情页(P0-5/域3)一并交付 |
| 6 | 工具栏搜索 | 220px + 放大镜 + `/` kbd 提示 + 200ms 防抖;Memory 列表同样有 | P- | `/` 提示与防抖补齐;Memory 搜索见域 4 |
| 7 | 排序切换 | newest/oldest 按钮 + `s` 键 | M | 按钮与 `s` 均可切换,列表即时重排(见 P0-12) |
| 8 | Recap 工具栏 | Weekly/Monthly tab + Generate 按钮 | M | 随域 5 Recap 交付 |
| 9 | Source 过滤器 | 多源时下拉筛选(每源勾选) | M | ≥2 源时出现过滤器,选择后列表过滤 |
| 10 | 搜索消息体开关 | toggleIncludeMessageBodies | M | 开关切换后搜索范围变化 |
| 11 | 品牌行 | logo + Obelisk 粗体 | P | 已对齐 |
| 12 | Source-health 圆点 | 每源一枚状态圆点(ok 发绿/warn/error),点开 popover | M | 侧栏品牌行右侧圆点组,状态与索引健康联动 |
| 13 | Sources popover | 每源行(名+计数+statusText),底部跳 Settings | M | 随 #12 一并交付 |
| 14 | Library 分区 | Sessions/Memory/Active/Archived + 计数徽标 | P | 已对齐(含计数语义) |
| 15 | 侧栏激活规则 | Sessions=会话域且无项目过滤;Active/Archived 按 view 互斥 | X | Active/Archived 互斥高亮见 P0-4;Sessions 激活加"无项目过滤"条件 |
| 16 | 导航副作用 | 切视图清 cursor/多选/搜索词;Memory 域清项目过滤 | X | 切视图时搜索词与选中态清空(先有选中态模型) |
| 17 | Stats 分区 | Activity/Recap | P | 已对齐 |
| 18 | Projects 显隐 | 仅 sessions/memory 域渲染 Projects 分区 | X | Activity/Recap/Settings 下隐藏 Projects 区 |
| 19 | 项目行 | formatProjectLabel(最短 project_path 末段)+ 计数;点击过滤;Memory 域留 Memory | P- | 标签格式化(P1);点击不强制切 Sessions;已选再点=取消是 GPUI 超集,保留 |
| 20 | 项目计数语义 | sessions 域=会话数;memory 域=记忆数(按 view 过滤) | X | Memory 域下项目计数为记忆数 |
| 21 | 项目搜索框 | ≥6 项目时 "Filter projects…" 过滤 | M | 项目多时可过滤 |
| 22 | Noise 项目折叠 | 十六进制/测试项目折叠 "N hidden" + show all 开关 | M | noise 项目默认折叠可展开 |
| 23 | Settings 底部固定 | margin-top auto | P | 已对齐 |
| 24 | 侧栏视觉 | 行高/圆角/药丸/hover/徽标 | P- | 激活行补 2px accent 发光竖条 |
| 25-27 | Cmd/Ctrl+1/2/3 | Sessions/Active/Archived | M | 见 P0-12 |
| 28 | `/` 聚焦搜索 | 非输入焦点按 `/` 聚焦+全选 | P- | 空列表时也生效;全选文本 |
| 29 | `s` 排序 | 见 #7 | M | 见 P0-12 |
| 30-31 | Esc | 清多选→清搜索词;输入框内仅 blur | M | 全局 keydown 分发补齐 |
| 32 | 输入框 Esc | blur 不清内容 | P- | fc-ui Input 已等价 |
| 33 | Memory 域键盘 | j/k/Enter/x/d/u/Cmd+Z | M | 见 P0-13(域 4 详表) |
| 34 | 路由表 | 9 条命名路由 | X | GPUI 用 AppView 枚举,五个顶层视图等价;详情路由随各域 |
| 35 | 深链 | hash URL 直达 | M | 桌面 app 无 URL,豁免(记录) |
| 36 | 前进/后退 | Chromium history | M | Alt+←/→ 或面包屑替代,记录为设计差异 |
| 37 | 窗口参数 | 1200×800、最小 800×500、背景色 | X | P2:补最小尺寸与背景色 |
| 38 | 窗口记忆 | 双方都无 | P | 一致 |
| 39 | 多窗口 | 原版单窗;GPUI tray 可多窗 | P+ | GPUI 超集 |
| 40 | 关闭行为 | 原版关窗退出(非 darwin);GPUI tray 常驻 | X | 见 P0-10(无 tray 桌面) |
| 41 | Tray | 原版无;GPUI 有(Open/Quit) | P+ | GPUI 超集(ADR 有意) |
| 42 | 外链策略 | 外部 http(s) 交系统浏览器 | P- | GPUI 暂无外链路径;补 openExternal 等价物时遵循 |
| 43 | 缩放锁定 | 拦截 web zoom | P | 无 web 概念,豁免 |
| 44 | 启动后台 | 启动即索引 | P | 已对齐(daemon 首建) |
| 45-47 | 全局刷新/推送/状态面 | 见审计 | P- | 补窗口重新可见时刷新;其余等价 |

## 域 2:Sessions 列表

原版:`SessionList.vue`、`styles/list.css`。GPUI:`session_list.rs`、`data.rs`。

| # | 功能 | 原版行为 | 状态 | 验收标准 |
|---|---|---|---|---|
| 1 | 标题+高亮 | (untitled) 兜底 + `<mark>` 高亮 | P | 已对齐 |
| 2 | 年龄条 | 3px 竖条按创建时间对数渐变着色 | M | P2:视觉补齐 |
| 3 | 项目标签 | 仅无过滤时显示 basename | P- | 过滤时不显示;basename 化(P1) |
| 4 | 消息数 | `N msg` | P- | 措辞对齐 |
| 5 | 时间格式 | 当天 HH:MM / 同年 MM/DD / 跨年 YYYY/MM/DD | X | 分级时间格式(P1) |
| 6 | 行内字段 | 仅标题/项目/消息数/时间 | X | source 徽标是 GPUI 超集,保留;branch 不显示(一致) |
| 7 | 项目过滤 | projectFilter 过滤 | P | 已对齐 |
| 8 | Source 过滤 | 工具栏下拉 | M | 见域 1 #9 |
| 9 | 客户端过滤 | 标题/项目/**git_branch** 三字段 substring | P- | **补 branch 字段参与过滤**;id 参与过滤是超集;FTS 命中区是超集保留(但点击应跳到命中消息——P1) |
| 10 | 高亮实现 | 转义+正则安全替换 | P | **修字节切片 panic**(P0-11) |
| 11 | 排序 | ended_at\|\|started_at,默认新→旧 | P- | 固定排序与原版默认一致 |
| 12 | 排序切换 | 见域 1 | M | 见 P0-12 |
| 13 | 噪音会话折叠 | untitled 默认隐藏 + 横幅 Show all | M | 大库下 untitled 不淹没列表(P1) |
| 14 | 搜索防抖 | 200ms | M | 防抖 + 查询移出 UI 线程(P1,大库输入卡顿) |
| 15 | `/` 聚焦 | 全局非输入焦点 | P- | 见域 1 #28 |
| 16 | Esc | blur/清词 | P- | 见域 1 #30-31 |
| 17 | 行点击 | 进详情 | P | 已对齐 |
| 18 | 侧栏联动 | 项目过滤 + 面包屑 | P- | 面包屑见域 1 #4 |
| 19-20 | 项目搜索/噪音项目 | 见域 1 #21-22 | M | 同域 1 |
| 21 | 空态(无数据) | onboarding 文案 + Settings 链接 + 搜索路径 | P- | GPUI 有 building/no-found 区分(超集);补 Settings 入口链接 |
| 22 | 空态(搜索无果) | "Try a different search term." | P | 已对齐 |
| 23 | 加载态 | 无(原版也无) | P | 一致 |
| 24 | 刷新 | 可见性/索引更新/路由切换 | P- | 补可见性触发 |
| 25-28 | 调试键/批量/右键/键盘导航 | 原版均无 | P | 一致(会话域无 j/k) |
| 29 | 分页 | 原版无分页(limit 1000) | X | GPUI 无上限全量;对齐为 limit 1000 或虚拟化(记录) |
| 30 | Cmd+1/2/3 | 见域 1 | M | 见 P0-12 |
| 31 | 标题覆盖 | 详情加载后列表标题即时更新 | P- | daemon 刷新已覆盖,等价 |

## 域 3:时间线与滚动引擎

原版:`SessionDetail.vue`、`SessionTimelineRow.vue`、`src/session-timeline*.mjs`、`tool-renderer.js`、`styles/detail.css`。GPUI:`timeline.rs`、`timeline_view.rs`、`tool_render.rs`、`file_reference.rs`。

### 滚动引擎(P0 核心)

| # | 功能 | 原版行为 | 状态 | 验收标准 |
|---|---|---|---|---|
| 29 | 虚拟化 | tanstack fork + 按 kind 估高 + 稳定 key | P- | GPUI 用测量 ListState;条目锚定结构上覆盖大部分场景 |
| 30 | 像素缓冲 | 上下各 4 视口 | P- | overdraw 600px≈1 视口;快速滚动测试无行塌缩 |
| 31 | 尺寸补偿 | 未测必补偿/媒体 settle 补偿/前向补偿 | P- | 条目锚定等价大部分;锚行自身增高会推移(记录) |
| 32-34 | 用户滚动检测+抑制+结算 | wheel 意图/450ms 看门狗/settle 锚定恢复 | X | **P0-1**:刷新不移动视口;settle 后锚行恢复原偏移 |
| 35 | 图片渐进 | 48px 占位/settle 补偿/错误态 | X | 图片加载不跳动;错误显示 "Image unavailable · alt" |
| 36 | reader-anchor | 捕获/三级解析恢复 | X | **P0-2** |
| 37 | 阅读状态缓存 | LRU 12 会话(位置+披露) | X | **P0-2** |
| 38 | follow-tail | 距底 50px 自动吸附;向上脱离 | P- | **补近底自动吸附与回底指示**(当前仅 End 键) |
| 39 | 远跳重试 | 双 rAF 重对齐 | P- | 内建 scroll_to_reveal 等价 |
| 40 | 稳定揭示 | ≤8 帧不重叠才显示 | X | P2(GPUI 布局期测量,风险低) |
| 41-42 | scrollMargin/resize | — | P | 结构等价 |
| 43-46 | live 补丁协调 | 滚动中不 IPC/settle 提交/tailOnly/刷新不跳位 | X | **P0-1**;纯追加路径已对齐(#45) |
| 47 | 全局刷新隔离 | 详情打开期间延迟目录刷新 | P | 等价 |
| 48 | 增量补丁协议 | cursor patch | X | P2(全量重载,大会话成本) |

### 页头与行类型

| # | 功能 | 原版行为 | 状态 | 验收标准 |
|---|---|---|---|---|
| 1-2 | 页头 | 项目/路径/来源徽标/相对时间/branch | P- | 补齐字段与来源配色 |
| 3 | 阅读进度条 | 2px sticky 百分比 | X | P2 |
| 4 | 消息分页导航 | first/prev/翻牌计数/next/last | X | P1:键盘已有(超集);补计数显示 |
| 5 | 字号调节 | 6 档 + toast | X | P2 |
| 6 | 加载态/防闪烁 | Loading + is-preparing | X | P2 |
| 7-8 | 消息卡片/空文本 | 气泡区分/(no text content) | P- | 补气泡底色与空文本占位 |
| 9 | 内嵌 thinking | `_thinking` 折叠 | X | **P1**:data 装配 + 渲染 |
| 10-11 | 独立 thinking/meta | 折叠+预览 | P- | 样式差异(thinking 多出 head) |
| 12 | 通用工具卡 | 图标/预览/Raw 切换 | P- | SVG 图标替代 ⚙ 文本(P2) |
| 13 | Read 查看器 | gutter 检测/Show all 可点 | P- | **修 "Show all N lines" 不可点**;gutter 检测 |
| 14-15 | Write/Edit diff | 完整移植 | P | 已对齐(diff 200 行截断记录为 P2) |
| 16 | Bash | 描述行/✓✗ 按行着色 | P- | P2 补齐 |
| 17 | exec CodeAct | JS 高亮/Script 头/JSON 高亮/截断 | X | P1 |
| 18-19 | 通用输出 | hero 卡/表格/长字段展开/数组内联 | P- | P2 细节补齐 |
| 20 | Agent/Task 行 | 名称/描述/跳转子代理/Prompt+Result | X | **P1**(与 SubagentDetail 同批) |
| 21 | Skill 卡 | badge/名/args/SKILL.md | M | P1 |
| 22 | Workflow agents | phase 分组/行跳转 | M | P1 |
| 23 | workflow-tools | 通用卡 | P- | 等价 |
| 24 | summary 行 | 折叠 compact markdown | X | P1(summaries 表未加载) |
| 25 | 截断全文加载 | ≥10000 字符点击加载 | X | P1 |
| 26 | ?focus 深链 | 定位+2s 高亮 | X | P2(桌面无 URL;等价物=从 FTS/记忆跳转定位) |
| 27 | error 呈现 | 徽标+红边+Error 标题 | P- | 已对齐 |
| 49 | markdown | 代码/链接/表格/图片 | P | **P1:每帧重解析需 memoize(性能)** |
| 50-51 | 图片安全/文件引用 | 协议白名单/多 root/行内 code 链接化 | P- | P2 |
| 52 | 搜索高亮 | query 高亮时间线文本 | X | P2 |
| 53 | 复制按钮 | 原版无 | P+ | GPUI 超集 |
| 54 | SubagentDetail 页 | 全量列表/返回父会话 | X | **P1**(整页缺失) |

## 域 4:Memory 视图

原版:`MemoryList.vue`(810 行)。GPUI:`views.rs` MemoryView。

| # | 功能 | 原版行为 | 状态 | 验收标准 |
|---|---|---|---|---|
| 1 | 数据形状 | 全字段含 anchors/message_start/end | P- | 补查询 anchors 与消息范围 |
| 2 | 行内容 | path 文件名+摘要+相对时间+归档按钮 | P- | 文件名为标题、fmtListTime/fmtRelative |
| 3 | 健康徽标 | 原版死代码 | X | 豁免(不跟进) |
| 4 | Active/Archived 切换 | 互斥过滤 + 侧栏互斥高亮 | X | **P0-4** |
| 5 | 搜索过滤 | path+summary 子串 | X | Memory 有搜索框 |
| 6 | 排序 | newest/oldest + `s` | P- | 切换按钮(默认已一致) |
| 7 | 项目过滤联动 | projectFilter 过滤记忆 | X | Memory 域项目过滤生效 |
| 8 | 详情页 | 整页:相对路径/摘要/相对时间/Back | P- | 随 P0-3/5 重做详情 |
| 9 | 来源会话链接 | 跳转 `?focus=message_start` | X | **P0-5** |
| 10 | 消息范围 | `a1b2…→e5f6…` | X | 详情显示 |
| 11 | 详情 Body | 懒加载/加载态/source-rendered 切换 | P- | source 切换 |
| 12 | 锚点展示 | `path:line` 按钮,失效禁用+title,可开编辑器 | X | 详情锚点区,点击经 file_reference 打开 |
| 13 | 归档/恢复 | 写 deleted_at/deleted_reason,乐观更新 | X | **P0-3** |
| 14 | 批量操作 | 复选框/Shift 范围/x 键/D 批量 | X | P1(随 P0-3) |
| 15 | Undo | 5s toast/u/Cmd+Z | X | P1(随 P0-3) |
| 16 | 键盘导航 | j/k/Enter/Esc/x/D/u | X | **P0-13** |
| 17 | Cmd+2/3 | Active/Archived | X | 见 P0-12 |
| 18 | 空态 | 按 view/搜索区分文案 | P- | Active 空但 Archived 有内容时有提示 |
| 19 | FTS/创建入口 | 原版均无 | P | 一致 |

## 域 5:Activity 与 Recap

原版:`Activity.vue`(730 行)、`ActivityLedger*.vue`、`FlapNumber.vue`、`Recap*.vue`、`recap/*`。GPUI:`views.rs`。

**统计口径已对齐**(tokens=messages UNION summaries 的 input+output 按日聚合,不滤 visibility/agent;活动=session 与区间重叠;streak=连续 tokens>0 日,今日无数据不打断)。

| # | 功能 | 原版行为 | 状态 | 验收标准 |
|---|---|---|---|---|
| A1 | 统计卡 5 张 | Lifetime/Peak/Longest task/Current streak/Longest streak | M | 补 streak 两卡;格式口径(d/h 档)对齐 |
| A2 | 三档 tabs | Daily/Weekly/Cumulative | M | 切换三档图表 |
| A3 | GitHub 热图 | 371 格/月标签/图例/tooltip/点击选中 | X | **P0-6** |
| A4 | 周柱状图 | ISO 周/53 周/hover | X | 随 A2 |
| A5 | 累计折线 | 逐日累计 line+area | X | 随 A2 |
| A6-A10 | 活动账本 | 月分块/日账本/三分类分组/行跳转/噪音过滤 | X | **P0-6** |
| A11 | 数据/刷新 | SQL 等价;onIndexUpdated 实时 | P- | 补构建后实时刷新 |
| A12 | FlapNumber | 翻牌动画(Session 域) | X | P2 |
| A13 | 维度 | 双方均仅按天 | P | 一致 |
| R1 | 周报列表 | 时间轴/印章/persona/metrics/kind 过滤/年份分组 | M | **P1**:列表呈现重做 |
| R2 | 空态 CTA | 生成按钮 | M | 随 R3 |
| R3 | 生成入口 | 弹窗四选项+命令复制 | X | P1 |
| R4-R9 | 详情五卡 | Cover/Path/Vibe/Workflow/Closing + 键盘翻页 | X | **P0-7** |
| R10 | archetype 主题 | 7 调色板+过渡 | X | 随 P0-7 |
| R11 | 导出 | Copy image/Export PNG(离屏渲染) | X | P1 |
| R12 | 自动刷新 | recap 目录 watcher | X | P1 |
| R13 | 上周对比 | 原版无 | P | 一致 |

## 域 6:Settings

原版:`Settings.vue`(537 行)、`main/provider-settings.ts`。GPUI:`views.rs` SettingsView、`data.rs`、`main.rs`。

| # | 功能 | 原版行为 | 状态 | 验收标准 |
|---|---|---|---|---|
| 1 | 加载/实时刷新 | onIndexUpdated 重载 | P- | daemon 构建后已重载 |
| 2 | 数据源状态卡 | 状态灯/statusText/lastIndexed/sessionCount/品牌色 | X | **P1**(与 P0-9 校验同批) |
| 3 | Root 校验 | 非绝对/不存在→error 红边 | X | **P0-9** |
| 4 | Browse/保存方式 | 目录选择对话框,选即存 | M | P1:对话框;**GPUI 手输+空=恢复默认是超集,保留** |
| 5 | 持久化 | tmp+rename 原子写 | P- | clear_provider_root 补原子写(P2) |
| 6 | 生效时机 | 即时 stop/start 服务 | M | **P0-9**:daemon targets/registry 跟随 settings |
| 7 | Index location+Reveal | db 路径 + showItemInFolder | X | P1 |
| 8 | Auto-refresh 开关 | 默认 on,即时生效 | X | P1(daemon always-on 是超集,提供开关) |
| 9 | Editor scheme | 5 项点选即存,未知回落 | P | 已对齐 |
| 10 | Recap 目录 | 可配置 | X | P1 |
| 11 | About 版本 | 版本号展示 | X | P2 |
| 12 | 手动 Rebuild | 强制全量/进度禁用/错误红字/writer-busy 处理 | X | **P0-8** |
| 13-14 | 附加字段/无外观设置 | 双方一致未用/均无 | P | 一致 |

---

## 修复进度

- **M4.0(批次 1)✅ 2026-09-08**:P0-10、P0-11 修复并验证(cargo 门禁 + 7 单测 + desktop E2E 11/11)。

## 修复路线(建议批次)

1. **批次 1 — 稳定性 P0**:P0-10(无 tray 关窗)、P0-11(panic)——先保命。
2. **批次 2 — 滚动/阅读 P0**:P0-1、P0-2(时间线 settle/锚定/状态缓存)+ follow-tail 吸附。
3. **批次 3 — Memory P0**:P0-3/4/5/13(归档+切换+跳转+键盘,一个域一次交付)。
4. **批次 4 — Settings P0**:P0-8/9(rebuild+root 校验/热生效)。
5. **批次 5 — Activity/Recap P0**:P0-6/7(热图+账本;五卡+主题)。
6. **批次 6 — 全局键盘 P0-12** 与高频 P1(排序切换、branch 过滤、时间格式、防抖、噪音折叠、FTS 跳转到命中消息、markdown memoize、Show all 可点)。
7. 之后按 P1 清单逐域消化,P2 收尾。

每批完成后:状态列更新 + 相关 D 场景断言增强 + 全量 E2E 回归。
