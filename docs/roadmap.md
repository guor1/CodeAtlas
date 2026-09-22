# CodeAtlas 实施计划与状态

> 状态图例：✅ 已实现并验证 · ⚠️ 已实现未端到端验证 · 🔜 计划中 · ❌ 不做

## 阶段 A —— 骨架与 L0 事实层 ✅

抽取器与存储，构建全项目确定性事实。

- [x] SQLite schema + 迁移（`src/store/`）
- [x] Maven POM 模块树
- [x] Java 符号抽取（tree-sitter：类/接口/枚举/枚举成员/方法/字段/常量 + Javadoc + 注解）
- [x] MyBatis mapper XML → 表/字段/表访问（含 `@TableName`、`ON DUPLICATE KEY UPDATE` 特判）
- [x] Dubbo XML → 服务入口；Spring XML → MQ 消费者；`@XxlJob` → 任务
- [x] 调用边解析（字段类型 + 唯一方法名启发式，带 confidence）
- [x] git 历史：churn、提交 subject、分支名

## 阶段 B —— L1 结构层 ✅

- [x] 领域划分（四路信号加权投票 + 变体折叠 + 命名空间识别 + 人工覆盖）
- [x] 入口→数据表链路 trace（BFS，含 override 动态分发边）
- [x] Markdown 渲染（README/领域/术语/参考清单 + CLAUDE.md 托管块）

## 阶段 C —— L2 深挖 ✅

- [x] LLM client（直连 Messages API，curl 子进程 + 凭证文件隔离）
- [x] 证据 pack 预算裁剪
- [x] 领域档案 + 术语表生成（含解析失败重试、截断检测）
- [x] prompt→缓存（只缓存已解析成功的响应）
- [ ] `capability`（单入口能力叙述）—— schema 就位，prompt 未接

## 阶段 D —— 增量与 agent 接入 ⚠️

- [x] 深挖结果跨 rebuild 存活（`subject_key`/`domain_key` 稳定键 + relink）
- [x] 领域档案/术语表按 digest 失效、`--only-stale` 跳过
- [ ] `catlas sync`（只重解析变更文件）—— **未实现**，当前 `build` 全量重建
- [ ] `catlas query`（FTS5 全文检索）—— **未实现**，schema 有 `search_fts` 表，命令和索引填充没接
- [ ] MCP server 暴露给 Claude Code —— 用 `catlas query --json` 先顶上，未做

## 已知未做 / 限制

- 跨项目知识融合：仅 schema 预埋 `project_id`，无实现
- `catlas query` / `catlas sync`：CLI 未实现，但 README/生成文档里已提前占位（**注意：现阶段会报「找不到子命令」**）
- JSP / JS 前端链路：不解析
- 非 Java 项目：probe 架构支持扩展，但未实现其它语言
- 配置中心持有的 MQ topic / 外部配置：无法从仓库反推，渲染时以 `${占位符}` 原样标注

## 验证状态

- 101 个单测全绿（含 fixtures：java/xml/properties/git/domain 划分/prompt 解析）
- `yaoex-promotion` 全量构建实测：20 秒，模块12/文件1714/符号22394/调用边47807/表140/字段1434/入口506/领域43/链路455
- `deepen --domain defective` 实测：产出领域解读 + 术语表，质量抽查通过（准确扒出「捡漏专区」别名、时间交叉互斥规则、`batchUpdateSortNum` 注释自曝无用等）

## 待办（下次会话优先）

1. 端到端验证「L2 存活 rebuild」（阶段 D 最后一步，被打断未跑完）
2. 实现 `catlas query`（FTS5）与 `catlas sync`（增量）
3. 决定 `capability` 是否接入
