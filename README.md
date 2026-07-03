# gift: 更快，更安全，更智能的 git

gift 是一个用 Rust 从零实现的 Git 学习项目，目标是在理解 Git 底层数据结构的基础上，探索一个更快、更安全、更智能的版本控制工具。它直接读写 Git 的真实磁盘格式，并通过与官方 Git 的差分测试验证兼容性。

在此基础上，giftai 进一步把 AI 对话上下文建模为 Git commit DAG：一轮问答就是一次提交，分支、切换、合并等 Git 机制可以自然表达不同的思考路径。

这个项目不只是复刻 Git，也是在探索一个问题：如果把版本控制的对象模型、历史 DAG 和 AI 上下文管理结合起来，开发工具还能长成什么样。

## 为什么做 gift

**更快**：用 Rust 重新实现 Git 的核心路径，直接面向 blob、tree、commit、index、ref 等底层结构工作。这里的“快”不仅指运行效率，也指学习和调试时更快看见 Git 的真实机制，而不是把 Git 当成黑盒。

**更安全**：对象内容寻址天然保证数据可校验；checkout 前会检查未追踪文件冲突，避免直接覆盖用户文件；测试大量对照官方 Git，让实现尽量靠近真实行为。

**更智能**：giftai 把 AI 对话历史放进 Git DAG 中管理，让一次问答、一次分叉、一次合并都可以被记录、切换和复现。它把 Git 的历史管理能力扩展到了“思考过程”和“上下文选择”上。

## 项目亮点

- 从零实现 Git 核心数据模型：blob、tree、commit、ref、symbolic HEAD、index。直接操作 Git 真实磁盘格式：loose object、`DIRC` index、refs、`HEAD`。
- 已实现常用命令：`init`、`hash-object`、`add`、`commit`、`branch`、`checkout`、`status`、`log`、`merge`、`ls-remote`、`clone`、`fetch`、`pull`、`push`。
- 额外实现 `giftai`：把 AI 对话建模为 commit DAG，支持 `init`、`ask`、`context`、`log`、`graph`、`branch`、`checkout`、`merge`。

## 快速开始

环境要求：

- Rust 2024 toolchain
- Unix-like 系统
- 系统中已安装真实 `git`，测试会调用它做对照

构建与测试：

```bash
cargo build
cargo test
```

因为仓库里有两个二进制入口，运行时需要显式指定 `--bin gift` 或 `--bin giftai`。

试用 `gift`：

```bash
cargo build
GIFT_BIN="$(pwd)/target/debug/gift"

mkdir -p /tmp/gift-demo
cd /tmp/gift-demo

"$GIFT_BIN" init
printf "hello gift\n" > hello.txt
"$GIFT_BIN" add hello.txt
"$GIFT_BIN" commit -m "first commit"
"$GIFT_BIN" status
"$GIFT_BIN" log
```

也可以在项目根目录中通过 Cargo 查看主 CLI 帮助：

```bash
cargo run --bin gift -- --help
```

试用 `giftai`：

```bash
export GIFTAI_API_KEY=...
cargo build
GIFTAI_BIN="$(pwd)/target/debug/giftai"

mkdir -p /tmp/giftai-demo
cd /tmp/giftai-demo

"$GIFTAI_BIN" init
"$GIFTAI_BIN" ask "解释一下 Git 的 tree object"
"$GIFTAI_BIN" log
"$GIFTAI_BIN" graph
```

`giftai` 默认使用 DeepSeek 兼容接口，也可以通过环境变量切换 OpenAI 兼容服务：

```bash
export GIFTAI_BASE_URL=https://api.deepseek.com
export GIFTAI_MODEL=deepseek-chat
```

## 功能完成度

| 分组 | 已实现能力 | 说明 |
| --- | --- | --- |
| 对象库 | `hash-object`、loose object 读写、blob/tree/commit 解析与序列化 | 主要支持 SHA1；SHA256 类型存在但不是完整实现 |
| index 与暂存 | `DIRC` index 解析/写入、递归 `add`、index -> tree | index 路径按 Git 语义保存为相对 worktree 的字节路径 |
| 历史与引用 | `commit`、`branch`、`checkout`、`log`、ref、symbolic HEAD | 支持分支 HEAD 与 detached HEAD |
| 工作区状态 | `status`、基础 ignore 规则 | `status` 已接入 CLI；仍有 Git 完整行为边界可继续补齐 |
| 差异与合并 | blob/tree/commit diff、`merge-base`、三方 merge | 冲突路径会报告并返回失败 |
| 网络能力 | `ls-remote`、`clone`、`fetch`、`pull`、`push` | 覆盖学习项目所需的核心协议路径，仍不是完整 Git 网络实现 |
| AI 实验 | `giftai init/ask/context/log/graph/branch/checkout/merge` | `--select` 当前按轮号选择上下文，不支持 OID 选择 |

