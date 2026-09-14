# 仓库贡献指南（Repository Guidelines）

## 项目结构与模块组织

本仓库是名为 `ripgrep-web` 的单 crate Rust 项目。HTTP 服务、检索流程、路径校验、压缩文件读取和单元测试位于 `src/main.rs`。浏览器界面位于 `src/index.html`，通过 `include_str!` 嵌入二进制；除非架构发生变化，前端修改应集中在该文件。部署资源位于 `deploy/`（当前为 systemd 单元），文档和视觉参考位于 `README.md` 与 `docs/`，CI 和发布自动化位于 `.github/workflows/`。

## 构建、测试与本地开发

使用 Cargo，并优先采用锁定的依赖版本：

```bash
cargo fmt --check                 # 检查 rustfmt 格式
cargo test --locked               # 运行单元测试
cargo build --locked --release    # 构建生产二进制文件
LOG_BASE_DIR=/var/log LISTEN_ADDR=127.0.0.1:5000 cargo run --release
```

启动服务后访问 `http://127.0.0.1:5000`。排查问题时可设置 `RUST_LOG`，例如 `RUST_LOG=ripgrep_web=debug`。

## 编码风格与命名约定

提交前运行 `cargo fmt`，遵循 Rust 2021 的惯用写法。使用四个空格缩进；函数、变量和模块使用 `snake_case`，类型使用 `PascalCase`，常量使用 `SCREAMING_SNAKE_CASE`。除非同步更新 README 和界面，否则不要随意修改用户可见的错误文本和 API 字段名。前端 JavaScript 延续现有紧凑风格，并保持语义化 HTML、可访问标签和响应式 CSS。

## 测试指南

测试集中在 `src/main.rs` 的 `#[cfg(test)] mod tests` 中。测试名称应描述可观察行为，例如 `path_traversal_is_rejected`、`searches_gzip_file_when_enabled`。针对结果上限、取消检索、路径安全、压缩输入和时间过滤补充回归测试。每次修改都应通过 `cargo test --locked` 和 `cargo fmt --check`。

## 提交与拉取请求

提交主题使用简短的祈使句，例如 `Update checkout action to v7` 或 `Make release checksum portable`，并拆分无关改动。拉取请求应说明用户可见或运维层面的影响，列出配置和 API 变更，有关联 issue 时附上链接；涉及界面时提供截图或简短录屏。请求评审前运行上述格式化和测试命令；发布相关改动还应确认标签版本与 `Cargo.toml` 一致。

## 发布后同步 Homebrew Formula

每次 GitHub Release 发布成功后，必须同步检查并更新 [`ng-life/homebrew-personal`](https://github.com/ng-life/homebrew-personal) 中的 `Formula/ripgrep-web.rb`。将 macOS ARM64 和 Linux x86_64 下载地址改为新版本，并用该版本发布产物的 SHA256 校验值更新 Formula；同时检查 `.github/workflows/release.yml` 的 Formula 生成模板和手动触发默认标签，确保后续自动同步不会覆盖这些改动。运行 Ruby 语法及 Homebrew 样式检查，确认 Formula 已推送到 GitHub 且指向本次发布后，才算完成发布。

## 安全与配置提示

服务会将检索范围限制在 canonicalize 后的 `LOG_BASE_DIR` 内，修改路径处理时必须保留这一边界。不要将未认证接口直接暴露到公网。生产环境应使用只读服务账号，并通过启用 TLS 和认证的反向代理提供访问，具体配置参见 `README.md`。
