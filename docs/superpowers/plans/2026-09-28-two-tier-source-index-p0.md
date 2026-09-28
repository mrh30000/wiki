# 两层源文章索引 · P0 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用 SQLite + FTS5 索引层取代检索时的文件系统全量遍历，并拆除 `MAX_SEARCH_FILES = 10_000` 静默漏检断崖。

**Architecture:** 新增 `index_db` 模块（SQLite 连接 + schema + FTS5 候选查询）。索引写入是磁盘的派生：`wiki/*.md` 与 `raw/sources/*` 仍是真相源，`index.db` 可删除后重建。检索路径改为「FTS5 生成候选 → 读候选文件 → **现有 `score_file`/RRF 原样评分**」；BM25 不参与最终排序，因此排序语义不变。

**Tech Stack:** Rust 1.88+、`rusqlite`（`bundled`，自带 FTS5）、现有 `walkdir`、Tauri 2 命令层、`vitest`（前端不变）。

**Spec:** `docs/superpowers/specs/2026-09-28-two-tier-source-index-design.md`

## Global Constraints

- Rust 版本下限 `1.88`（`src-tauri/Cargo.toml` 的 `rust-version`），不得降低。
- 保持 `edition = "2021"`。
- 命令返回类型沿用 `Result<T, String>`，错误信息为面向用户的中文或英文短句，不得 panic。
- 所有 Tauri 命令必须经 `run_guarded_async("<name>", ...)` 包裹（现有约定，见 `search.rs:116`）。
- **磁盘是真相源**：任何索引写入失败不得阻断 Markdown 落盘。
- **不变量 5**：`score_file`（`search.rs:928`）、`tokenize_query`（`search.rs:995`）、RRF 融合常量与算法**不得修改语义**。P0 只替换候选生成层。
- **禁止第二套 CJK 分词器**：必须复用 `tokenize_query`（`search.rs:995`）。
- 索引库路径固定为 `{project_path}/.llm-wiki/index.db`。
- FTS5 表必须使用 `tokenize = 'unicode61'`（**不是** `trigram`，理由见 Spec §13.1）。
- 提交粒度：每个 Task 一次提交，提交信息用 Conventional Commits。

## Review Focus

以下五类输入/失败模式是 Spec 隐含、但任何单个 Task 的测试都可能漏掉、且最可能伤到真实用户的。**每一行都必须在对应 Task 中落成一条测试**：

1. **中文二字词查询**（如「巡检」「灾备」）——用户最常用的中文检索粒度；`trigram` 会让它们返回 0 结果，是本次改造最易引入的回归。归属 Task 2、Task 5。
2. **`index.db` 缺失、损坏或 schema 版本落后**——用户删库、半写、或从旧版本升级后首次检索；期望：不崩溃、不静默返回空，而是自动重建或降级到文件遍历。归属 Task 1、Task 7。
3. **FTS5 特殊语法字符**（`"`、`*`、`NEAR`、`AND`、`OR`、`(`、`:`）出现在用户查询里——期望：当作字面量处理，不得解析为 FTS5 语法或报错。归属 Task 2。
4. **并发写入下的索引一致性**——用户导入时同时检索；期望：读到的是旧快照而非半写状态，不报 `database is locked` 给用户。归属 Task 3。
5. **候选召回不足导致的新形式漏检**——FTS5 候选集里没有某页，但全量遍历会命中它；期望：候选召回率达标（Spec 要求设为 P0.2 硬门）。归属 Task 5、Task 6。

---

## File Structure

| 文件 | 职责 |
|---|---|
| `src-tauri/src/commands/index_db.rs`（新建） | SQLite 连接、schema、FTS5 候选查询、`links` 读取。**不含**检索评分逻辑。 |
| `src-tauri/src/commands/index_build.rs`（新建） | 从磁盘构建索引：遍历 `wiki/`+`raw/sources/` → 写 `pages`/`sources`/`chunks`/`links`/`pages_fts`。**不含** SQL 细节（走 `index_db` 的接口）。 |
| `src-tauri/src/commands/segment.rs`（新建） | `segment_for_fts` 与 `build_fts_match_query`——纯函数，零依赖，最易测。 |
| `src-tauri/src/commands/search.rs`（修改） | 候选生成改用 `index_db`；删除 `MAX_SEARCH_FILES` 截断。评分逻辑不动。 |
| `src-tauri/src/commands/project_maintenance.rs`（修改） | `rebuild_wiki_index` 扩展为同时重建索引层。 |
| `src-tauri/src/commands/mod.rs`（修改） | 注册三个新模块。 |
| `src-tauri/src/lib.rs`（修改） | 注册 `rebuild_index_db` 命令。 |
| `README_CN.md`（修改） | 修正 E13 文档与代码不一致。 |

**拆分理由**：`segment.rs` 是纯函数（无 IO），`index_db.rs` 是存储访问，`index_build.rs` 是编排。三者可独立测试，且 `search.rs`（已 2453 行）不再继续膨胀。

---

### Task 1: rusqlite 依赖 + FTS5 可用性探针 + schema

先证明 FTS5 真的可用，再建 schema。若 `bundled` 未启用 FTS5，本 Task 立刻失败并暴露——不要假设。

**Files:**
- Create: `src-tauri/src/commands/index_db.rs`
- Modify: `src-tauri/src/commands/mod.rs`
- Modify: `src-tauri/Cargo.toml`

**Interfaces:**
- Consumes: 无（首个 Task）
- Produces:
  - `pub struct IndexDb { conn: rusqlite::Connection }`
  - `pub const INDEX_SCHEMA_VERSION: i64 = 1;`
  - `pub fn index_db_path(project_path: &str) -> std::path::PathBuf`
  - `impl IndexDb { pub fn open(project_path: &str) -> Result<IndexDb, String>; pub fn open_in_memory() -> Result<IndexDb, String>; pub fn ensure_schema(&self) -> Result<(), String>; pub fn schema_version(&self) -> Result<i64, String>; }`

- [x] **Step 1: 添加依赖**

```bash
cd src-tauri && cargo add rusqlite --features bundled
```

Expected: `Cargo.toml` 的 `[dependencies]` 出现 `rusqlite = { version = "...", features = ["bundled"] }`。

- [x] **Step 2: 注册模块**

在 `src-tauri/src/commands/mod.rs` 按字母序插入（现有 `pub mod fs;` 与 `pub mod page_embedding;` 之间）：

```rust
pub mod index_db;
```

- [x] **Step 3: 写失败测试（FTS5 可用性 + schema 幂等）**

创建 `src-tauri/src/commands/index_db.rs`，**仅**写入测试与最小骨架：

