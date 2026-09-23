# CodeAtlas

为**存量老项目**生成项目级业务领域知识库的通用工具。产出人能读、Claude Code 能用的文档，随代码增量演进。

典型目标：多年历史、几十个模块、几十万行代码、技术栈老旧、**零文档**的项目。

## 一句话原理

老项目里最可靠的业务知识不在代码结构里，而在三处：**注释（尤其枚举/常量类里的中文注释）、表结构、git 历史**。`catlas` 把这三处确定性抽取出来（不花钱、可复现），再用大模型在证据之上写领域叙述（每条挂证据、可追溯、可失效重算）。

## 快速开始

```bash
# 1. 在目标项目根目录初始化并构建（零 token，秒级）
catlas init /path/to/legacy-project
#    → 生成 .codeatlas/codeatlas.db（SQLite 真相层）

# 2. 生成人类可读文档
catlas render --claude-md
#    → .knowledge/（README + 领域 + 术语表 + 参考清单）+ CLAUDE.md 托管块

# 3.（可选）花 token 深挖某个领域的业务语义
catlas deepen --domain defective --dry-run   # 先看预估 token
catlas deepen --domain defective             # 生成领域解读 + 术语表
catlas render --claude-md                    # 把深挖结果写回 Markdown

# 4.（可选）为单个入口生成能力叙述（调用者视角：入参/副作用/易踩坑）
catlas deepen --capability --dry-run         # 先看预估 token
catlas deepen --capability --kind dubbo      # 只做 Dubbo 接口；--kind http/job/mq 同
```

其它命令：`catlas build`（全量重建）、`catlas sync`（增量，复用未变文件的解析缓存）、`catlas query "关键词" [--json]`（全文检索）、`catlas status`（各层统计）、`catlas domains [--json]`。

## 接入 Claude Code

```bash
# 注册为 MCP server，让 Claude Code 直接查询知识库（只读工具）
claude mcp add catlas -- catlas mcp --path /path/to/legacy-project
```

暴露三个只读工具：`search`（全文检索）、`domains`（领域清单）、`status`（各层统计）。

## 产物结构

```
目标项目/
├── CLAUDE.md              ← catlas 托管的入口索引（标记块内，块外手写不动）
├── .codeatlas/                 ← 真相层（SQLite + 配置），gitignore
│   ├── codeatlas.db
│   └── domains.toml       ← 领域划分的人工覆盖
└── .knowledge/            ← 派生的 Markdown 视图，可删可重建
    ├── README.md          ← 项目全貌 + 领域地图
    ├── glossary.md        ← 中文术语 ↔ 代码标识符/表/枚举值
    ├── domains/<key>.md   ← 领域档案：职责/生命周期/业务规则/⚠️坑/关键链路
    └── reference/         ← tables / entrypoints / jobs / mq
```

## 文档

- [docs/overview.html](docs/overview.html) — **它为什么能解决老项目的知识沉淀问题**（核心机制通读版，浏览器打开）
- [docs/improvements.html](docs/improvements.html) — 对照同类开源项目得出的改进建议与落地顺序
- [docs/design.md](docs/design.md) — 三层知识模型、SQLite schema、关键设计决策
- [docs/roadmap.md](docs/roadmap.md) — 实施计划、各阶段状态、已知未做项

## 构建

```bash
cargo build --release   # 需要 rustc/cargo，依赖 tree-sitter-java + rusqlite(bundled)
cargo test              # 单测（含 fixtures）
```

LLM 调用复用 `$ANTHROPIC_BASE_URL` / `$ANTHROPIC_AUTH_TOKEN`（与 Claude Code 相同的环境变量）。

## 许可 / 归属

公司内部工具。
