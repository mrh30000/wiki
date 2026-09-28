# 两层源文章索引重构 · 设计说明

- **日期**：2026-09-28
- **状态**：已评审通过（设计阶段），待生成实施计划
- **范围**：P0 索引层 + P1 廉价管道与队列 + P2 交互规模与图谱（四期合一）
- **参考实现**：`open-source/WeKnora`（腾讯，Go + Postgres/Redis）
- **目标规模**：10,000 源文章可顺畅使用，架构上留出 100,000 的路径

---

## 1. 背景与目标

### 1.1 问题陈述

`llm_wiki` 是本地优先的 Tauri 桌面应用，内容以明文 Markdown 存放于磁盘。它**没有内容数据库**：
检索走文件系统全量遍历，状态散落在 `.llm-wiki/*.json`，向量库虽已存在但未建 ANN 索引。

在 10,000+ 源文章规模下，当前实现存在**功能性断崖**而非仅仅性能退化——关键词检索会静默漏检。

### 1.2 目标

1. 检索、列表、建图三条热路径**不再随语料规模线性退化**。
2. 源文章**本身可被检索**，无需先经 LLM 合成。
3. 导入 10,000 源文章**零 LLM 调用**（成本可预测）。
4. LLM 合成改为**用户显式触发**的唯一花钱路径。
5. 保持本地优先：明文 Markdown、用户拥有、git 可版本化、索引可丢弃可重建。

### 1.3 非目标

- 不引入服务端依赖（Postgres / Redis / 消息队列）。
- 不改变现有排序算法的语义（RRF 融合、图谱配额）。
- 不迁移到 WeKnora 的技术栈（Go / asynq / docreader gRPC / 多租户）。

---

## 2. 现状证据

以下均已针对当前工作区内容核实。

| # | 问题 | 证据 |
|---|---|---|
| E1 | 关键词检索在 **10,000 个 wiki Markdown 文件**处硬性断崖，且**静默截断**（只打 stderr） | `src-tauri/src/commands/search.rs:23` `MAX_SEARCH_FILES = 10_000`；`:364-367` `break` + `eprintln!` |
| E2 | 每次检索**全量遍历并全文读盘**，无倒排索引、无缓存 | `search.rs:352` `WalkDir::new(&wiki_root)`；`:370` `fs::read_to_string` |
| E3 | 每查询把**整个语料正文**驻留内存（`graph_pages` 无条件构建，纯关键词查询亦然） | `search.rs:208` `content,`；`:399` `graph_pages.insert` |
| E4 | 向量库**未建 ANN 索引**，退化为暴力扫描；而文档声称「快速 ANN 检索」 | `src-tauri/src/commands/vectorstore.rs` 无 `create_index`/`IvfPq`/`Hnsw`；`README_CN.md:212` |
| E5 | 目录树**整棵递归**通过单个 IPC 返回，客户端全量物化 | `src-tauri/src/commands/fs.rs:1592` `build_tree`（默认 `maxDepth=30`）；`sources-view.tsx:84` |
| E6 | 知识树**逐页串行 IPC**（10K 页 = 10K 次往返） | `src/components/layout/knowledge-tree.tsx:71-77` |
| E7 | 每行渲染**递归整棵子树**求文件数，近似 O(n²) | `src/components/sources/sources-view.tsx:810` `countFiles(node.children)` |
| E8 | 列表**无虚拟化**，行只增不减，滚到底即全量挂载 | `sources-view.tsx:31-32,735` `SOURCE_TREE_*`；全仓无 react-window / tanstack-virtual |
| E9 | 图谱**一页一节点、无节点上限**，ForceAtlas2 全量布局，且建图每轮读全站文件 | `src/lib/wiki-graph.ts` 无节点上限常量；`graph-view.tsx` 仅视觉参数 |
| E10 | 摄入队列**每次状态变更全量重写** JSON | `src/lib/ingest-queue.ts:106` `JSON.stringify(toSave, null, 2)` |
| E11 | `ingest-cache.json` 全量读+解析+重写；**以文件名为键**（跨目录会撞） | `src/lib/ingest-cache.ts:32-56`；`entries[sourceFileName]` |
| E12 | 文件监视队列/snapshot 每任务全量重写 | `src-tauri/src/commands/file_sync.rs:767,877` |
| E13 | 文档与代码不一致：README 称「同时搜索 wiki/ 和 raw/sources/」，代码只走 `wiki/` | `README_CN.md:208` vs `search.rs:349` `Path::new(&project_path).join("wiki")` |

