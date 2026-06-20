//! `giftai` 二进制的薄入口：所有逻辑都在 `gift::run_ai()`（见 `src/ai/`）。

fn main() {
    if let Err(e) = gift::run_ai() {
        // `{:#}` 展开 anyhow 的完整错误链（含 context）
        eprintln!("giftai: {e:#}");
        std::process::exit(1);
    }
}
