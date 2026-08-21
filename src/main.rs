use std::{
    convert::Infallible,
    env,
    fs::File,
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime},
};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{sse::Event, sse::KeepAlive, Html, IntoResponse, Sse},
    routing::get,
    Json, Router,
};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{sinks::UTF8, BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio_stream::wrappers::ReceiverStream;
use tower_http::{compression::CompressionLayer, trace::TraceLayer};
use tracing::{info, warn};

const DEFAULT_BASE_DIR: &str = "/var/log";
const DEFAULT_LISTEN: &str = "0.0.0.0:5000";
const DEFAULT_LIMIT: usize = 1_000;
const HARD_LIMIT: usize = 2_000;
const MAX_PATTERN_BYTES: usize = 4_096;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DECOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone)]
struct AppState {
    base_dir: Arc<PathBuf>,
    search_slots: Arc<Semaphore>,
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    keyword: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    case_sensitive: bool,
    #[serde(default)]
    search_zip: bool,
    #[serde(default)]
    changed_within: String,
    limit: Option<usize>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
struct Submatch {
    start: usize,
    end: usize,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
struct SearchMatch {
    path: String,
    line_number: u64,
    line: String,
    submatches: Vec<Submatch>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct SearchDone {
    count: usize,
    truncated: bool,
    elapsed_ms: u128,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SearchOutcome {
    count: usize,
    truncated: bool,
    cancelled: bool,
}

#[derive(Debug, Error)]
enum AppError {
    #[error("检索关键词不能为空")]
    EmptyKeyword,
    #[error("检索关键词过长（最大 {MAX_PATTERN_BYTES} 字节）")]
    PatternTooLong,
    #[error("路径不存在、不可访问或超出日志根目录")]
    InvalidPath,
    #[error("正则表达式无效: {0}")]
    InvalidRegex(String),
    #[error("修改时间范围无效: {0}（示例：10min、2h、1d、2weeks）")]
    InvalidDuration(String),
    #[error("检索执行失败: {0}")]
    Search(String),
    #[error("检索超时（最长 30 秒）")]
    Timeout,
    #[error("服务正在关闭")]
    Unavailable,
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let status = match self {
            Self::EmptyKeyword
            | Self::PatternTooLong
            | Self::InvalidPath
            | Self::InvalidRegex(_)
            | Self::InvalidDuration(_) => StatusCode::BAD_REQUEST,
            Self::Timeout => StatusCode::REQUEST_TIMEOUT,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Search(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(serde_json::json!({ "error": self.to_string() })),
        )
            .into_response()
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ripgrep_web=info,tower_http=info".into()),
        )
        .init();

    let base_dir = env::var("LOG_BASE_DIR").unwrap_or_else(|_| DEFAULT_BASE_DIR.into());
    let base_dir = std::fs::canonicalize(&base_dir)
        .unwrap_or_else(|error| panic!("LOG_BASE_DIR {base_dir:?} 不可访问: {error}"));
    let listen = env::var("LISTEN_ADDR").unwrap_or_else(|_| DEFAULT_LISTEN.into());
    let listen: SocketAddr = listen.parse().expect("LISTEN_ADDR 格式无效");
    let max_concurrent = env::var("MAX_CONCURRENT_SEARCHES")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(4);

    let state = AppState {
        base_dir: Arc::new(base_dir),
        search_slots: Arc::new(Semaphore::new(max_concurrent)),
    };
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .expect("监听端口失败");
    info!(%listen, base_dir = %state.base_dir.display(), max_concurrent, "ripgrep Web 已启动");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("Web 服务异常退出");
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/healthz", get(health_handler))
        .route("/api/search", get(search_handler))
        .with_state(state)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

async fn health_handler() -> &'static str {
    "ok"
}

async fn search_handler(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Result<Sse<ReceiverStream<Result<Event, Infallible>>>, AppError> {
    validate_keyword(&query.keyword)?;
    let target = resolve_target(&state.base_dir, &query.path)?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, HARD_LIMIT);
    let matcher = build_matcher(&query.keyword, query.regex, query.case_sensitive)?;
    let changed_after = parse_changed_within(&query.changed_within)?;
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);
    let cancelled = Arc::new(AtomicBool::new(false));
    let slots = Arc::clone(&state.search_slots);

    tokio::spawn(async move {
        let permit = match slots.acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                send_sse_error(&sender, AppError::Unavailable.to_string()).await;
                return;
            }
        };
        let started = Instant::now();
        let blocking_sender = sender.clone();
        let blocking_cancelled = Arc::clone(&cancelled);
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            stream_search_logs(
                &target,
                matcher,
                limit,
                query.search_zip,
                changed_after,
                &blocking_cancelled,
                |item| {
                    serde_json::to_string(&item).is_ok_and(|data| {
                        blocking_sender
                            .blocking_send(Ok(Event::default().event("match").data(data)))
                            .is_ok()
                    })
                },
            )
        });