### 2.1 影响范围

- 非测试代码中 `listDirectory` 调用点 27 处、`readFile` 调用点 30+ 处，**高度集中于查询层**。
- Tauri 命令注册 71 个（`src-tauri/src/lib.rs`），`search_project` 注册于 `lib.rs:656`。
- 已有可扩展钩子：`rebuild_wiki_index`（`src-tauri/src/commands/project_maintenance.rs:288`）当前只重建 `wiki/index.md`。

---

## 3. 不变量（不可协商）

1. **磁盘是真相源**：`wiki/*.md`、`raw/sources/*` 为明文、人类可读、git 可版本化。
2. **索引是可丢弃派生物**：`index.db` 与 LanceDB 可被删除并从磁盘全量重建；重建后检索结果一致。
3. **写盘先于更新索引**：索引更新失败不得阻断写入；允许最终一致。
4. **热路径无全量遍历**：检索、列表、建图均不得 `WalkDir` 全站。
5. **排序语义不变**：现有 RRF 融合与图谱配额逻辑保持原样，仅替换候选生成层。

---

## 4. 目标架构

### 4.1 分层

| 层 | 职责 | 现状 | 目标 |
|---|---|---|---|
| L0 真相源 | 明文内容 | `wiki/*.md`、`raw/sources/*` | **不变** |
| L1 控制面 | 元数据 + 关键词 + 图谱边 + 队列 | ❌ 无（文件遍历替代） | ✅ `.llm-wiki/index.db`（SQLite + FTS5） |
| L2 检索面 | 向量 + ANN | LanceDB，**无索引** | ✅ 同一 LanceDB，加 `IVF_PQ`/`HNSW` + 维度分区 |
| L3 排序 | RRF 融合 + 图谱配额 | ✅ 已实现 | **不变** |
| L4 交互 | 列表与图谱渲染 | 无虚拟化、无上限 | 键集分页 + 虚拟化 + 节点上限 |

### 4.2 存储划分决策

**采用方案 A：控制面 / 检索面二分。**

| | A（采用） | B（评估后弃用） | C（弃用） |
|---|---|---|---|
| 元数据 / 队列 | SQLite（事务、唯一键、job claim） | LanceDB 标量列——**非 OLTP**，频繁小更新为其弱点 | SQLite |
| 关键词 | FTS5（成熟） | Tantivy FTS（0.27.2 API 未验证） | FTS5 |
| 向量 | LanceDB + ANN（复用现有） | LanceDB + ANN | sqlite-vec（新依赖 + 破坏性迁移） |
| 新增依赖 | `rusqlite`(bundled) | 0 | 2 |

**决策理由**：`sources`/`pages`/`chunks`/`links`/`jobs` 元数据与带 claim 语义的任务队列*本来就必须*关系型存储，SQLite 无法回避；既然引入，FTS5 顺带解决关键词检索。`tantivy` 已在依赖树内（经 `lancedb → lance-index → tantivy`），但**方案 B 暂不采用（YAGNI）**——若 P0.3 的 ANN 实测暴露 Lance 侧结构性优势，再行评估。

**副作用**：`rusqlite` bundled 增加构建体积与首次编译时间；Windows 下无需外部 SQLite。

---

## 5. 数据模型（`index.db`）

