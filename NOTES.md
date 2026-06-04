# 学习笔记

记录开发 gift 时遇到的小知识点 / 踩坑。每条:**结论**(一句话) + **位置**(链回代码) + **要点**(为什么)。

---

## git fetch / push / pull 的关系

- 日期: 2026-06-03
- 结论: **`pull = fetch + merge`，日常更推荐先 `fetch` 再手动决定如何整合。**
- 要点:
  - `git fetch`：只把远程分支下载到本地（`origin/main` 等远程追踪分支），**不动本地分支**，安全无副作用，可以先 `git log origin/main` 看看差异再决定。
  - `git push`：把本地分支推送到远程，更新远程 ref。
  - `git pull`：等价于 `fetch` 后立即 `merge`，会直接改动当前分支，如果有冲突或想审查变更则不够灵活。
  - 推荐习惯：`git fetch && git log HEAD..origin/main` 看差异，再选 `git merge`。

---

## Rust `Hash` trait 的实现原理

- 日期: 2026-06-02
- 结论: **哈希不是直接对值算，而是把字节流写进一个 `Hasher` 容器，由容器统一做哈希运算。**
- 两个 trait 的关系:
  - `Hash`：**被哈希的类型**（整数、字符串、结构体……）实现，负责"把自身字节喂给 `Hasher`"。
  - `Hasher`：**哈希算法容器**（`DefaultHasher` 等）实现，负责"消费字节流，产出哈希值"。两者角色不同，普通类型只需实现 `Hash`。
  ```rust
  pub trait Hash {
      fn hash<H: Hasher>(&self, state: &mut H);
  }

  pub trait Hasher {
      fn write(&mut self, bytes: &[u8]);
      fn finish(&self) -> u64;
      // 还有 write_u8 / write_u64 / ... 等便捷方法，默认实现都调用 write
  }
  ```
- 自己实现 `Hash`（不用 `#[derive]`）需要做什么：
  - 实现 `fn hash<H: Hasher>(&self, state: &mut H)`，调用 `state.write(...)` 或各字段的 `.hash(state)`，把"代表自身唯一性"的字节全部喂给 `state`。
  - 若还要实现 `Hasher`（自定义算法），还需实现 `write` 和 `finish`。
- 各类型 `Hash` 实现的规律:
  - **整数 / 字符串 / 数组&slice**：都实现了 `Hash`，内部直接把字节表示 `write` 进 `Hasher`。
  - **结构体**：`#[derive(Hash)]` 展开为依次调用每个字段的 `hash(state)`。手动实现时要注意**字段之间加分隔符**（如 `state.write_u8(0xff)`），防止 `("ab","c")` 与 `("a","bc")` 碰撞。
  - **enum**：先写入变体的判别值（discriminant），再写入字段，天然区分不同变体。
- 核心性质：**若 `x == y`，则 `H(x) == H(y)`**（反之不必然，碰撞可能存在）。自定义类型实现 `Eq` 时必须同步实现 `Hash` 且逻辑一致——这是 `HashMap`/`HashSet` 正确工作的前提。

---

## 路径：绝对 vs 相对的权衡

- 日期: 2026-05-31
- 位置: `src/git_paths.rs`、`src/tests.rs`
- 结论:
  - **对外接口可以兼容，进入项目内部后尽快转成绝对路径**
  - **凡是直接读写文件/文件夹的函数，明确声明接收绝对路径，并要求上层配合传入**
- 要点:
  - 相对路径隐式依赖当前工作目录（CWD），函数内部逻辑难以推断路径是否正确；绝对路径消除这个歧义。
  - "尽快转换"的意思：在系统边界（CLI 入口、测试 setup）拿到用户输入后立即 `fs::canonicalize` 或 `cwd.join(...)`，之后内部全程传绝对路径。
  - 只有有明确"锚定"（比如 index 文件里记录的路径相对 worktree）时才保留相对路径，且要明确标注相对于什么。

### 本项目中的具体应用

- `git_abs`（git 仓库目录）本来写成 `git_dir`，当作相对 worktree 的路径传入，每个函数内部都要 `worktree.join(git_dir)` 转换一次——冗余且容易出错。
- 重构后 `git_abs` 全程是绝对路径；`discover_repo_from_cwd()` 返回的 `RepoPaths` 直接提供绝对路径，调用方无需再做拼接。
- **例外：index 文件内部记录的文件路径是相对 worktree 的**，这与 git 格式一致，不应改变。index entry 的 path 字段就代表"相对于 worktree 的位置"，读写 index 的代码需要拿到 worktree 才能还原成磁盘绝对路径。

---

## impl AsRef<Path> vs &Path

- 日期: 2026-05-30
- 位置: src/checkout.rs
- 结论:
  - **内部函数用 `&Path`**
  - **对外 API 接口用 `impl AsRef<Path>`**
- 补充:
  - 函数内部 `.as_ref()` 之后两者完全一样,泛型只是给调用方省事(能直接传 `&str` / `String`)。
  - `&PathBuf` 会 Deref 自动强转成 `&Path`,所以 `&Path` 没那么挑;真正传不进的只有 `&str` / `String` / `OsStr`。
