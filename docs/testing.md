# 测试约定

## 单测摆放

单测与实现同文件，为 `#[cfg(test)] mod tests { use super::*; }` 块，同 Rust 惯例：紧贴被测代码，可访问私有项。测试名称用描述性的英文 snake_case。

- 同步测试用 `#[test]`
- 异步测试用 `#[tokio::test]`
- 断言只用 `assert!` / `assert_eq!`

示例骨架见各源文件尾部，如 `src/adapter/headless/text_util.rs` 的 `mod tests`。

## 集成测试

集成测试属于顶层 `tests/` 目录，每个文件是独立 crate，只能访问公开 API。当前仓库未使用该目录（`tests/` 不存在）。

## 测试范围选择

提交改动时按「变更面」选最小测试集（对齐 pre-push 自查精神），不默认全量运行。职责归属：改动所在模块及其直接依赖跑通即可，只改文档/配置时不重跑整套测试。按需全量验证或交给 CI，见 [ci-cd.md](ci-cd.md)。

## CI 测试门

本地提交前，格式与静态检查按：

- `cargo fmt --check`
- `cargo test --all-targets --locked`
- `cargo clippy --all-targets --locked -- -D warnings`

CI 的 quality 门即运行以上三道，全部通过才进入构建，见 [ci-cd.md](ci-cd.md)。测试用例本身不写入本文档。