        match tokio::time::timeout(SEARCH_TIMEOUT, task).await {
            Ok(Ok(Ok(outcome))) if !outcome.cancelled => {
                let done = SearchDone {
                    count: outcome.count,
                    truncated: outcome.truncated,
                    elapsed_ms: started.elapsed().as_millis(),
                };
                if let Ok(data) = serde_json::to_string(&done) {
                    let _ = sender
                        .send(Ok(Event::default().event("done").data(data)))
                        .await;
                }
            }
            Ok(Ok(Ok(_))) => {}
            Ok(Ok(Err(error))) => send_sse_error(&sender, error.to_string()).await,
            Ok(Err(error)) => {
                send_sse_error(&sender, AppError::Search(error.to_string()).to_string()).await
            }
            Err(_) => {
                cancelled.store(true, Ordering::Relaxed);
                send_sse_error(&sender, AppError::Timeout.to_string()).await;
            }
        }
    });

    Ok(Sse::new(ReceiverStream::new(receiver)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(10))
            .text("keep-alive"),
    ))
}

async fn send_sse_error(
    sender: &tokio::sync::mpsc::Sender<Result<Event, Infallible>>,
    message: String,
) {
    let data = serde_json::json!({ "error": message }).to_string();
    let _ = sender
        .send(Ok(Event::default().event("error").data(data)))
        .await;
}

fn validate_keyword(keyword: &str) -> Result<(), AppError> {
    if keyword.is_empty() {
        return Err(AppError::EmptyKeyword);
    }
    if keyword.len() > MAX_PATTERN_BYTES {
        return Err(AppError::PatternTooLong);
    }
    Ok(())
}

fn resolve_target(base_dir: &Path, requested: &str) -> Result<PathBuf, AppError> {
    let relative = requested.trim().trim_start_matches('/');
    let candidate = if relative.is_empty() {
        base_dir.to_path_buf()
    } else {
        base_dir.join(relative)
    };
    let canonical = std::fs::canonicalize(candidate).map_err(|_| AppError::InvalidPath)?;
    if !canonical.starts_with(base_dir) {
        return Err(AppError::InvalidPath);
    }
    Ok(canonical)
}

fn build_matcher(
    pattern: &str,
    regex: bool,
    case_sensitive: bool,
) -> Result<RegexMatcher, AppError> {
    RegexMatcherBuilder::new()
        .case_insensitive(!case_sensitive)
        .fixed_strings(!regex)
        .build(pattern)
        .map_err(|error| AppError::InvalidRegex(error.to_string()))
}

fn parse_changed_within(value: &str) -> Result<Option<SystemTime>, AppError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let duration = humantime::parse_duration(value)
        .map_err(|_| AppError::InvalidDuration(value.to_owned()))?;
    SystemTime::now()
        .checked_sub(duration)
        .map(Some)
        .ok_or_else(|| AppError::InvalidDuration(value.to_owned()))
}