```rust
use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// Bump when the schema below changes in a way that requires a rebuild.
pub const INDEX_SCHEMA_VERSION: i64 = 1;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta(
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS pages(
  id          TEXT PRIMARY KEY,
  path        TEXT UNIQUE NOT NULL,
  title       TEXT,
  type        TEXT,
  hash        TEXT,
  mtime       INTEGER,
  frontmatter TEXT,
  source_id   TEXT,
  deleted_at  INTEGER
);
CREATE INDEX IF NOT EXISTS idx_pages_source ON pages(source_id);
CREATE INDEX IF NOT EXISTS idx_pages_type   ON pages(type);

CREATE TABLE IF NOT EXISTS sources(
  id           TEXT PRIMARY KEY,
  path         TEXT UNIQUE NOT NULL,
  hash         TEXT,
  size         INTEGER,
  mtime        INTEGER,
  parse_status TEXT NOT NULL DEFAULT 'pending',
  error        TEXT,
  created_at   INTEGER,
  processed_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_sources_status ON sources(parse_status);

CREATE TABLE IF NOT EXISTS chunks(
  id            TEXT PRIMARY KEY,
  page_id       TEXT NOT NULL,
  chunk_index   INTEGER NOT NULL,
  heading_path  TEXT,
  text          TEXT NOT NULL,
  content_hash  TEXT,
  parent_chunk_id TEXT,
  chunk_type    TEXT NOT NULL DEFAULT 'text'
);
CREATE INDEX IF NOT EXISTS idx_chunks_page ON chunks(page_id, chunk_index);

CREATE TABLE IF NOT EXISTS links(
  src_page_id TEXT NOT NULL,
  dst_page_id TEXT,
  dst_raw     TEXT,
  weight      REAL NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_links_src ON links(src_page_id);
CREATE INDEX IF NOT EXISTS idx_links_dst ON links(dst_page_id);

-- body/title hold the SEGMENTED text for matching; body_raw holds the original
-- for display. tokenize MUST be unicode61 (see Spec 13.1).
CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
  title, body, heading_path, page_id UNINDEXED, body_raw UNINDEXED,
  tokenize = 'unicode61'
);
"#;

pub struct IndexDb {
    conn: Connection,
}

pub fn index_db_path(project_path: &str) -> PathBuf {
    Path::new(project_path).join(".llm-wiki").join("index.db")
}

impl IndexDb {
    pub fn open(project_path: &str) -> Result<IndexDb, String> {
        let path = index_db_path(project_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("无法创建索引目录 {}: {e}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .map_err(|e| format!("无法打开索引库 {}: {e}", path.display()))?;
        let db = IndexDb { conn };
        db.apply_pragmas()?;
        db.ensure_schema()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<IndexDb, String> {
        let conn = Connection::open_in_memory()
            .map_err(|e| format!("无法创建内存索引库: {e}"))?;
        let db = IndexDb { conn };
        db.ensure_schema()?;
        Ok(db)
    }

    fn apply_pragmas(&self) -> Result<(), String> {
        // WAL: readers never block the indexer (Review Focus #4).
        self.conn
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| format!("设置 journal_mode 失败: {e}"))?;
        self.conn
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| format!("设置 synchronous 失败: {e}"))?;
        self.conn
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| format!("设置 busy_timeout 失败: {e}"))?;
        Ok(())
    }

    pub fn ensure_schema(&self) -> Result<(), String> {
        self.conn
            .execute_batch(SCHEMA_SQL)
            .map_err(|e| format!("创建索引 schema 失败: {e}"))?;
        self.conn
            .execute(
                "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
                 ON CONFLICT(key) DO NOTHING",
                [INDEX_SCHEMA_VERSION.to_string()],
            )
            .map_err(|e| format!("写入 schema_version 失败: {e}"))?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<i64, String> {
        let value: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |row| {
                row.get(0)
            })
            .ok();
        Ok(value.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    /// Stamp the schema version. Called after a successful rebuild so the
    /// stale-version flag actually clears; `ensure_schema` deliberately does
    /// NOT overwrite an existing version.
    pub(crate) fn set_schema_version(&self, version: i64) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO meta(key, value) VALUES('schema_version', ?1)\n                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [version.to_string()],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts5_is_available_in_bundled_sqlite() {
        let db = IndexDb::open_in_memory().expect("schema should apply");
        db.conn
            .execute(
                "INSERT INTO pages_fts(title, body, heading_path, page_id, body_raw)
                 VALUES('t', '灾 备 集 群', '', 'p1', '灾备集群')",
                [],
            )
            .expect("FTS5 insert should work");
        let hits: i64 = db
            .conn
            .query_row(
                "SELECT count(*) FROM pages_fts WHERE pages_fts MATCH '\"灾 备\"'",
                [],
                |row| row.get(0),
            )
            .expect("FTS5 MATCH should work");
        assert_eq!(hits, 1);
    }

    #[test]
    fn schema_creation_is_idempotent_and_reports_version() {
        let db = IndexDb::open_in_memory().unwrap();
        db.ensure_schema().expect("second ensure_schema must not fail");
        assert_eq!(db.schema_version().unwrap(), INDEX_SCHEMA_VERSION);
    }
}
```

- [x] **Step 4: 运行测试，确认 FTS5 可用**

Run: `cd src-tauri && cargo test --lib index_db -- --nocapture`

Expected: 两个测试 **PASS**。

若 `fts5_is_available_in_bundled_sqlite` 失败并报 `no such module: fts5`，说明 `bundled` 未启用 FTS5，改为：

```bash
cd src-tauri && cargo add rusqlite --features bundled-full
```

然后重跑本步。**不得**在没有通过此测试的情况下继续。

- [x] **Step 5: 提交**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/commands/mod.rs src-tauri/src/commands/index_db.rs
git commit -m "feat(index): add SQLite index store with FTS5 schema

Introduces .llm-wiki/index.db as a discardable derivative of the
plaintext wiki. Uses unicode61 (not trigram) per spec 13.1: trigram
cannot match 2-character CJK words such as 巡检/灾备.

WAL + busy_timeout so reads never surface database-is-locked during
concurrent ingest."
```

---

### Task 2: CJK 切分与 FTS5 查询构造（纯函数）

本 Task 是 Spec §13.1 决议的落点，也是 Review Focus #1 与 #3 的防线。

**Files:**
- Create: `src-tauri/src/commands/segment.rs`
- Modify: `src-tauri/src/commands/mod.rs`

**Interfaces:**
- Consumes: `crate::commands::search::tokenize_query`（已存在，`search.rs:995`）
- Produces:
  - `pub fn segment_for_fts(input: &str) -> String`
  - `pub fn build_fts_match_query(tokens: &[String]) -> String`
  - `fn fts_escape_phrase(s: &str) -> String`（私有）

- [x] **Step 1: 注册模块**

在 `src-tauri/src/commands/mod.rs` 按字母序插入：

```rust
pub mod segment;
```

- [x] **Step 2: 写失败测试**

创建 `src-tauri/src/commands/segment.rs`：

```rust
use crate::commands::search::tokenize_query;