## 架构说明

gift 的核心数据流和 Git 一致：

```text
worktree -> index -> tree -> commit -> ref/HEAD
```

主要模块：

- `src/object.rs`：Git 对象模型与 loose object 读写，包含 blob、tree、commit 的解析和二进制序列化。
- `src/index/`：`DIRC` index 文件格式、暂存区 entry、index tree 构建和 tree object 写入。
- `src/commit.rs`、`src/head.rs`、`src/reference.rs`：提交创建、父提交解析、分支 ref 和 `HEAD` 移动。
- `src/checkout.rs`、`src/status.rs`、`src/diff.rs`、`src/merge.rs`：工作区层能力，包括检出、状态、差异和合并。
- `src/get_packfile_by_network.rs`、`src/fetch.rs`、`src/pull.rs`、`src/push.rs`：远端引用发现、packfile 获取、拉取和推送。
- `src/ai/`：giftai 对话 DAG 实验，包括消息文件、LLM 客户端、历史视图和对话合并。

路径约定上，大多数内部函数同时接收：

- `worktree`：工作区根目录的绝对路径。
- `git_abs`：Git 元数据目录的绝对路径，例如 `.gift`、`.git` 或 `.giftai`。

这让库函数不依赖进程当前目录，也方便同一套底层逻辑服务 `gift` 和测试中的真实 `.git` 仓库。

## 测试策略

这个项目的测试重点是“差分测试”：先用真实 `git` 生成或解析结果，再和 gift 的实现结果比较。这样可以把学习项目从“看起来像 Git”推进到“在关键字节和行为上接近 Git”。

测试覆盖包括：

- object/index/tree/commit 与官方 Git 的哈希和字节级兼容性。
- checkout、status、diff、merge-base、merge 等用户可见行为。
- 引用、symbolic ref、detached HEAD、branch commit 等历史操作。
- giftai 的 message 组装、轮号选择、LLM 配置解析和响应解析。

常用测试命令：

```bash
cargo test
cargo test commit_on_branch
cargo test -- --nocapture
```

## giftai 设计简述

giftai 的核心想法是：AI 对话也可以被版本控制。

- 一轮 AI 问答 = 一个 commit。
- 当前 `HEAD` 决定默认上下文。
- `branch` / `checkout` 表达对话分叉和回到旧思路。
- `merge` 把两条对话线合并成一条新的上下文线。
- `context` 可以预览本次将发送给模型的 messages。
- `--select` 当前按轮号选择上下文，例如只发送第 1、3 轮；它是本轮临时选择，不改变 commit DAG。

giftai 的仓库布局：

```text
project/
  .giftai/       # 对话元数据，复用 Git 风格对象库和 refs
  chat/          # 对话工作区，保存 0001-system.txt、0002-user.txt 等消息文件
```

配置来自环境变量，密钥不会写入仓库：

- `GIFTAI_API_KEY`：API 密钥；如果没有设置，会回退读取 `DEEPSEEK_API_KEY`。
- `GIFTAI_BASE_URL`：OpenAI 兼容接口地址，默认 `https://api.deepseek.com`。
- `GIFTAI_MODEL`：模型名，默认 `deepseek-chat`。

如果已经把 `giftai` 放进 `PATH`，示例：

```bash
export GIFTAI_API_KEY=...
giftai init
giftai ask "记住一个数字：42"
giftai ask "我刚才让你记住的数字是多少？"
giftai log
giftai context --select 1
```

## 后续计划

- 完善 Git 兼容性边界：packfile、协议细节、index 删除、更多 `status` 行为。
- 增强 giftai：OID 选择、token 预算、摘要压缩、配置文件、更多 OpenAI 兼容模型适配。
- 补充文档：模块设计说明、命令示例、内部格式笔记。
- 继续扩大差分测试范围，让更多行为可以和官方 Git 做自动对照。

## 当前限制

- 项目主要支持 SHA1；SHA256 还不是完整可用路径。
- 代码和测试大量依赖 Unix 文件系统语义，例如权限位、符号链接和原始路径字节。
- 这是学习项目和实验工具，不是生产级 Git 替代品。
- 网络、merge、status 等能力覆盖了核心学习路径，但没有追求完整复刻官方 Git 的全部边界行为。