fn stream_search_logs<F>(
    target: &Path,
    matcher: RegexMatcher,
    limit: usize,
    search_zip: bool,
    changed_after: Option<SystemTime>,
    cancelled: &AtomicBool,
    mut emit: F,
) -> Result<SearchOutcome, AppError>
where
    F: FnMut(SearchMatch) -> bool,
{
    let mut outcome = SearchOutcome::default();

    let mut walker = WalkBuilder::new(target);
    walker
        .hidden(false)
        .follow_links(false)
        .standard_filters(false)
        .threads(1);

    for entry in walker.build() {
        if cancelled.load(Ordering::Relaxed) {
            outcome.cancelled = true;
            break;
        }
        if outcome.count >= limit {
            outcome.truncated = true;
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warn!(%error, "跳过不可访问的日志路径");
                continue;
            }
        };
        let path = entry.path();
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if changed_after.is_some_and(|cutoff| {
            entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .is_none_or(|modified| modified <= cutoff)
        }) {
            continue;
        }

        if is_compressed_path(path) {
            if search_zip {
                if let Err(error) = search_compressed_file(
                    path,
                    &matcher,
                    limit,
                    cancelled,
                    &mut outcome,
                    &mut emit,
                ) {
                    warn!(path = %path.display(), %error, "跳过无法解压检索的文件");
                }
            }
            continue;
        }
        if let Ok(file) = File::open(path) {
            let display_path = path.to_string_lossy().into_owned();
            if let Err(error) = search_reader(
                file,
                display_path,
                &matcher,
                limit,
                cancelled,
                &mut outcome,
                &mut emit,
            ) {
                warn!(path = %path.display(), %error, "跳过无法检索的文件");
            }
        }
    }

    Ok(outcome)
}

fn is_compressed_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "zip" | "gz" | "bz2" | "xz" | "lzma" | "zst" | "zstd"
            )
        })
}

fn search_compressed_file<F>(
    path: &Path,
    matcher: &RegexMatcher,
    limit: usize,
    cancelled: &AtomicBool,
    outcome: &mut SearchOutcome,
    emit: &mut F,
) -> Result<(), AppError>
where
    F: FnMut(SearchMatch) -> bool,
{
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if extension == "zip" {
        return search_zip_archive(path, matcher, limit, cancelled, outcome, emit);
    }

    let file = File::open(path).map_err(|error| AppError::Search(error.to_string()))?;
    let reader: Box<dyn Read> = match extension.as_str() {
        "gz" => Box::new(flate2::read::GzDecoder::new(file)),
        "bz2" => Box::new(bzip2::read::BzDecoder::new(file)),
        "xz" | "lzma" => Box::new(xz2::read::XzDecoder::new(file)),
        "zst" | "zstd" => Box::new(
            zstd::stream::read::Decoder::new(file)
                .map_err(|error| AppError::Search(error.to_string()))?,
        ),
        _ => return Ok(()),
    };
    search_reader(
        reader.take(MAX_DECOMPRESSED_BYTES),
        path.to_string_lossy().into_owned(),
        matcher,
        limit,
        cancelled,
        outcome,
        emit,
    )
    .map_err(|error| AppError::Search(error.to_string()))
}

