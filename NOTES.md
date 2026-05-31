# 学习笔记

记录开发 gift 时遇到的小知识点 / 踩坑。每条:**结论**(一句话) + **位置**(链回代码) + **要点**(为什么)。

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