```sql
-- 每个源文章一行（对应 WeKnora knowledges）
CREATE TABLE sources(
  id TEXT PRIMARY KEY, path TEXT UNIQUE NOT NULL, hash TEXT, size INTEGER,
  mtime INTEGER, parse_status TEXT NOT NULL DEFAULT 'pending', error TEXT,
  created_at INTEGER, processed_at INTEGER);
CREATE INDEX idx_sources_status ON sources(parse_status);

-- 每个 wiki 页一行
CREATE TABLE pages(
  id TEXT PRIMARY KEY, path TEXT UNIQUE NOT NULL, title TEXT, type TEXT,
  hash TEXT, mtime INTEGER, frontmatter TEXT, source_id TEXT, deleted_at INTEGER);
CREATE INDEX idx_pages_source ON pages(source_id);
CREATE INDEX idx_pages_type   ON pages(type);

-- 检索单元（对应 WeKnora chunks），含父子分块列
CREATE TABLE chunks(
  id TEXT PRIMARY KEY, page_id TEXT NOT NULL, chunk_index INTEGER NOT NULL,
  heading_path TEXT, text TEXT NOT NULL, content_hash TEXT,
  parent_chunk_id TEXT, chunk_type TEXT NOT NULL DEFAULT 'text');
CREATE INDEX idx_chunks_page ON chunks(page_id, chunk_index);

-- 图谱边：替代运行时读全站文件建图
CREATE TABLE links(
  src_page_id TEXT NOT NULL, dst_page_id TEXT, dst_raw TEXT,
  weight REAL NOT NULL DEFAULT 1);
CREATE INDEX idx_links_src ON links(src_page_id);
CREATE INDEX idx_links_dst ON links(dst_page_id);

-- 关键词检索：替代 E1/E2/E3
-- 分词方案已实测确定（见 §13.1）：CJK 逐字切分 + unicode61 + 短语查询。
-- body/title 存「切分后」文本用于匹配；body_raw 存原始文本（UNINDEXED）供摘要展示。
-- 切分函数 segment_for_fts(s) 在写入与查询两侧对称应用，查询侧以短语引号包裹。
CREATE VIRTUAL TABLE pages_fts USING fts5(
  title, body, heading_path, page_id UNINDEXED, body_raw UNINDEXED,
  tokenize = 'unicode61');

-- 持久化队列 + DLQ（对应 WeKnora task_pending_ops / task_dead_letters）
CREATE TABLE jobs(
  id INTEGER PRIMARY KEY AUTOINCREMENT, source_id TEXT NOT NULL, op TEXT NOT NULL,
  dedup_key TEXT NOT NULL DEFAULT '', state TEXT NOT NULL DEFAULT 'pending',
  attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT,
  claimed_at INTEGER, created_at INTEGER NOT NULL DEFAULT (unixepoch()));
CREATE INDEX idx_jobs_claim ON jobs(state, dedup_key, attempts, id);

CREATE TABLE job_dlq(
  id INTEGER PRIMARY KEY AUTOINCREMENT, source_id TEXT, payload TEXT NOT NULL,
  last_error TEXT, fail_count INTEGER NOT NULL,
  failed_at INTEGER NOT NULL DEFAULT (unixepoch()));

CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);  -- schema_version, data_version
```

**索引纪律**：WeKnora 专门有 migration *删除*低价值索引以提升写入（`migrations/sqlite/000020_chunks_index_diet.up.sql`、`migrations/versioned/000100_chunks_index_diet.up.sql`）。批量导入压测时须复核 `idx_pages_source` 类索引的净收益。

**软删除**：`pages.deleted_at`、`sources` 通过状态表达，避免硬删破坏可重建性。

---

## 6. 数据流

```
导入（廉价路径，零 LLM）
  watcher → jobs 行 → 解析(复用现有 Rust 解析栈: anydoc/pdfium/calamine/docx-rs/epub/mobi)
                    → 分块 → embed → LanceDB
                    → sources/pages/chunks 行 + pages_fts → 立即可检索

合成（唯一花 LLM 的路径，用户显式触发）
  用户点选源 → 复用 ingest.ts LLM 管道 → 写 wiki/*.md → 更新 pages 行

检索
  pages_fts (BM25) ┐
                   ├→ RRF 融合(现有，不改) → 图谱扩展(读 links 表) → 键集分页
  LanceDB ANN      ┘

一致性：dataVersion + outbox（先写 SQLite，再补 LanceDB；jobs 表天然承担 outbox）
```

---

## 7. 分期实施计划

每期独立可交付、独立验证。

### P0 — 索引层（可丢弃派生索引）