fn search_zip_archive<F>(
    path: &Path,
    matcher: &RegexMatcher,
    limit: usize,
    cancelled: &AtomicBool,
    outcome: &mut SearchOutcome,
    emit: &mut F,
) -> Result<(), AppError>
where
    F: FnMut(SearchMatch) -> bool,
{
    let file = File::open(path).map_err(|error| AppError::Search(error.to_string()))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|error| AppError::Search(error.to_string()))?;
    for index in 0..archive.len() {
        if cancelled.load(Ordering::Relaxed) || outcome.count >= limit {
            break;
        }
        let mut entry = match archive.by_index(index) {
            Ok(entry) => entry,
            Err(error) => {
                warn!(archive = %path.display(), index, %error, "跳过无法读取的 ZIP 成员");
                continue;
            }
        };
        if entry.is_dir() {
            continue;
        }
        let display_path = format!("{}!{}", path.display(), entry.name());
        search_reader(
            (&mut entry).take(MAX_DECOMPRESSED_BYTES),
            display_path,
            matcher,
            limit,
            cancelled,
            outcome,
            emit,
        )
        .map_err(|error| AppError::Search(error.to_string()))?;
    }
    Ok(())
}

fn search_reader<R, F>(
    reader: R,
    display_path: String,
    matcher: &RegexMatcher,
    limit: usize,
    cancelled: &AtomicBool,
    outcome: &mut SearchOutcome,
    emit: &mut F,
) -> std::io::Result<()>
where
    R: Read,
    F: FnMut(SearchMatch) -> bool,
{
    let sink_matcher = matcher.clone();
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();
    searcher.search_reader(
        matcher,
        reader,
        UTF8(|line_number, line: &str| {
            let mut submatches = Vec::new();
            let _ = sink_matcher.find_iter(line.as_bytes(), |matched| {
                submatches.push(Submatch {
                    start: matched.start(),
                    end: matched.end(),
                });
                true
            });
            if cancelled.load(Ordering::Relaxed) {
                outcome.cancelled = true;
                return Ok(false);
            }
            if outcome.count >= limit {
                outcome.truncated = true;
                return Ok(false);
            }
            if !emit(SearchMatch {
                path: display_path.clone(),
                line_number,
                line: line.trim_end_matches(['\r', '\n']).to_owned(),
                submatches,
            }) {
                cancelled.store(true, Ordering::Relaxed);
                outcome.cancelled = true;
                return Ok(false);
            }
            outcome.count += 1;
            if outcome.count >= limit {
                outcome.truncated = true;
                return Ok(false);
            }
            Ok(true)
        }),
    )
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("安装 Ctrl+C 处理器失败");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("安装 SIGTERM 处理器失败")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    info!("收到停止信号，正在优雅退出");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

    fn temp_fixture() -> PathBuf {
        let id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!("ripgrep-web-test-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(
            root.join("nested/app.log"),
            "INFO ready\nERROR failed request\nerror retry failed\n",
        )
        .unwrap();
        root.canonicalize().unwrap()
    }

    #[test]
    fn literal_search_returns_ranges() {
        let root = temp_fixture();
        let matcher = build_matcher("failed", false, true).unwrap();
        let mut found = Vec::new();
        let outcome = stream_search_logs(
            &root,
            matcher,
            20,
            false,
            None,
            &AtomicBool::new(false),
            |item| {
                found.push(item);
                true
            },
        )
        .unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(outcome.count, 2);
        assert_eq!(found[0].line_number, 2);
        assert_eq!(found[0].submatches, vec![Submatch { start: 6, end: 12 }]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn regex_and_case_insensitive_search_work() {
        let root = temp_fixture();
        let matcher = build_matcher("error|ready", true, false).unwrap();
        let mut found = Vec::new();
        let outcome = stream_search_logs(
            &root,
            matcher,
            20,
            false,
            None,
            &AtomicBool::new(false),
            |item| {
                found.push(item);
                true
            },
        )
        .unwrap();
        assert_eq!(outcome.count, 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn path_traversal_is_rejected() {
        let root = temp_fixture();
        assert!(matches!(
            resolve_target(&root, "../../etc"),
            Err(AppError::InvalidPath)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn result_limit_is_global() {
        let root = temp_fixture();
        let matcher = build_matcher("failed", false, false).unwrap();
        let mut found = Vec::new();
        let outcome = stream_search_logs(
            &root,
            matcher,
            1,
            false,
            None,
            &AtomicBool::new(false),
            |item| {
                found.push(item);
                true
            },
        )
        .unwrap();
        assert_eq!(outcome.count, 1);
        assert!(outcome.truncated);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn receiver_cancellation_stops_search() {
        let root = temp_fixture();
        let matcher = build_matcher("failed", false, false).unwrap();
        let cancelled = AtomicBool::new(false);
        let outcome =
            stream_search_logs(&root, matcher, 20, false, None, &cancelled, |_| false).unwrap();
        assert!(outcome.cancelled);
        assert_eq!(outcome.count, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn searches_files_inside_zip_when_enabled() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        let root = temp_fixture();
        let archive_path = root.join("archived-logs.zip");
        let file = File::create(&archive_path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file("logs/app.log", SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(b"INFO archived\nERROR zip-search-marker\n")
            .unwrap();
        archive.finish().unwrap();

        let matcher = build_matcher("zip-search-marker", false, true).unwrap();
        let mut found = Vec::new();
        let outcome = stream_search_logs(
            &root,
            matcher,
            20,
            true,
            None,
            &AtomicBool::new(false),
            |item| {
                found.push(item);
                true
            },
        )
        .unwrap();
        assert_eq!(outcome.count, 1);
        assert!(found[0].path.ends_with("archived-logs.zip!logs/app.log"));
        assert_eq!(found[0].line_number, 2);

        let matcher = build_matcher("zip-search-marker", false, true).unwrap();
        let disabled = stream_search_logs(
            &root,
            matcher,
            20,
            false,
            None,
            &AtomicBool::new(false),
            |_| true,
        )
        .unwrap();
        assert_eq!(disabled.count, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn searches_gzip_file_when_enabled() {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write;

        let root = temp_fixture();
        let file = File::create(root.join("app.log.gz")).unwrap();
        let mut encoder = GzEncoder::new(file, Compression::fast());
        encoder
            .write_all(b"INFO ready\nERROR gzip-marker\n")
            .unwrap();
        encoder.finish().unwrap();

        let matcher = build_matcher("gzip-marker", false, true).unwrap();
        let mut found = Vec::new();
        let outcome = stream_search_logs(
            &root,
            matcher,
            20,
            true,
            None,
            &AtomicBool::new(false),
            |item| {
                found.push(item);
                true
            },
        )
        .unwrap();
        assert_eq!(outcome.count, 1);
        assert!(found[0].path.ends_with("app.log.gz"));
        assert_eq!(found[0].line_number, 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_within_filters_old_files() {
        use std::fs::FileTimes;

        let root = temp_fixture();
        let old_path = root.join("old.log");
        fs::write(&old_path, "ERROR time-filter-marker\n").unwrap();
        let old_file = File::options().write(true).open(&old_path).unwrap();
        old_file
            .set_times(
                FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(48 * 60 * 60)),
            )
            .unwrap();
        fs::write(root.join("recent.log"), "ERROR time-filter-marker\n").unwrap();

        let matcher = build_matcher("time-filter-marker", false, true).unwrap();
        let cutoff = SystemTime::now() - Duration::from_secs(60 * 60);
        let mut found = Vec::new();
        let outcome = stream_search_logs(
            &root,
            matcher,
            20,
            false,
            Some(cutoff),
            &AtomicBool::new(false),
            |item| {
                found.push(item);
                true
            },
        )
        .unwrap();
        assert_eq!(outcome.count, 1);
        assert!(found[0].path.ends_with("recent.log"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_within_duration_parser_accepts_fd_style_values() {
        assert!(parse_changed_within("").unwrap().is_none());
        assert!(parse_changed_within("35min").unwrap().is_some());
        assert!(parse_changed_within("2weeks").unwrap().is_some());
        assert!(matches!(
            parse_changed_within("yesterday-ish"),
            Err(AppError::InvalidDuration(_))
        ));
    }
}
