# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 协作约定

- **用英文思考，用中文交流。** 面向用户的回复、以及本仓库内的文档（如本文件）都用中文撰写，与代码里的中文注释保持一致。
- 关于本项目的背景信息统一记录在本 CLAUDE.md 中，不另外写 memory 文件；需要时直接从这里读取。

## 项目简介

`gift` 是用 Rust（edition 2024）从零复刻 Git 底层（plumbing）与上层（porcelain）命令的学习项目。它直接读写 Git 真实的磁盘格式（loose object、`DIRC` 索引、ref、`HEAD`），因此产物与真实 `git` 二进制**逐字节兼容**——这正是测试所断言的。

源码注释和设计文档（`change_plan.md`）均为中文；`change_plan.md` 是「已实现 / 待实现」的权威路线图。

## 常用命令

```bash
cargo build                       # 调试构建 -> target/debug/gift
cargo build --release
cargo test                        # 跑全部测试（注意下方限制）
cargo test commit_on_branch       # 按（子）名跑单个测试
cargo test -- --nocapture         # 测试时显示 println! / git 的输出
cargo run -- <subcommand> ...     # 运行 CLI，例如 cargo run -- init
cargo run --example try_write_hash_object
```

### 测试的关键前提（重要）

- **测试依赖系统里真实的 `git` 二进制。** 测试是*差分式*的：`src/tests.rs` 通过 `run_git` / `git_stdout` 调用 `git init` / `git add` / `git write-tree` / `git commit-tree` / `git ls-tree` / `rev-parse`，再断言 gift 的 OID、tree 字节、解析出的结构体与真实 Git 完全一致。
- **测试仅限 Unix。** 代码大量使用 `std::os::unix`（符号链接、权限、inode/dev、`OsString` 字节转换）；部分测试带 `#[cfg(unix)]`。
- 每个测试通过 `make_test_repo` / `make_gift_repo` 在 `target/inspect/<case>-<unix_ts>/` 下新建独立测试目录，返回 `TestRepo { worktree, git_abs }`；这些是构建产物（`/target` 已被 gitignore），可随时删除。

## 架构

整体流程对应 Git：工作区 → blob + **索引(index)** → **tree** → **commit** → **ref**/**HEAD**。模块自底向上分层，从磁盘原语到命令。

**磁盘原语**
- `src/object.rs` —— 对象模型。`ObjectSha`（目前只真正支持 SHA1；SHA256 是占位，多数写路径会 `bail!`）、`FileMode`、以及 `Object` 枚举（`Blob`/`Tree`/`Commit`/`Tag`-TODO）。负责 loose object 的读写：`flate2` 做 zlib，处理 `type <len>\0` 头（`read_object_type` → `skip_git_object_size_nul` → `read_*`），以及 `hash_object`、`write_hash_object`、`commit_tree`。`TreeObject`/`CommitObject` 能从解压后的流自解析，并通过 `to_binary` 重新序列化。
- `src/index.rs` —— `DIRC` 索引文件：`parse_index_file` / `write_index_file`（尾部带 SHA1 校验、entry 按 8 字节对齐、临时文件原子 rename）。子模块 `index_tree` 持有内存目录树（`TreeNode::{Tree,Blob}` → `BlobLeaf`，原 `IndexRootTree` 已合并进 `TreeNode`）；`TreeNode::from_index_file` 把扁平索引重建为目录层级树，`TreeNode::write_tree_return_entry` 递归写入 loose tree 对象并返回 `TreeEntry`；checkout 时从 `TreeObject` 重建树的 `from_tree_object` 定义在 `checkout.rs`。
- `src/git_paths.rs` —— 路径约定与 `discover_repo_from_cwd`（向上查找 `.gift`）。
- `src/reference.rs` —— 直接 ref（40 位 hex 的 OID 文件）：`Ref`、`read_ref`/`update_ref`（都会校验目标是 `commit` 对象）、`branch`。
- `src/symbolic_ref.rs` —— 符号 ref（`ref: <name>` 文件）。
- `src/head.rs` —— `Head` 枚举：`TargetBranch{branch_ref_path}`（符号引用）vs `TargetCommit(oid)`（detached）。集中处理读 `HEAD`、解析当前 commit、以及 `record_new_commit`（更新分支 tip ref，或重写 detached `HEAD`）。

**命令 / porcelain**
- `src/staging.rs` —— `stage_paths`（即 `git add`）：把输入解析到工作区内、递归目录、写 blob、更新索引。尚未实现 `.gitignore`，也未处理删除。
- `src/commit.rs` —— `commit`：索引 → `write_tree` → 依据 `HEAD` 解析父 commit → 构建并写入 `CommitObject` → 移动 ref/`HEAD`。
- `src/commit_identity.rs` —— 从 `GIT_AUTHOR_*` / `GIT_COMMITTER_*` 环境变量构造 author/committer（遵循 Git 的回退规则）。
- `src/checkout.rs` —— `checkout`，目标为 `CheckoutTarget::{Commit,Branch}`（由 `FromStr` 解析：40/64 位 hex ⇒ commit，否则按分支名）。读取目标 tree，**在改动任何文件前先检查未追踪文件冲突**，删除已移除的被追踪文件，清理空目录，落地 blob/符号链接/权限位，最后重写索引与 `HEAD`。
- `src/status.rs` —— `status`：工作区 vs 索引的差异，先用缓存的 stat 快速比较（`is_stat_changed`），再用哈希确认。**HEAD vs 索引（已暂存改动）尚未实现**（`staged` 硬编码为空）。
- `src/get_args.rs` —— `clap` 子命令分发（`init`、`hash-object`、`add`、`commit`、`branch`、`checkout`、`status`）。`src/main.rs` 仅调用 `gift::run`。

### 贯穿全代码库的约定

- **`worktree` + `git_abs` 参数成对出现。** 多数函数同时接收 `worktree`（工作区根的绝对路径）和 `git_abs`（Git 目录的绝对路径，如 `/path/to/repo/.git` 或 `/path/to/repo/.gift`）。两者均为绝对路径，函数内部直接使用，不再做 `worktree.join(git_abs)` 转换。`discover_repo_from_cwd()` 返回的 `RepoPaths { worktree, git_abs }` 是获取这两个路径的统一入口。**例外：index entry 内部记录的文件路径是相对 `worktree` 的**，与 git 格式一致，不属于此处的 `git_abs`。
- **`.gift` 与 `.git`。** CLI 硬编码 `.gift`（`init` 和 `hash-object` 处理逻辑里直接写 `.gift`；`discover_repo_from_cwd` 搜索 `.gift`）。库函数本身与 Git 目录名无关，而测试套件用 `git init` 创建的真实 `.git` 仓库来驱动它们——同一套代码路径对两者都被覆盖。测试中 `make_test_repo` 对应 `.git`，`make_gift_repo` 对应 `.gift`。
- **需要留意的 CLI 缺口。** `status` 子命令目前只 `println!("Status")`，并**没有**接到 `status::status`。扩展 CLI 时，请对照 `get_args.rs` 与实际库函数。
- **路径以字节存储。** 索引 entry 路径与 tree entry 名按原始字节（`Vec<u8>` / 经 `OsStrExt` 的 `OsString`）存储与比较，分隔符归一为 `/`，以保留非 UTF-8 / 非 ASCII 名称——与 Git 一致。在意正确性的地方避免经 lossy `String` 往返。
- **SHA256 并未真正支持。** 类型系统里有 `ObjectSha::SHA256`，但多数写/编码路径对它 `bail!`。可按只有 SHA1 来处理。