| 项 | 内容 | 验收门 |
|---|---|---|
| P0.1 | `index.db` schema + FTS5 + 扩展 `rebuild_wiki_index` 重建索引层 | 10K fixture 可重建；`rm index.db` 后重建，检索结果**逐条一致**；**中文二字词可命中**（如「巡检」「灾备」，防回归 §13.1 的 trigram 陷阱） |
| P0.2 | `search_project` 改查 `pages_fts`；删除 `MAX_SEARCH_FILES` 截断 | **第 10,001 个文件可被搜到**；1K/10K/100K 三档查询延迟呈亚线性 |
| P0.3 | LanceDB `create_index`（ANN）+ 维度分区索引 | 100K chunk 下 P99 达标；recall@10 对比暴力扫描 ≥ 0.95 |

### P1 — 廉价管道与持久化队列

| 项 | 内容 | 验收门 |
|---|---|---|
| P1.1 | 源文章 parse + chunk + embed，**零 LLM** | 10K 源导入 **0 次 LLM 调用**，且全部可检索 |
| P1.2 | 合成改显式触发（复用 `ingest.ts`） | 点选才产出页；未选中的源**字节不变** |
| P1.3 | `jobs` + `job_dlq` 替代 JSON 队列（E10/E12） | 导入中途 kill 进程 → 重启**无丢失、无重复**；DLQ 可查询 |

### P2 — 交互规模与图谱

| 项 | 内容 | 验收门 |
|---|---|---|
| P2.1 | UI 虚拟化 + 键集分页；`countFiles` 移出渲染（E7/E8） | 滚动 10K 行时 DOM 节点数**恒定**（当前会达 5–8 万） |
| P2.2 | 图谱改 `links` 表驱动 + 节点上限（E9） | 建图不再全站读文件；上限生效 |
| P2.3 | 退役 `.llm-wiki/*.json` 热路径（E11/E12） | 无全量重写 JSON 的写路径 |

---

## 8. 迁移与兼容

1. **既有项目**：首次打开时若 `schema_version` 缺失，从 `raw/sources/` + `wiki/` 全量构建 `index.db`；不得阻塞 UI（后台任务 + 进度）。
2. **回填**：`sources` 行由 `raw/sources/` 目录扫描 + `ingest-cache.json` 的 `filesWritten` 推导；`ingest-cache.json` 的**文件名键**需迁移为路径键（E11）。
3. **行为变更**：`autoIngest` 默认值由 `true` 翻转为 `false`（见 §13.2 决议）。
   - 唯一事实来源：`src/lib/source-watch-defaults.json` 的 `autoIngest`（当前为 `true`），经 `src/lib/source-watch-config.ts` 消费。
   - **默认值只影响新项目**：既有项目的已持久化配置不变，因此不会静默改变既有用户行为。
   - 保留键名 `autoIngest` 不改名（避免配置迁移churn）；语义由 UI 文案明确为「导入时自动**合成** wiki 页」，因为廉价索引在新模型下**恒为开启**。
   - UI 文案需更新：`source-watch-section.tsx:211,214` 的 `autoIngest` / `autoIngestDescription`。
4. **回退**：`index.db` 删除后应用仍可运行（检索降级为现有文件遍历路径，保留一个版本周期）。
5. **`rebuild_wiki_index`** 扩展为同时重建 `wiki/index.md` 与新索引层，作为灾备与修复入口。

---

## 9. 验证方法

- **Fixture 生成器**：可生成 1K / 10K / 100K 三档源文章与 wiki 页的测试语料（含中文、英文、混合）。
- **断崖回归**：断言第 10,001 个文件可被关键词检索命中（直接针对 E1）。
- **规模解耦**：断言查询延迟随语料规模呈亚线性，而非线性。
- **可重建性**：`rm index.db` → 重建 → 检索结果集合逐条一致（针对不变量 2）。
- **零 LLM 导入**：mock LLM 端点并断言导入期间调用次数为 0（针对 P1.1）。
- **崩溃恢复**：导入中途 kill 进程，重启后断言无丢失无重复（针对 P1.3）。
- **ANN 质量**：recall@10 对比暴力扫描基线。
- **DOM 稳定性**：滚动 10K 行时断言行数恒定。

---

## 10. 风险与未知

