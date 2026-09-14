# Ripgrep Web

轻量级 B/S 日志检索服务。后端使用 ripgrep 官方代码库中的 `grep-regex`、`grep-searcher` 与 `ignore` crates 在进程内完成目录遍历和检索，不执行 shell，也不依赖服务器预装 `rg`。前端通过 `include_str!` 嵌入二进制。

当前版本：`0.0.2`

## 功能

- 字面量或正则表达式检索、大小写选项、全局结果上限
- `--config` 指定 JSON 配置文件，Homebrew 服务默认使用生成的配置文件
- SSE 实时流式返回，浏览器可中止检索并保留已经接收的结果
- 可选的 `-z/--search-zip` 风格压缩日志检索：ZIP 成员及 `.gz`、`.bz2`、`.xz/.lzma`、`.zst` 文件
- 类似 `fd --changed-within` 的文件修改时间过滤，只检索指定时长内更新的文件
- 文件路径、行号和 UTF-8 字节级匹配范围（前端精准高亮）
- canonicalize 后的日志根目录白名单校验，可阻止目录穿越和符号链接逃逸
- 并发检索信号量、30 秒请求超时、二进制文件跳过
- `/healthz` 健康检查、gzip、请求日志、优雅退出
- 单个 release 二进制和加固的 systemd 服务配置

## 本地运行

```bash
LOG_BASE_DIR=/var/log LISTEN_ADDR=127.0.0.1:5000 cargo run --release
```

打开 <http://127.0.0.1:5000>。路径输入框只接受 `LOG_BASE_DIR` 下的相对路径；留空表示搜索整个根目录。

配置项：

| 环境变量 | 默认值 | 说明 |
| --- | --- | --- |
| `LOG_BASE_DIR` | `/var/log` | 允许检索的根目录，启动时必须存在 |
| `LISTEN_ADDR` | `0.0.0.0:5000` | HTTP 监听地址 |
| `MAX_CONCURRENT_SEARCHES` | `4` | 同时运行的检索任务数 |
| `RUST_LOG` | `ripgrep_web=info,tower_http=info` | 日志过滤器 |

## 配置文件

命令行可通过 `--config` 指定 JSON 配置文件；各字段优先使用配置文件中的值，缺失时依次回退到环境变量和内置默认值（`rust_log` 由 `RUST_LOG` 环境变量优先覆盖）：

```bash
ripgrep-web --config /etc/ripgrep-web/config.json
ripgrep-web --config=/etc/ripgrep-web/config.json
```

配置字段如下：

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `base_dir` | `/var/log` | 允许检索的根目录，启动时必须存在 |
| `listen_addr` | `0.0.0.0:5000` | HTTP 监听地址 |
| `max_concurrent_searches` | `4` | 同时运行的检索任务数 |
| `rust_log` | `ripgrep_web=info,tower_http=info` | 日志过滤器，`RUST_LOG` 环境变量优先 |

不传 `--config` 时，服务仍支持 `LOG_BASE_DIR`、`LISTEN_ADDR`、`MAX_CONCURRENT_SEARCHES` 和 `RUST_LOG` 环境变量。

Homebrew 安装时会在 `$(brew --prefix)/etc/ripgrep-web.json` 生成默认配置；`brew services start ng-life/personal/ripgrep-web` 会使用该配置启动服务。升级包不会覆盖已有配置。

## 构建与验证

```bash
cargo fmt --check
cargo test
cargo build --release
```

产物为 `target/release/ripgrep-web`。它是单文件应用，但默认 Linux GNU 构建仍会动态链接系统 libc；若需要跨发行版的真正静态二进制，请使用 musl target 构建。

## systemd 部署

推荐创建只读服务账号，而不是使用 root：

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin ripgrep-web
sudo install -m 0755 target/release/ripgrep-web /usr/local/bin/ripgrep-web
sudo install -m 0644 deploy/ripgrep-web.service /etc/systemd/system/ripgrep-web.service
sudo usermod -aG adm ripgrep-web   # Debian/Ubuntu；按实际日志权限调整
sudo systemctl daemon-reload
sudo systemctl enable --now ripgrep-web
sudo systemctl status ripgrep-web
sudo journalctl -u ripgrep-web.service -f
```

服务模板默认监听 `0.0.0.0:5000`。这会允许网络访问；生产环境请使用防火墙限制来源，或通过带 TLS 和认证的反向代理暴露，不要把未认证的日志检索接口直接开放到公网。

## GitHub Release

推送与 `Cargo.toml` 版本一致的标签（例如 `v0.0.1`）会触发 GitHub Actions，运行检查并创建包含 Linux x86_64 二进制、systemd 单元和 SHA256 校验文件的 Release。

## API

```text
GET /api/search?keyword=error&path=nginx&changed_within=2h&regex=false&case_sensitive=false&search_zip=true&limit=1000
```

响应类型为 `text/event-stream`：

- `match`：单条结果，包含 `path`、`line_number`、`line` 与 `submatches`
- `done`：检索汇总，包含 `count`、`truncated` 和 `elapsed_ms`
- `error`：流建立后的检索错误

客户端断开或使用 `AbortController.abort()` 中止请求后，服务端会停止扫描。

启用 `search_zip` 后，ZIP 归档内的结果路径显示为 `archive.zip!成员路径`。压缩内容只在内存中流式解码，不写入磁盘；每个压缩文件或 ZIP 成员最多读取 256 MiB，以降低压缩炸弹风险。

`changed_within` 接受 `10min`、`2h`、`1d`、`2weeks` 等相对时长；留空表示不限制。它按文件系统修改时间过滤，ZIP 内部成员沿用 ZIP 归档文件本身的修改时间。
