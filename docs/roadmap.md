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
- [x] `capability`（单入口能力叙述）—— `deepen --capability [--kind …] [--domain …]`，渲染进 reference 入口页

## 阶段 D —— 增量与 agent 接入 ⚠️

- [x] 深挖结果跨 rebuild 存活（`subject_key`/`domain_key` 稳定键 + relink）—— 端到端已验证：笔记存活、id 漂移后正确重链、领域消失变 stale 三条路径
- [x] 领域档案/术语表按 digest 失效、`--only-stale` 跳过
- [x] `catlas query`（FTS5 全文检索）—— trigram 分词、索引随 build/deepen 重建、`--json`
- [x] `catlas sync`（增量）—— sha256 变更检测 + 解析缓存复用，见下方「sync 的变更检测为什么不看 git」
- [ ] MCP server 暴露给 Claude Code —— 用 `catlas query --json` 先顶上，未做

## sync 的变更检测为什么不看 git

`sync` 判断「哪些文件变了」用的是每文件内容的 SHA-256，而不是 git 提交记录。原因：

1. **看不见未提交的改动。** 开发最频繁的循环是「改一行 → 跑 sync 看效果」，`git log` 只反映已提交状态，恰恰漏掉这个循环。要补得用 `git status --porcelain` 探测脏工作区，而一旦探测到脏文件，还是得回头去哈希那些文件。
2. **git 记录的是「哪个 commit 碰了哪些文件」，不是「磁盘字节与我上次索引时是否不同」。** 文件被 revert 回原样、rebase 重写对象哈希、换机器新 clone 后构建——git 的 commit sha 都指不到「我上次解析的那个字节状态」。
3. **代价不对称。** 现在的 build 里 `git log` 历史信号（churn 全量走一遍、subject 限 4000 条）是**最慢**的部分（十年老库要走几分钟）。用 git 驱动 sync 既保留这个成本又添脆弱性；sha256 遍历哈希几千个文件单线程都不到一秒。
4. **sha256 唯一能覆盖所有状态**：干净树、脏树、rebase 后、非 git 项目、别人机器建的库——全统一。

而 sync 真正的难点不在「找变更文件」，在于 **L1/L2 是全局聚合**：`refs` 解析（`unique_method` 需要全项目符号表）、领域投票、trace BFS、FTS 索引都无法按文件增量。所以 sync 的实现是「**复用 build 的整条流水线，只跳过 tree-sitter 解析这一步**」——未变更文件的解析结果按内容哈希缓存在 `parse_cache`，命中即复用；产出与全量 `build` 逐字节一致。只有工具版本升级（改了语法树或 schema）时才丢弃缓存全量重算，靠 `tool_version` 表判定。

## 已知未做 / 限制

- 跨项目知识融合：仅 schema 预埋 `project_id`，无实现
- JSP / JS 前端链路：不解析
- 非 Java 项目：probe 架构支持扩展，但未实现其它语言
- 配置中心持有的 MQ topic / 外部配置：无法从仓库反推，渲染时以 `${占位符}` 原样标注
- `catlas query` 用 trigram 分词：**2 个及以下字符的检索（如单字「价」、两字「特价」）不会命中**，这是 trigram 的固有限制——需要 ≥3 字符（或 ≥3 字节的英文/标识符）。中文领域名、常量注释通常是 4 字以上，影响可控。
- `catlas sync` 只缓存 Java 解析（tree-sitter 是唯一重计算）；XML/properties/git 每次仍全量重扫——它们便宜，暂不值得缓存。git 历史信号每次仍重跑，是 sync 后剩余的主要耗时。

## 验证状态

- 109 个单测全绿（含 fixtures：java/xml/properties/git/domain 划分/prompt 解析/query/sync）
- `capability` 新增 4 个单测（响应容错 / 渲染分节 / 稀疏省略 / 弱输出标记），共 113 个
- `yaoex-promotion` 全量构建实测：20 秒，模块12/文件1714/符号22394/调用边47807/表140/字段1434/入口506/领域43/链路455
- `deepen --domain defective` 实测：产出领域解读 + 术语表，质量抽查通过（准确扒出「捡漏专区」别名、时间交叉互斥规则、`batchUpdateSortNum` 注释自曝无用等）
- `deepen --capability` 实测：单入口能力叙述端到端跑通（demo 项目 `GET /coupon/list` 产出摘要/入参/执行过程/副作用/约束/易踩坑，二次运行按 digest 正确跳过）
- `L2 存活 rebuild` 实测：三场景全过——(1) 全量 build 后笔记存活且 `subject_id` 正确重链；(2) 插入新领域使 coupon id 从 1→2，笔记跟着重链到 2；(3) `exclude` 掉 order 领域后其笔记变 `stale` 而非错误渲染

## 待办（下次会话优先）

1. `catlas query` 短查询（≤2 字符）优化：可选方案是查询前做 CJK 切词，或对短查询回退到 LIKE 扫描
2. `catlas sync` 加速 git 历史信号（增量拉取新 commit、按 HEAD 缓存），目前每次 sync 仍全量重跑 `git log`