/// Insert a space after every CJK ideograph so unicode61 emits one token per
/// character. Non-CJK characters pass through untouched.
///
/// The write side and the query side MUST both go through this function;
/// asymmetric use fails silently (no match, no error).
pub fn segment_for_fts(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + input.len() / 2);
    for ch in input.chars() {
        if ('\u{3400}'..='\u{9fff}').contains(&ch) {
            out.push(ch);
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out.trim_end().to_string()
}

/// Escape a string for use inside an FTS5 double-quoted phrase.
/// FTS5 escapes a literal `"` by doubling it.
///
/// Private on purpose: nothing outside this module needs it, and a second
/// escape implementation elsewhere is how FTS5 injection creeps back in.
fn fts_escape_phrase(s: &str) -> String {
    s.replace('"', "\"\"")
}

/// Build an FTS5 MATCH expression from already-tokenized query terms.
///
/// Each term becomes its own quoted phrase so that ordering inside a term is
/// preserved (灾备 matches, 备灾 does not), and terms are joined with OR to
/// keep the existing bag-of-tokens recall semantics of `score_file`.
pub fn build_fts_match_query(tokens: &[String]) -> String {
    let parts: Vec<String> = tokens
        .iter()
        .map(|t| segment_for_fts(t))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| format!("\"{}\"", fts_escape_phrase(&s)))
        .collect();
    parts.join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_cjk_per_character_and_preserves_ascii() {
        assert_eq!(segment_for_fts("灾备集群"), "灾 备 集 群");
        assert_eq!(segment_for_fts("ORION-3310"), "ORION-3310");
        assert_eq!(segment_for_fts("华东2区"), "华 东 2区");
        assert_eq!(segment_for_fts("RPO 15 分钟"), "RPO 15 分 钟");
    }

    #[test]
    fn round_trip_is_symmetric_for_write_and_query() {
        let raw = "灾备集群部署在华东2区";
        let indexed = segment_for_fts(raw);
        // A 2-character CJK query must survive the round trip (Review Focus #1).
        let q = build_fts_match_query(&["巡检".to_string(), "灾备".to_string()]);
        assert_eq!(q, "\"巡 检\" OR \"灾 备\"");
        assert!(indexed.contains("灾 备"));
    }

    #[test]
    fn fts_syntax_characters_are_treated_as_literals() {
        // Review Focus #3: user typing FTS5 syntax must not break or inject.
        for raw in ["NEAR", "AND", "OR", "a*b", "(x)", "col:val", "he said \"hi\""] {
            let q = build_fts_match_query(&[raw.to_string()]);
            // Every term is wrapped in quotes, so no bare operator survives.
            assert!(q.starts_with('"') && q.ends_with('"'), "unquoted: {q}");
            assert_eq!(q.matches('"').count() % 2, 0, "unbalanced quotes: {q}");
        }

        // The two structural assertions above CANNOT detect a broken escaper:
        // an unescaped `he said "hi"` yields `"he said "hi""` — 4 quotes
        // (even) and still starts/ends with a quote. So pin the exact escaped
        // output too, or removing the escaping would pass this test silently.
        assert_eq!(
            build_fts_match_query(&["he said \"hi\"".to_string()]),
            "\"he said \"\"hi\"\"\""
        );
        assert_eq!(build_fts_match_query(&["\"".to_string()]), "\"\"\"\"");
    }

    #[test]
    fn empty_and_whitespace_tokens_are_dropped() {
        assert_eq!(build_fts_match_query(&[]), "");
        assert_eq!(build_fts_match_query(&["  ".to_string()]), "");
    }

    #[test]
    fn reuses_existing_query_tokenizer_for_cjk_bigrams() {
        // Guards Global Constraint: no second CJK tokenizer.
        let tokens = tokenize_query("默会知识");
        assert!(tokens.contains(&"默会".to_string()));
        let q = build_fts_match_query(&tokens);
        assert!(q.contains("\"默 会\""));
    }
}
```

- [x] **Step 3: 运行测试，确认失败**

Run: `cd src-tauri && cargo test --lib segment`

Expected: 编译失败，因为 `search::tokenize_query` 可能需要提升可见性（见 Step 4），或测试断言失败。记录实际错误。

- [x] **Step 4: 满足编译需求（最小实现）**

若报 `function tokenize_query is private`，在 `search.rs:995` 将其签名改为：

```rust
pub fn tokenize_query(query: &str) -> Vec<String> {
```

**只改可见性，不改函数体。** 若报 `is_query_separator`/`is_stop_word` 私有，同样只加 `pub`。

- [x] **Step 5: 运行测试，确认通过**

Run: `cd src-tauri && cargo test --lib segment`

Expected: 5 个测试全部 **PASS**。

- [x] **Step 6: 提交**

```bash
git add src-tauri/src/commands/mod.rs src-tauri/src/commands/segment.rs src-tauri/src/commands/search.rs
git commit -m "feat(index): CJK per-character segmenter and FTS5 query builder

Reuses the existing tokenize_query for CJK bigram expansion instead of
introducing a second tokenizer. Every query term becomes a quoted FTS5
phrase joined by OR, so ordering is preserved and FTS5 syntax
characters in user input stay literal."
```

---

### Task 3: 从磁盘构建索引

**Files:**
- Create: `src-tauri/src/commands/index_build.rs`
- Modify: `src-tauri/src/commands/mod.rs`
- Modify: `src-tauri/src/commands/index_db.rs`（新增写入方法）

**Interfaces:**
- Consumes: `IndexDb`（Task 1）、`segment_for_fts`（Task 2）、`search::extract_graph_links`（已存在）
- Produces:
  - `pub struct BuildStats { pub pages: usize, pub links: usize, pub skipped: usize }`
  - `pub fn build_index(db: &mut IndexDb, project_path: &str) -> Result<BuildStats, String>`
  - `impl IndexDb { pub fn replace_all(&mut self, pages: &[IndexPage]) -> Result<(), String>; }`
  - `pub struct IndexPage { pub path: String, pub title: String, pub page_type: String, pub hash: String, pub mtime: i64, pub body_raw: String, pub links: Vec<String> }`

- [x] **Step 1: 在 `index_db.rs` 增加写入接口**

追加到 `impl IndexDb`（`SCHEMA_SQL` 之后定义 `IndexPage`）：

```rust
pub struct IndexPage {
    pub path: String,
    pub title: String,
    pub page_type: String,
    pub hash: String,
    pub mtime: i64,
    pub body_raw: String,
    pub links: Vec<String>,
}

impl IndexDb {
    /// Replace the entire index in one transaction. Callers must have already
    /// written the Markdown to disk — the index is a derivative.
    pub fn replace_all(&mut self, pages: &[IndexPage]) -> Result<(), String> {
        let tx = self
            .conn
            .transaction()
            .map_err(|e| format!("无法开启索引事务: {e}"))?;
        tx.execute("DELETE FROM pages_fts", [])
            .map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM pages", []).map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM links", []).map_err(|e| e.to_string())?;
        for page in pages {
            tx.execute(
                "INSERT INTO pages(id, path, title, type, hash, mtime, frontmatter)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                rusqlite::params![
                    page.path,
                    page.path,
                    page.title,
                    page.page_type,
                    page.hash,
                    page.mtime
                ],
            )
            .map_err(|e| format!("写入 page 失败 ({}): {e}", page.path))?;
            let segmented_title = crate::commands::segment::segment_for_fts(&page.title);
            let segmented_body = crate::commands::segment::segment_for_fts(&page.body_raw);
            tx.execute(
                "INSERT INTO pages_fts(title, body, heading_path, page_id, body_raw)
                 VALUES(?1, ?2, '', ?3, ?4)",
                rusqlite::params![
                    segmented_title,
                    segmented_body,
                    page.path,
                    page.body_raw
                ],
            )
            .map_err(|e| format!("写入 FTS 失败 ({}): {e}", page.path))?;
            for dst in &page.links {
                tx.execute(
                    "INSERT INTO links(src_page_id, dst_page_id, dst_raw, weight)
                     VALUES(?1, NULL, ?2, 1.0)",
                    rusqlite::params![page.path, dst],
                )
                .map_err(|e| format!("写入 link 失败: {e}"))?;
            }
        }
        tx.commit().map_err(|e| format!("提交索引事务失败: {e}"))?;
        Ok(())
    }

    /// FTS5 candidate generation. Returns project-relative page paths.
    pub fn search_candidates(&self, match_query: &str, limit: usize) -> Result<Vec<String>, String> {
        if match_query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT page_id FROM pages_fts
                 WHERE pages_fts MATCH ?1
                 ORDER BY bm25(pages_fts)
                 LIMIT ?2",
            )
            .map_err(|e| format!("准备候选查询失败: {e}"))?;
        let rows = stmt
            .query_map(rusqlite::params![match_query, limit as i64], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| format!("执行候选查询失败: {e}"))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| format!("读取候选失败: {e}"))?);
        }
        Ok(out)
    }
}
```

- [x] **Step 2: 写失败测试**

创建 `src-tauri/src/commands/index_build.rs`：

```rust
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::commands::index_db::{IndexDb, IndexPage};