| 风险 | 级别 | 说明 / 缓解 |
|---|---|---|
| ~~中文分词~~ | ~~高~~ → **已解决，残留低** | 已实测决议为 CJK 逐字切分 + unicode61 + 短语查询（§13.1）。残留风险：索引体积（逐字 token 较多）与 FTS5 排序质量（无词频加权，BM25 按字统计而非按词）需在 P0.1 用真实中文语料复核。 |
| 两个派生存储一致性 | 中 | `dataVersion` + outbox（jobs 表）；写入顺序先 SQLite 后 LanceDB |
| LanceDB ANN 与过滤条件相互作用 | 中 | WeKnora `migrations/versioned/000095_memory_vector_search.up.sql:13` 明确记录「近似索引反而需事后过滤，不如扫描」。小集合 / 强过滤场景**不要盲目上 ANN**，需分档实测 |
| LanceDB 0.27.2 ANN 索引参数未验证 | 中 | 需实测调参；与 Postgres HNSW 参数不可直接照搬 |
| 100K 的 embedding 成本与耗时 | 中 | 存储与检索不是瓶颈；embedding 吞吐与 API 费用需在 plan 阶段量化 |
| `autoIngest` 默认值变更 | 中 | 用户可见行为变更，需设置迁移与提示 |
| `rusqlite` bundled 构建代价 | 低 | Windows 免外部 SQLite；体积与首次编译时间增加 |

### 未决未知

- LanceDB 原生 FTS（Tantivy）在 0.27.2 的实际可用性与质量（方案 B 的备选前提；`tantivy` 经 `lance-index` 已在依赖树内）。
- FTS5 候选召回率（vs 全量遍历）不足导致新形式漏检——已设为 P0.2 硬验收门。
- 源文章导入后 `sources` 与 `pages` 的**膨胀比**（一源产出多少页）实测值——直接决定 100K 实际页数。
- 100K 规模下 SQLite FTS5（trigram）的索引体积与查询 P99。

---

## 11. 明确不做

| 项 | 理由 |
|---|---|
| asynq + Redis 六 worker 池（`WeKnora internal/types/task.go:17-25`） | 桌面端无 Redis；队列用 SQLite `jobs` + tokio 任务 |
| docreader Python gRPC 服务 | 已有原生 Rust 解析栈，退化无收益 |
| 多租户 `tenant_id` 全表冗余 | 单用户场景无意义 |
| Postgres HNSW 参数（`m=16, ef_construction=64`） | LanceDB 索引参数体系不同，需实测 |
| `knowledge_processing_spans` 全量 span 树 | 10K 阶段 `jobs.state` + 单表进度足够；可作为后续参考 |
| 保持 `README_CN.md:208` 的旧表述 | 需随实现同步修正文档（E13） |

---

## 12. 参考

- WeKnora 数据模型：`migrations/sqlite/000000_init.up.sql:90-118`（knowledges）、`:242-274`（chunks）
- WeKnora 队列与 DLQ：`migrations/versioned/000041_task_queue_and_wiki_indexes.up.sql:63-130`
- WeKnora 向量索引：`migrations/versioned/000059_embeddings_hnsw_1024.up.sql:35-45`
- WeKnora 存储抽象（原始字节在磁盘/S3，DB 只存元数据）：`internal/types/interfaces/file.go:11-33`
- WeKnora 规模声明：`CHANGELOG.md` v0.5.2（40k 文档，task queue + DLQ）、v0.6.2（HNSW 1024 维）
- 本仓现状证据：见本文第 2 节

---

## 13. 决议（Decisions）

以下两项曾在设计评审时标记为待定，现已定案。两项均以可执行探针或代码核实，非推测。

### 13.1 中文分词：CJK 逐字切分 + unicode61 + 短语查询

**决议**：不采用 `trigram`。改用「写入与查询两侧对称切分」：CJK 字符逐字以空格分隔，非 CJK 原样保留；查询侧将切分后的串以短语引号包裹。

**证据**（SQLite 3.50.4 实测）：

| 查询词 | 长度 | `trigram` | 逐字切分 + unicode61 |
|---|---|---|---|
| `灾备集群` | 4 | 命中 1 | 命中 1 |
| `灾备集` | 3 | 命中 1 | 命中 1 |
| **`灾备`** | **2** | **0（漏）** | 命中 1 |
| **`巡检`** | **2** | **0（漏）** | 命中 1 |
| `华东2区` | 4 | 命中 1 | 命中 1 |
| `ORION-3310` | 10 | 命中 1 | 命中 1 |
| `disaster` | 8 | 命中 1 | 命中 1 |

