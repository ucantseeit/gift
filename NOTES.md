# 学习笔记

记录开发 gift 时遇到的小知识点 / 踩坑。每条:**结论**(一句话) + **位置**(链回代码) + **要点**(为什么)。

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