pub struct BuildStats {
    pub pages: usize,
    pub links: usize,
    pub skipped: usize,
}

fn content_hash(content: &str) -> String {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn mtime_secs(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Collect every Markdown page under wiki/, relative to the project root.
pub fn collect_pages(project_path: &str) -> Result<Vec<IndexPage>, String> {
    let wiki = Path::new(project_path).join("wiki");
    if !wiki.exists() {
        return Ok(Vec::new());
    }
    let mut pages = Vec::new();
    for entry in WalkDir::new(&wiki).follow_links(false) {
        let entry = entry.map_err(|e| format!("枚举 wiki 页面失败: {e}"))?;
        if !entry.file_type().is_file() {
            continue;
        }
        if !entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        let Ok(body_raw) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let relative = entry
            .path()
            .strip_prefix(Path::new(project_path))
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let title = crate::commands::search::extract_title(
            &body_raw,
            entry.path().file_stem().and_then(|s| s.to_str()).unwrap_or_default(),
        );
        // Reuse the one frontmatter parser the existing index rebuilder uses;
        // a second YAML-ish parser in this file would drift from it.
        let page_type = crate::commands::project_maintenance::frontmatter_value(&body_raw, "type")
            .unwrap_or_else(|| "other".to_string());
        pages.push(IndexPage {
            path: relative,
            title,
            page_type,
            hash: content_hash(&body_raw),
            mtime: mtime_secs(entry.path()),
            links: crate::commands::search::extract_graph_links(&body_raw),
            body_raw,
        });
    }
    Ok(pages)
}

pub fn build_index(db: &mut IndexDb, project_path: &str) -> Result<BuildStats, String> {
    let pages = collect_pages(project_path)?;
    let link_count = pages.iter().map(|p| p.links.len()).sum();
    let total = pages.len();
    db.replace_all(&pages)?;
    Ok(BuildStats {
        pages: total,
        links: link_count,
        skipped: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_project() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("llm-wiki-indexbuild-test-{id}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("wiki/concepts")).unwrap();
        path
    }

    fn write_page(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn builds_pages_and_links_from_disk() {
        let root = tmp_project();
        write_page(
            &root,
            "wiki/concepts/orion.md",
            "---\ntitle: ORION-3310\ntype: concept\n---\n\n灾备集群见 [[wiki/concepts/dr|灾备]]。",
        );
        write_page(&root, "wiki/concepts/dr.md", "# 灾备\n\nRPO 15 分钟。");

        let mut db = IndexDb::open_in_memory().unwrap();
        let stats = build_index(&mut db, root.to_string_lossy().as_ref()).unwrap();

        assert_eq!(stats.pages, 2);
        assert!(stats.links >= 1);
        let hits = db
            .search_candidates(&crate::commands::segment::build_fts_match_query(&[
                "灾备".to_string()
            ]), 50)
            .unwrap();
        assert_eq!(hits.len(), 2, "both pages mention 灾备");
    }

    #[test]
    fn rebuild_is_idempotent_and_drops_deleted_pages() {
        let root = tmp_project();
        write_page(&root, "wiki/a.md", "# A\n\n灾备");
        write_page(&root, "wiki/b.md", "# B\n\n巡检");

        let mut db = IndexDb::open_in_memory().unwrap();
        build_index(&mut db, root.to_string_lossy().as_ref()).unwrap();
        assert_eq!(db.page_count().unwrap(), 2);

        fs::remove_file(root.join("wiki/b.md")).unwrap();
        build_index(&mut db, root.to_string_lossy().as_ref()).unwrap();

        assert_eq!(db.page_count().unwrap(), 1, "stale rows must be gone");
        let stale = db
            .search_candidates(&crate::commands::segment::build_fts_match_query(&[
                "巡检".to_string()
            ]), 50)
            .unwrap();
        assert!(stale.is_empty(), "deleted page must not be searchable");
    }

    #[test]
    fn missing_wiki_dir_is_not_an_error() {
        let root = tmp_project();
        fs::remove_dir_all(root.join("wiki")).unwrap();
        let mut db = IndexDb::open_in_memory().unwrap();
        let stats = build_index(&mut db, root.to_string_lossy().as_ref()).unwrap();
        assert_eq!(stats.pages, 0);
    }
}
```

- [x] **Step 3: 补齐 `index_db.rs` 中测试用到的 `page_count`**

追加到 `impl IndexDb`：

```rust
    pub fn page_count(&self) -> Result<usize, String> {
        let n: i64 = self
            .conn
            .query_row("SELECT count(*) FROM pages", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        Ok(n as usize)
    }
```

- [x] **Step 4: 运行测试，确认失败**

Run: `cd src-tauri && cargo test --lib index_build`

Expected: 编译失败（`extract_page_type` 不存在，或 `extract_title`/`extract_graph_links` 私有）。

- [x] **Step 5: 满足编译需求**

- `extract_title`：已是 `pub`，无需改动。
- `extract_graph_links`：已是 `pub(crate)`（`search.rs:707`），`index_build` 同 crate 可直接调用，**无需改动**。
- `extract_page_type` **不存在，也不要新建**。直接调用 `project_maintenance::frontmatter_value`（Task 3 的代码已这样写）。它是私有函数，只加 `pub(crate)`：

```rust
pub(crate) fn frontmatter_value(content: &str, key: &str) -> Option<String> {
```

  **只改可见性，不改函数体**——它与 `rebuild_wiki_index_inner` 必须对 frontmatter 保持同一解析语义。

- [x] **Step 6: 运行测试，确认通过**

Run: `cd src-tauri && cargo test --lib index_build`

Expected: 3 个测试 **PASS**。

- [x] **Step 7: 提交**

```bash
git add src-tauri/src/commands/mod.rs src-tauri/src/commands/index_db.rs src-tauri/src/commands/index_build.rs src-tauri/src/commands/search.rs
git commit -m "feat(index): build the index from disk in one transaction

Full replace inside a single transaction keeps readers on a consistent
snapshot (WAL). Rebuild drops rows for pages that no longer exist, so
deleting a Markdown file removes it from search."
```

---

### Task 4: 重建命令（灾备入口）

**Files:**
- Modify: `src-tauri/src/commands/project_maintenance.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `.gitignore`

**Interfaces:**
- Consumes: `IndexDb`（Task 1）、`build_index`（Task 3）
- Produces:
  - `pub struct RebuildIndexDbResult { pub pages: usize, pub links: usize }`
  - `#[tauri::command] pub async fn rebuild_index_db(project_path: String) -> Result<RebuildIndexDbResult, String>`
  - `fn rebuild_index_db_inner(project_path: String) -> Result<RebuildIndexDbResult, String>`

- [x] **Step 0: 把索引库加入 `.gitignore`**

本 Task 是第一个真正写出 `.llm-wiki/index.db` 的地方。该文件是派生状态且为 WAL 模式（含 `-wal`/`-shm` 旁文件），**不得进入版本控制**。当前 `.gitignore` 未包含它。

在 `.gitignore` 的 `# Benchmark and test data (local only)` 段附近追加：

```gitignore
# Derived search index (rebuilt from wiki/*.md; see .llm-wiki/index.db)
.llm-wiki/
```

验证：

```bash
git check-ignore -v .llm-wiki/index.db
```

Expected: 输出一行匹配 `.llm-wiki/` 的规则（而非无输出）。

- [x] **Step 1: 写失败测试**

在 `project_maintenance.rs` 的 `#[cfg(test)] mod tests` 中追加：

```rust
    #[test]
    fn rebuild_index_db_recreates_searchable_index_from_disk() {
        let root = temp("rebuild-index-db");
        let wiki = root.join("wiki");
        fs::create_dir_all(&wiki).unwrap();
        fs::write(wiki.join("a.md"), "# A\n\n灾备集群").unwrap();

        let result = rebuild_index_db_inner(root.to_string_lossy().into_owned()).unwrap();
        assert_eq!(result.pages, 1);

        // Deleting the DB and rebuilding must yield the same searchable state
        // (spec invariant 2).
        let db_path = crate::commands::index_db::index_db_path(root.to_string_lossy().as_ref());
        fs::remove_file(&db_path).unwrap();
        let again = rebuild_index_db_inner(root.to_string_lossy().into_owned()).unwrap();
        assert_eq!(again.pages, 1);

        let db = crate::commands::index_db::IndexDb::open(root.to_string_lossy().as_ref()).unwrap();
        let hits = db
            .search_candidates(&crate::commands::segment::build_fts_match_query(&[
                "灾备".to_string()
            ]), 50)
            .unwrap();
        assert_eq!(hits.len(), 1);
    }
```

- [x] **Step 2: 运行测试，确认失败**

Run: `cd src-tauri && cargo test --lib rebuild_index_db`

Expected: 编译失败，`rebuild_index_db_inner` 未定义。

- [x] **Step 3: 实现**

追加到 `project_maintenance.rs`：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RebuildIndexDbResult {
    pub pages: usize,
    pub links: usize,
}

/// Rebuild the derived search index from the plaintext wiki.
///
/// Safe to run at any time: the index is a discardable derivative, so this is
/// both the recovery path (corrupt/missing index.db) and the migration path
/// (schema version bump).
#[tauri::command]
pub async fn rebuild_index_db(project_path: String) -> Result<RebuildIndexDbResult, String> {
    tauri::async_runtime::spawn_blocking(move || rebuild_index_db_inner(project_path))
        .await
        .map_err(|error| format!("Index rebuild task failed: {error}"))?
}

fn rebuild_index_db_inner(project_path: String) -> Result<RebuildIndexDbResult, String> {
    let mut db = crate::commands::index_db::IndexDb::open(&project_path)?;
    let stats = crate::commands::index_build::build_index(&mut db, &project_path)?;
    // Stamp the current version AFTER the rebuild. `ensure_schema` uses
    // ON CONFLICT DO NOTHING, so without this the stale flag would survive the
    // rebuild and the self-heal check would re-trigger on every search.
    db.set_schema_version(crate::commands::index_db::INDEX_SCHEMA_VERSION)?;
    Ok(RebuildIndexDbResult {
        pages: stats.pages,
        links: stats.links,
    })
}
```

在 `src-tauri/src/lib.rs` 的 `invoke_handler` 中，紧随 `commands::project_maintenance::rebuild_wiki_index` 注册：

```rust
            commands::project_maintenance::rebuild_index_db,
```

- [x] **Step 4: 运行测试，确认通过**

Run: `cd src-tauri && cargo test --lib rebuild_index_db`

Expected: **PASS**。

- [x] **Step 5: 全量回归**

Run: `cd src-tauri && cargo test --lib`

Expected: 全部通过（含既有的 `rebuilds_index_from_page_frontmatter`）。

- [x] **Step 6: 提交**

```bash
git add src-tauri/src/commands/project_maintenance.rs src-tauri/src/lib.rs .gitignore
git commit -m "feat(index): add rebuild_index_db command as recovery path

Deleting index.db and rebuilding yields identical searchable state,
which is the spec's discardability invariant and the schema-migration
mechanism. The DB is derived state and WAL-mode, so it is gitignored
rather than committed."
```

---

### Task 5: 检索改用 FTS5 候选生成（保留现有评分器）

本 Task 是 Review Focus #5（召回不足）的防线，也是最需要谨慎的一步。

**Files:**
- Modify: `src-tauri/src/commands/search.rs:327-460`（`search_project_inner` 的候选生成段）

**Interfaces:**
- Consumes: `IndexDb::search_candidates`（Task 1）、`build_fts_match_query`（Task 2）
- Produces: 无新公开接口；`search_project_inner` 的签名与返回类型**不变**。

- [x] **Step 1: 写失败测试（断崖 + 召回）**

在 `search.rs` 的 `#[cfg(test)] mod tests` 追加（该模块**已有** `tmp_project()` 与 `write_page()` 辅助函数，直接用；`search.rs` 的既有测试已用 `#[tokio::test]`，保持一致）。注意 `search_project_inner` 是 async，必须用 `#[tokio::test]` + `.await`，**不要**用 `block_on`：

```rust
    fn index_fixture(root: &Path, pages: usize) {
        for i in 0..pages {
            let dir = root.join("wiki/bulk");
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(format!("p{i}.md")),
                format!("# Page {i}\n\ncommon filler text 灾备集群 number {i}\n"),
            )
            .unwrap();
        }
    }

    #[tokio::test]
    async fn searches_beyond_ten_thousand_pages() {
        let root = tmp_project();
        index_fixture(&root, 10_050);
        // The needle sits past the old MAX_SEARCH_FILES boundary.
        fs::write(
            root.join("wiki/bulk/zz_needle.md"),
            "# Needle\n\n唯一标记词 逆向工程\n",
        )
        .unwrap();

        let mut db = crate::commands::index_db::IndexDb::open(root.to_string_lossy().as_ref())
            .unwrap();
        crate::commands::index_build::build_index(&mut db, root.to_string_lossy().as_ref())
            .unwrap();
        drop(db);

        let resp = search_project_inner(
            root.to_string_lossy().into_owned(),
            "逆向工程".to_string(),
            20,
            false,
            None,
        )
        .await
        .unwrap();

        assert!(
            resp.results.iter().any(|r| r.path.ends_with("zz_needle.md")),
            "page beyond 10k must be findable; got {} results",
            resp.results.len()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn two_character_cjk_query_matches() {
        // Review Focus #1.
        let root = tmp_project();
        write_page(&root, "wiki/concepts/dr.md", "# 灾备\n\n巡检流程每 3n 秒采样。");
        let mut db = crate::commands::index_db::IndexDb::open(root.to_string_lossy().as_ref())
            .unwrap();
        crate::commands::index_build::build_index(&mut db, root.to_string_lossy().as_ref())
            .unwrap();
        drop(db);

        for q in ["巡检", "灾备"] {
            let resp = search_project_inner(
                root.to_string_lossy().into_owned(),
                q.to_string(),
                20,
                false,
                None,
            )
            .await
            .unwrap();
            assert!(!resp.results.is_empty(), "query {q} must match");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn candidate_recall_matches_full_scan() {
        // Review Focus #5: the candidate set must contain every page the old
        // full scan would have scored as a hit.
        let root = tmp_project();
        for i in 0..200 {
            write_page(
                &root,
                &format!("wiki/recall/p{i}.md"),
                &format!("# P{i}\n\nalpha beta 灾备 cluster {i}\n"),
            );
        }
        let mut db = crate::commands::index_db::IndexDb::open(root.to_string_lossy().as_ref())
            .unwrap();
        crate::commands::index_build::build_index(&mut db, root.to_string_lossy().as_ref())
            .unwrap();

        let tokens = tokenize_query("灾备 cluster");
        let match_query = crate::commands::segment::build_fts_match_query(&tokens);
        let candidates = db.search_candidates(&match_query, 1_000).unwrap();
        assert_eq!(
            candidates.len(),
            200,
            "every matching page must be a candidate"
        );
    }
    #[tokio::test]
    async fn missing_index_is_rebuilt_not_fatal() {
        // Review Focus #2.
        let root = tmp_project();
        write_page(&root, "wiki/a.md", "# A\n\n灾备集群");
        let db_path = crate::commands::index_db::index_db_path(root.to_string_lossy().as_ref());
        let _ = fs::remove_file(&db_path);

        let resp = search_project_inner(
            root.to_string_lossy().into_owned(),
            "灾备".to_string(),
            20,
            false,
            None,
        )
        .await
        .unwrap();
        assert!(!resp.results.is_empty(), "missing index must self-heal");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stale_schema_version_triggers_rebuild() {
        // Review Focus #2: an index written by an older app version.
        // Build a POPULATED index first, then mark it stale — otherwise this
        // test would pass via the empty-index path and never exercise the
        // schema check at all.
        let root = tmp_project();
        write_page(&root, "wiki/a.md", "# A\n\n灾备集群");
        {
            let mut db =
                crate::commands::index_db::IndexDb::open(root.to_string_lossy().as_ref())
                    .unwrap();
            crate::commands::index_build::build_index(
                &mut db,
                root.to_string_lossy().as_ref(),
            )
            .unwrap();
            db.set_schema_version(0).unwrap();
            assert_eq!(db.page_count().unwrap(), 1, "fixture must be populated");
        }
        let resp = search_project_inner(
            root.to_string_lossy().into_owned(),
            "灾备".to_string(),
            20,
            false,
            None,
        )
        .await
        .unwrap();
        assert!(!resp.results.is_empty(), "stale schema must self-heal");
        // The rebuild must have cleared the stale flag, or every later search
        // rebuilds again.
        let after = crate::commands::index_db::IndexDb::open(root.to_string_lossy().as_ref())
            .unwrap();
        assert_eq!(
            after.schema_version().unwrap(),
            crate::commands::index_db::INDEX_SCHEMA_VERSION,
            "rebuild must stamp the current version"
        );
        let _ = fs::remove_dir_all(root);
    }
```

- [x] **Step 2: 运行测试，确认失败**

Run: `cd src-tauri && cargo test --lib searches_beyond_ten_thousand_pages two_character_cjk_query_matches candidate_recall_matches_full_scan`

Expected: `searches_beyond_ten_thousand_pages` **FAIL**（当前 `MAX_SEARCH_FILES` 截断）；其余两个可能因索引未接线而失败。

**重要（前置条件）**：本 Task 的自愈块使既有测试（它们只写 Markdown、从不建索引）自动受益；若无它，`search.rs` 中 4 个既有 `search_project_inner` 测试会因空索引而失败。本 Task 的 Step 3 已包含自愈块，**不得**省略。

- [x] **Step 3: 替换候选生成段**

将 `search.rs:350-417`（从 `let wiki_root = ...` 到该 `if` 块结束的 `}`）**整块**替换为下面代码。这是完整替换内容，不是片段：

```rust
    // Candidates come from the derived index instead of a full directory walk.
    // BM25 only orders the candidate set for truncation; final ranking stays
    // with score_file/RRF (spec invariant 5).
    //
    // Self-heal: the index is a discardable derivative, so a missing, empty,
    // or stale index is rebuilt rather than surfaced as an error
    // (spec invariant 2, Review Focus #2). This MUST ship in the same task
    // that makes the index the mandatory candidate source — without it, every
    // existing project that has no index.db yet would silently return zero
    // results, and the pre-existing search tests (which never build an index)
    // would all fail.
    let mut db = IndexDb::open(&project_path)?;
    let stale = db.schema_version().unwrap_or(0) != INDEX_SCHEMA_VERSION;
    if stale || db.page_count().unwrap_or(0) == 0 {
        let _ = crate::commands::project_maintenance::rebuild_index_db_inner(
            project_path.clone(),
        );
        db = IndexDb::open(&project_path)?;
    }
    let match_query = crate::commands::segment::build_fts_match_query(&effective_tokens);
    let mut candidate_paths = db.search_candidates(&match_query, CANDIDATE_LIMIT)?;
    // Always probe the exact phrase too: token expansion can drop a page the
    // user typed verbatim.
    if !query_phrase.is_empty() {
        let phrase_query = crate::commands::segment::build_fts_match_query(&[
            query_phrase.clone(),
        ]);
        for path in db.search_candidates(&phrase_query, CANDIDATE_LIMIT)? {
            if !candidate_paths.contains(&path) {
                candidate_paths.push(path);
            }
        }
    }

    for relative_path in candidate_paths {
        let absolute = Path::new(&project_path).join(&relative_path);
        let content = match fs::read_to_string(&absolute) {
            Ok(content) => content,
            Err(_) => continue,
        };
        if let Some(vector_id) = vector_page_id(&relative_path) {
            page_paths_by_vector_id.insert(vector_id, relative_path.clone());
        }
        if let Some(stem) = absolute.file_stem().and_then(|s| s.to_str()) {
            legacy_stem_candidates
                .entry(stem.to_string())
                .or_default()
                .push(relative_path.clone());
        }
        let title = extract_title(
            &content,
            absolute
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default(),
        );
        let hit = score_file(
            &project_path,
            &absolute,
            &content,
            &effective_tokens,
            &query_phrase,
            &query,
            include_content,
        );
        graph_pages.insert(
            normalize_path(&relative_path),
            GraphPage {
                path: relative_path,
                title,
                links: extract_graph_links(&content),
                content,
            },
        );
        if let Some(hit) = hit {
            results.push(hit);
        }
    }
```

**改动要点**（逐条对应上面的替换）：
- `searched_files` 计数器与 `MAX_SEARCH_FILES` 截断块**整体消失**（Task 6 因此只需确认无残留）。
- `entry.path()` → `absolute`；`relative_path` 改为直接来自候选（索引存的已是项目相对路径）。
- `score_file` 的第二个参数由 `entry.path()` 改为 `&absolute`（它接收 `&Path`）。
- 其余逐行逻辑（`page_paths_by_vector_id`、`legacy_stem_candidates`、`graph_pages`、`results`）保持原样，确保 `score_file`/RRF 行为不变。

在常量区新增（`MAX_SEARCH_FILES` 删除后）：

```rust
/// Upper bound on pages handed to the scorer. This bounds memory and IO per
/// query; it is NOT a silent truncation of the corpus, because candidates are
/// selected by relevance rather than by directory order.
const CANDIDATE_LIMIT: usize = 2_000;
```

在文件顶部 `use` 区新增：

```rust
use crate::commands::index_db::{IndexDb, INDEX_SCHEMA_VERSION};
```

`WalkDir` 的 `use` **保留**：`search.rs` 其它位置可能仍在用（Task 6 的 clippy 步骤会暴露未使用告警，届时再删）。

**注意**：`graph_pages` 的构建原先依赖遍历到的全部文件；现在只覆盖候选页。图谱扩展因此可能变浅——这是可接受的 P0 降级（P2.2 会用 `links` 表补回）。但**必须**在 Step 5 的注释中写明，不得静默。

- [x] **Step 4: 运行测试，确认通过**

Run: `cd src-tauri && cargo test --lib`

Expected: 全部 **PASS**，含既有的 `tokenizes_cjk_bigrams_and_chars` 等。

- [x] **Step 5: 记录图谱降级**

在 `search_project_inner` 中 `graph_pages` 构建处加注释：

```rust
            // P0: graph_pages now covers only the candidate set, so graph
            // expansion is shallower than the pre-index behaviour. P2.2
            // restores depth by reading the `links` table.
```

- [x] **Step 6: 提交**

```bash
git add src-tauri/src/commands/search.rs
git commit -m "feat(search): generate candidates from the index instead of a full walk

FTS5 narrows the corpus to a bounded, relevance-ordered candidate set;
score_file and RRF remain the authority on ranking, so search ordering
semantics are unchanged. Adds an exact-phrase probe so token expansion
cannot drop a page the user typed verbatim.

Known P0 limitation: graph expansion now sees only candidates."
```

---

### Task 6: 重命名 `MAX_SEARCH_FILES` 并锁定回归

**注意（已修正的前置条件）**：`MAX_SEARCH_FILES` 在 `search.rs:184` 的 `get_page_links_inner` 中**仍有第二个用途**（图谱路径的页数上限）。因此本 Task **不删除**该常量，而是：

1. 确认候选生成处的截断已由 Task 5 移除（`grep` 应只剩 `:184` 一处使用）。
2. 将常量重命名为 `MAX_GRAPH_PAGES` 以反映其仅存的图谱用途（`get_page_links_inner` 是独立的图谱路径断崖，属 P2.2 范围，P0 不动）。

删除整个常量会破坏 `get_page_links_inner` 的编译——那不在 P0 范围内。

**Files:**
- Modify: `src-tauri/src/commands/search.rs:23`、`search.rs:184`

**Interfaces:**
- Consumes: Task 5 的候选生成
- Produces: 无

- [x] **Step 1: 确认候选生成截断已移除，并重命名常量**

先确认只剩一处使用：

Run: `grep -n "MAX_SEARCH_FILES" src-tauri/src/commands/search.rs`

Expected: 仅剩 `:23`（定义）与 `:184`（`get_page_links_inner`）。若仍见 `searched_files`/截断块，说明 Task 5 未完成，停下来报告。

然后重命名（只改名字，不改 `10_000` 的值，也不改 `:184` 的行为）：

```rust
/// Upper bound on pages enumerated by the graph link path. NOTE: this is a
/// graph-path cap only — search candidates are bounded by CANDIDATE_LIMIT in
/// search_project_inner. The graph path's own cliff is P2.2 scope.
const MAX_GRAPH_PAGES: usize = 10_000;
```

并把 `search.rs:184` 的 `MAX_SEARCH_FILES` 改为 `MAX_GRAPH_PAGES`。

- [x] **Step 2: 确认无残留引用**

Run: `grep -rn "MAX_SEARCH_FILES" src-tauri/src || echo "removed"`

Expected: `removed`

- [x] **Step 3: 运行全量回归**

Run: `cd src-tauri && cargo test --lib`

Expected: 全部 **PASS**（除 7 个既有的 Windows 路径失败），特别是 Task 5 的 `searches_beyond_ten_thousand_pages` 与 `missing_index_is_rebuilt_not_fatal`。

- [x] **Step 4: 提交**

```bash
git add src-tauri/src/commands/search.rs
git commit -m "refactor(search): rename MAX_SEARCH_FILES to MAX_GRAPH_PAGES

The search-path truncation is gone (candidates are now bounded by
relevance via CANDIDATE_LIMIT). The constant's only remaining use is
the graph link path in get_page_links_inner, so the name now says what
it actually caps. That graph cliff is P2.2 scope, not P0."
```

---

### Task 7: 清理死代码标记与文档修正

**说明**：原 Task 7 的自愈实现与其两个测试（`missing_index_is_rebuilt_not_fatal`、`stale_schema_version_triggers_rebuild`）已在 **Task 5** 交付——因为 Task 5 才是让索引成为必需候选来源的那一步，恢复路径必须与之同批，否则中间态会让无索引的既有项目静默返回空结果。因此本 Task 只剩清理与文档两件事。

**Files:**
- Modify: `src-tauri/src/commands/index_db.rs`、`index_build.rs`（仅属性/字段清理）
- Modify: `README_CN.md:208`、`README_CN.md:212`

**Interfaces:**
- Consumes: 无
- Produces: 无新公开接口

- [x] **Step 1: 移除已无必要的 `#[allow(dead_code)]`**

Task 1 在 `index_db.rs` 加了四处 `#[allow(dead_code)]`，Task 3 在 `index_build.rs` 的 `BuildStats.skipped` 上加了一处，都是因为当时接口尚未被消费。现在 `open`/`apply_pragmas`/`schema_version`/`page_count`/`set_schema_version` 均已被 Task 4/5 真实使用。

**逐个**删除这些属性，然后：

Run: `cd src-tauri && cargo build --lib 2>&1 | grep -A3 "never used\|dead_code"`

Expected: 对每个仍报 `never used` 的项，**说明它确实仍未被使用**——此时才保留该处 `#[allow(dead_code)]`，并在其上写一行注释说明为何保留（而不是为了消警告而盲目加回）。不得出现新的警告堆积。

- [x] **Step 2: 删除 `BuildStats.skipped`（YAGNI）**

`skipped` 恒为 `0` 且 P0 中无任何读取方（`RebuildIndexDbResult` 只取 `pages`/`links`）。一个永远是 0、无人读的字段加上它的 `#[allow]` 是纯噪音。

从 `index_build.rs` 的 `BuildStats` 删除该字段，并同步删除 `build_index` 里构造它的 `skipped: 0,` 行。Task 4 只读 `pages`/`links`，不受影响。

Run: `cd src-tauri && cargo build --lib`

Expected: 编译通过，且 `index_build.rs` 中不再有 `#[allow(dead_code)]`。

- [x] **Step 3: 修正文档（E13：文档与代码不符）**

`README_CN.md:208` 与 `README_CN.md:212` 当前声称「同时搜索 wiki/ 和 raw/sources/」与「快速 ANN 检索」，与代码证据不符。改为准确描述：

- 检索覆盖：当前索引 **`wiki/` 下的页面**；`raw/sources/` 中源文章的本地可检索能力由后续阶段（P1.1）交付，**尚未**提供。
- 向量检索：如实描述当前状态；**在 P0.3 交付前不得声称「快速 ANN」**。

同时可补一句说明 `.llm-wiki/index.db` 是磁盘 Markdown 的派生物，可删除后自动重建——这是用户可见的重要事实。

- [x] **Step 4: 全量回归并提交**

Run: `cd src-tauri && cargo test --lib`

Expected: 全部 **PASS**（除 7 个既有的 Windows 路径失败），且失败数与基线持平。

Run: `cd src-tauri && cargo clippy --lib 2>&1 | tail -20`

Expected: 无本计划新增的告警。

```bash
git add src-tauri/src/commands/index_db.rs src-tauri/src/commands/index_build.rs README_CN.md
git commit -m "chore(index): drop now-unused dead_code allowances; correct README

The Task 1 allowances existed only because the interface had no consumers
yet; Task 4/5 consume all of them. BuildStats.skipped was always 0 and
read by nothing, so the field goes too.

README claimed search covered raw/sources/ and that vector search used a
fast ANN index; neither matched the code. Both claims now describe what
actually ships, and the page documents that index.db is a rebuildable
derivative of the Markdown."
```

---
## 完成标准（P0）

- [x] `cargo test --lib` 全绿，含新增的断崖、二字词、召回、自愈测试。
- [x] `grep -rn "MAX_SEARCH_FILES" src-tauri/src` 为空。
- [x] 删除 `.llm-wiki/index.db` 后检索仍可用（自动重建）。
- [x] 10,050 页 fixture 下第 10,001 个之后的页面可被检索命中。
- [x] 中文二字词（`巡检`、`灾备`）可命中。
- [x] `score_file`、`tokenize_query`、RRF 常量与算法**未被修改语义**（`git diff` 复核）。
- [x] `cargo clippy -- -D warnings` 无新增告警。

## 后续计划（不在本计划内）

P1（廉价管道 + 持久化队列 + 合成显式触发）与 P2（UI 虚拟化 + 图谱 `links` 驱动）各自独立成计划，在 P0 合入并取得实测数据后编写。P0 的 `sources`/`jobs`/`job_dlq` 表已按 Spec 建好，P1 直接使用，无需再次迁移 schema。