**裁决理由**：`trigram` 对 CJK 要求 **≥3 字符**，而二字词恰是中文最常用的检索粒度。纯 trigram 会让「巡检」「灾备」这类词**完全检索不到**，等于把 E1 的静默漏检换个形式重新引入，属不可接受的回归。逐字切分在 1/2/3/4 字词及混合中英场景下全部正确命中；短语查询保证语序敏感（`灾备` 命中、`备灾` 不命中），无误匹配。

**实现要求**：
- **复用现有 `tokenize_query`（`search.rs:995`），不写第二套 CJK 分词器。** 它已经实现「CJK bigram + 单字 + 完整 token」的候选展开，是当前行为的来源。同一仓内并存两套 CJK 分词语义是重大隐患，必须收斶为一处。
- `segment_for_fts(s: &str) -> String`：CJK 字符（`U+3400..=U+9FFF`）后补空格，其他字符原样保留。写入侧用此函数。
- **查询侧**：对 `tokenize_query` 输出的每个 token 分别调用 `segment_for_fts`，各自以双引号包裹为短语，再用 `OR` 连接（如 `"灾 备" OR "巡 检"`）。短语包裹保留语序约束，`OR` 保留了现有「bag-of-tokens」召回语义。
- `body`/`title` 存切分后文本用于匹配；`body_raw` 存原文（`UNINDEXED`）供结果摘要展示，避免向用户回显带空格的分词文本。

**关键架构后果：FTS5 只做候选生成（召回），Rust 保留现有评分器（排序）。**

这是本决议中最重要的部分，直接保障不变量 5（排序语义不变）：

1. FTS5 从 10K/100K 页中**召回候选集**（大幅缩小），取代当前的 `WalkDir` 全量遍历 + 全量 `read_to_string`。
2. 现有的 `score_file`（`search.rs:928`）、短语加成、`RRF` 融合**原封不动**地跑在候选集上。因为候选集很小，它仍是廉价的。
3. **BM25 不参与最终排序**——只用于候选截断时的预排序（若候选数超过阀值）。

**副作用（正向）**：这直接消解了 §10 中「FTS5 按字 BM25 与按词排序不同」的风险——因为我们根本不依赖 BM25 排序。

**残留风险**（见 §10）：逐字 token 使索引体积增大（实测评估）；候选集召回率必须足够高，否则会因「候选里没有」而产生新形式的漏检——**需用「FTS5 候选召回率 vs 全量遍历结果」作为 P0.2 的硬验收门**。

**残留风险**（见 §10）：逐字 token 使索引体积增大；FTS5 的 BM25 按**字**统计，排序不同于按词统计，P0.1 需用真实中文语料复核，必要时在 RRF 融合前做归一化。

### 13.2 `autoIngest` 默认值：翻转为 `false`

**决议**：`src/lib/source-watch-defaults.json` 的 `autoIngest` 由 `true` 改为 `false`。

**已核实的现状**：该键当前为 `true`，经 `src/lib/source-watch-config.ts` 消费（UI 开关在 `src/components/settings/sections/source-watch-section.tsx:204-214`）。注意 `autoIngest` 是**配置开关名**，与函数 `autoIngest()`（`src/lib/ingest.ts:636`）同名但不同物——P1.2 改的是开关语义，不重命名函数。

**裁决理由**：两层内容模型下，廉价索引**恒为开启**（导入即索引、零 LLM），因此该开关不再可能表达「要不要索引」。它现在只该表达「要不要在导入时自动**合成** wiki 页」——而这正是 §7 P1.2 要改为显式触发的部分。默认关闭使 10K 导入的 LLM 调用数为 0（P1.1 验收门），成本可预测。

**兼容性**：默认值只作用于**新项目**；既有项目的已持久化配置不被改写，故不构成对既有用户的静默行为变更。保留键名不改以避配置迁移churn，改由 UI 文案澄清语义（「导入时自动合成 wiki 页」）。

**归属期次**：默认值翻转属 P1.2（与「合成改显式触发」同批交付），P0 不得引入该行为变更。