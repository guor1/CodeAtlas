# CodeAtlas 设计

## 问题

公司有大量存量老项目：代码多、业务分散、技术栈老旧、无文档。新人或 Claude Code 接手时，只能在几十万行代码里硬啃。本工具的目标是给这类项目自动生成一份**项目级业务领域知识库**，人能读、Claude Code 能用，随代码增量演进。

## 核心设计：三层知识模型

整个方案的骨架是把「事实」和「叙述」严格分层，叙述必须挂证据。

### L0 —— 证据层（确定性抽取，零 LLM）

从源码、配置、git 历史里扒出来的、不会出错的事实：

- 模块树（Maven POM）、文件、符号（类/方法/字段/枚举成员 + Javadoc 原文 + 行号）
- 数据表与字段：mapper XML 的 `resultMap` 反推（目标项目无 DDL，这是唯一来源）+ `@TableName`
- 表访问：哪个 DAO 方法读写哪张表（select/insert/update/delete）
- 入口清单：Dubbo 接口（`<dubbo:service>`）、HTTP 路由（Spring MVC 注解）、XXL-Job 任务、MQ 消费者（listener ↔ topic）
- 调用边：import + 字段类型启发式解析，带 `confidence`（1.0 / 0.6 / 0.3），解析不了降级存原始文本
- git 信号：每文件 churn、触碰它的提交 subject、分支名词表

### L1 —— 结构层（确定性推导）

从 L0 纯计算得出：

- **领域划分**：包路径、表名前缀、git 共变、调用关系四路信号加权投票（权重 3/2/1/1），迭代传播到不动点；同义拼写归一（`groupBuying`/`groupbuying`/`group_buy`）；命名空间自动识别（同一路径深度出现率 ≥90% 的段是 `com/company/product`，不是领域）；人工 `domains.toml` 可覆盖且优先级最高
- **能力链路 trace**：从入口 BFS 沿调用边下探（深度 ≤6），含接口→实现的 override 边（老项目一律经接口调服务，缺这条链路会断在接口处），到触表符号终止

### L2 —— 叙述层（LLM 生成，逐条挂证据）

- **术语表**：中文术语 → 定义/别名/代码标识符
- **领域档案**：职责、核心概念、生命周期与状态机、业务规则、⚠️已知陷阱、待确认问题
- 每条记录强制持有 `source_digest`（证据摘要）+ `status`；证据变了自动失效重算

这三层同时解决三个问题：**反幻觉**（叙述可追溯）、**可审计**（证据引用）、**可增量**（digest 失效）。

## 存储：SQLite 为唯一真相，Markdown 是视图

- `codeatlas.db` 是真相层。Markdown 是 `render` 派生的视图，可删可重建
- 所有表带 `project_id`，为后期**跨项目知识融合**预留（v1 不实现，schema 已就位）
- `llm_cache` 按 prompt+model+max_tokens 哈希命中，重跑免费；**只有解析成功的响应才写缓存**（坏响应不固化）

## 抽取引擎：probe 架构（可复用性的关键）

```rust
trait Probe {
    fn detect(&self, root) -> Option<Confidence>;
    fn extract(&self, ctx) -> Result<()>;
}
```

v1 实现：maven / java(tree-sitter) / mybatis-xml / spring-xml / dubbo-xml / properties / git。换一种老项目只需加一个 probe（Spring Boot 注解式、MyBatis 注解式、Go…），核心与 schema 不动。

## LLM 层

- 直连 Anthropic Messages API（复用 `$ANTHROPIC_BASE_URL`/`$ANTHROPIC_AUTH_TOKEN`），内置在工具里，不 shell out
- 三类 task：`glossary_mine` / `domain_dossier` /（`capability` 预留）
- system prompt 硬约束：**只依据给定证据、不确定写 UNKNOWN、区分「代码行为」与「业务意图」、只输出 JSON**
- 证据 pack 按预算裁剪（枚举注释 > 表结构 > 入口 > 链路 > 提交 > 源码节选），跨领域共享类型（如 `PromotionTypeEnum`）显式标注避免误归

## 关键决策记录

| 决策 | 理由 |
|---|---|
| Rust 独立 CLI | 用户指定；可进 CI、脱离 Claude 可用 |
| 自带 tree-sitter 解析，codegraph 可选 | 不硬绑外部工具私有 schema |
| 两段式生成 | 先零成本全量骨架，再按需花 token 深挖 |
| SQLite 真相 + Markdown 视图 | 兼顾程序查询与人类/agent 可读，融合可后加 |
| 注释/枚举/commit 为一等证据 | 老项目这些比代码结构可靠（实测验证） |

## 为什么这样设计对老项目有效

在 `yaoex-promotion`（12 模块 / 1472 Java / 23 万行 / 5003 commit / 零文档）上验证的三个发现：

1. MySQL 的 `ON DUPLICATE KEY UPDATE col` 会骗过朴素表名提取 → 必须特判，否则凭空造表
2. 纯语法调用图死在接口边界 → 必须建 override 边，链路从 4 条变 455 条
3. 包名里的项目命名空间不是领域 → 按出现率自动识别，不能硬编码
