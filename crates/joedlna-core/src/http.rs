use std::collections::HashMap;
use std::future::IntoFuture;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, RwLock};

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path as AxumPath, State};
use axum::http::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, SeekFrom};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinError;
use tokio::time::Instant;
use tokio_util::io::ReaderStream;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::bookmarks::{BookmarkError, BookmarkStore};
use crate::catalog::{Catalog, EntryKind, ScanError};
use crate::config::{Config, ConfigError};
use crate::description::{
    CONNECTION_MANAGER_SCPD, CONTENT_DIRECTORY_SCPD, device_description, presentation_page,
};
use crate::events::{EventManager, EventService, SubscriptionError};
use crate::platform::SERVER as SERVER_VALUE;
use crate::soap::{
    connection_manager_response, content_directory_response, parse_request, protocol_info,
    soap_fault,
};
use crate::ssdp::{SsdpError, run_ssdp};

const XML_CONTENT_TYPE: &str = "text/xml; charset=\"utf-8\"";
const STREAM_BUFFER_BYTES: usize = 64 * 1_024;
const GET_CONTENT_FEATURES: HeaderName = HeaderName::from_static("getcontentfeatures.dlna.org");
const CONTENT_FEATURES: HeaderName = HeaderName::from_static("contentfeatures.dlna.org");
const TRANSFER_MODE: HeaderName = HeaderName::from_static("transfermode.dlna.org");
const EXT: HeaderName = HeaderName::from_static("ext");
const SERVER: HeaderName = HeaderName::from_static("server");

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("failed to scan media: {0}")]
    Scan(#[from] ScanError),
    #[error("media scan task failed: {0}")]
    ScanTask(#[from] JoinError),
    #[error("failed to bind HTTP listener: {0}")]
    BindHttp(#[source] io::Error),
    #[error("HTTP server failed: {0}")]
    Http(#[source] io::Error),
    #[error("SSDP service failed: {0}")]
    Ssdp(#[from] SsdpError),
    #[error("bookmark state failed: {0}")]
    Bookmarks(#[from] BookmarkError),
    #[error("no usable IPv4 address was found; set network.interface")]
    NoInterface,
}

#[derive(Debug, Clone)]
struct AppState {
    catalog: Arc<RwLock<Arc<Catalog>>>,
    config: Arc<RwLock<Config>>,
    base_url: Arc<str>,
    udn: Arc<str>,
    events: EventManager,
    bookmarks: BookmarkStore,
}

pub async fn serve(config_path: PathBuf, config: Config) -> Result<(), ServerError> {
    let interface = match config.network.interface {
        Some(interface) => interface,
        None => detect_local_ipv4().await.ok_or(ServerError::NoInterface)?,
    };
    let first_config = config.clone();
    let first_scan =
        tokio::task::spawn_blocking(move || Catalog::scan_settled(&first_config)).await??;
    let catalog = first_scan.catalog;
    info!(
        files = catalog.file_count(),
        bytes = catalog.total_bytes(),
        "media catalog ready"
    );
    if first_scan.unsettled_files > 0 {
        info!(
            files = first_scan.unsettled_files,
            settle_seconds = config.scanner.settle_time_seconds,
            "withholding recently modified media until it settles"
        );
    }

    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, config.network.http_port))
        .await
        .map_err(ServerError::BindHttp)?;
    let http_port = listener.local_addr().map_err(ServerError::BindHttp)?.port();
    let base_url: Arc<str> = format!("http://{interface}:{http_port}").into();
    let udn: Arc<str> = stable_udn(&config_path).into();
    let bookmark_path = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("bookmarks.toml");
    let bookmarks = BookmarkStore::load(bookmark_path).await?;
    let shared_catalog = Arc::new(RwLock::new(Arc::new(catalog)));
    let shared_config = Arc::new(RwLock::new(config.clone()));
    let events = EventManager::new(interface);
    let state = AppState {
        catalog: shared_catalog.clone(),
        config: shared_config.clone(),
        base_url: base_url.clone(),
        udn: udn.clone(),
        events: events.clone(),
        bookmarks,
    };
    let app = router(state);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let scanner = tokio::spawn(run_scanner(
        config_path,
        shared_config,
        shared_catalog,
        events,
        first_scan.unsettled_files,
        shutdown_rx.clone(),
    ));
    let mut ssdp = tokio::spawn(run_ssdp(
        interface,
        http_port,
        udn.to_string(),
        config.network.ssdp_max_age_seconds,
        shutdown_rx.clone(),
    ));
    info!(%interface, http_port, %udn, "JoeDLNA ready");

    let http = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(shutdown_rx))
    .into_future();
    tokio::pin!(http);
    let result = tokio::select! {
        http_result = &mut http => {
            let _ = shutdown_tx.send(true);
            let ssdp_result = ssdp.await;
            map_runtime_results(http_result, ssdp_result)
        }
        ssdp_result = &mut ssdp => {
            let _ = shutdown_tx.send(true);
            let http_result = http.await;
            map_runtime_results(http_result, ssdp_result)
        }
    };
    scanner.abort();
    result
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(presentation))
        .route("/root.xml", get(root_description))
        .route("/upnp/content-directory.xml", get(content_directory_scpd))
        .route("/upnp/connection-manager.xml", get(connection_manager_scpd))
        .route(
            "/upnp/content-directory/control",
            post(content_directory_control),
        )
        .route(
            "/upnp/connection-manager/control",
            post(connection_manager_control),
        )
        .route(
            "/upnp/content-directory/events",
            axum::routing::any(content_directory_events),
        )
        .route(
            "/upnp/connection-manager/events",
            axum::routing::any(connection_manager_events),
        )
        .route("/media/{id}/{name}", get(media).head(media))
        .with_state(state)
}

async fn presentation(State(state): State<AppState>) -> Response<Body> {
    let catalog = read_catalog(&state);
    let friendly_name = state.config.read().unwrap().friendly_name.clone();
    response(
        StatusCode::OK,
        "text/html; charset=utf-8",
        presentation_page(&friendly_name, catalog.file_count(), catalog.total_bytes()),
    )
}

async fn root_description(State(state): State<AppState>) -> Response<Body> {
    let friendly_name = state.config.read().unwrap().friendly_name.clone();
    xml_response(
        StatusCode::OK,
        device_description(&friendly_name, &state.udn, &state.base_url),
    )
}

async fn content_directory_scpd() -> Response<Body> {
    xml_response(StatusCode::OK, CONTENT_DIRECTORY_SCPD.into())
}

async fn connection_manager_scpd() -> Response<Body> {
    xml_response(StatusCode::OK, CONNECTION_MANAGER_SCPD.into())
}

async fn content_directory_control(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response<Body> {
    let request = parse_request(header(&headers, "soapaction"), &body);
    let (action, arguments) = match request {
        Ok(request) => request,
        Err(_) => {
            return soap_error(crate::soap::UpnpError {
                code: 402,
                description: "Invalid Args",
            });
        }
    };
    debug!(
        %action,
        object_id = arguments.get("ObjectID").map(String::as_str),
        browse_flag = arguments.get("BrowseFlag").map(String::as_str),
        "ContentDirectory action"
    );
    let catalog = read_catalog(&state);
    if action == "X_SetBookmark" {
        return set_bookmark(&state, &arguments, &catalog).await;
    }
    let bookmarks = state.bookmarks.snapshot().await;
    match content_directory_response(&action, &arguments, &catalog, &bookmarks, &state.base_url) {
        Ok(body) => soap_success(body),
        Err(error) => soap_error(error),
    }
}

async fn set_bookmark(
    state: &AppState,
    arguments: &std::collections::HashMap<String, String>,
    catalog: &Catalog,
) -> Response<Body> {
    let Some(object_id) = arguments.get("ObjectID") else {
        return soap_error(crate::soap::UpnpError {
            code: 402,
            description: "Invalid Args",
        });
    };
    let Some(position) = arguments
        .get("PosSecond")
        .and_then(|position| position.parse::<u64>().ok())
    else {
        return soap_error(crate::soap::UpnpError {
            code: 402,
            description: "Invalid Args",
        });
    };
    if !matches!(
        catalog.entry(object_id).map(|entry| &entry.kind),
        Some(EntryKind::Media(_))
    ) {
        return soap_error(crate::soap::UpnpError {
            code: 701,
            description: "No Such Object",
        });
    }
    if let Err(error) = state.bookmarks.set(object_id.clone(), position).await {
        warn!(%error, %object_id, "failed to persist Samsung bookmark");
        return soap_error(crate::soap::UpnpError {
            code: 501,
            description: "Action Failed",
        });
    }
    debug!(%object_id, position, "Samsung bookmark updated");
    match content_directory_response(
        "X_SetBookmark",
        arguments,
        catalog,
        &state.bookmarks.snapshot().await,
        &state.base_url,
    ) {
        Ok(body) => soap_success(body),
        Err(error) => soap_error(error),
    }
}

async fn connection_manager_control(headers: HeaderMap, body: axum::body::Bytes) -> Response<Body> {
    let request = parse_request(header(&headers, "soapaction"), &body);
    let (action, arguments) = match request {
        Ok(request) => request,
        Err(_) => {
            return soap_error(crate::soap::UpnpError {
                code: 402,
                description: "Invalid Args",
            });
        }
    };
    debug!(
        %action,
        connection_id = arguments.get("ConnectionID").map(String::as_str),
        "ConnectionManager action"
    );
    match connection_manager_response(&action, &arguments) {
        Ok(body) => soap_success(body),
        Err(error) => soap_error(error),
    }
}

async fn content_directory_events(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response<Body> {
    event_request(state, EventService::ContentDirectory, peer, request).await
}

async fn connection_manager_events(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request<Body>,
) -> Response<Body> {
    event_request(state, EventService::ConnectionManager, peer, request).await
}

async fn event_request(
    state: AppState,
    service: EventService,
    peer: SocketAddr,
    request: Request<Body>,
) -> Response<Body> {
    match request.method().as_str() {
        "SUBSCRIBE" => match peer.ip() {
            IpAddr::V4(requester) => match state
                .events
                .subscribe(service, request.headers(), requester)
                .await
            {
                Ok(subscription) => {
                    let mut response = Response::builder()
                        .status(StatusCode::OK)
                        .header(SERVER, SERVER_VALUE)
                        .header(CONTENT_LENGTH, 0)
                        .header("sid", &subscription.sid)
                        .header(
                            "timeout",
                            format!("Second-{}", subscription.timeout_seconds),
                        )
                        .body(Body::empty())
                        .unwrap();
                    if subscription.is_new {
                        let events = state.events.clone();
                        let sid = subscription.sid;
                        let update_id = read_catalog(&state).update_id();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            events.notify_initial(&sid, update_id).await;
                        });
                    }
                    response
                        .headers_mut()
                        .insert(EXT, HeaderValue::from_static(""));
                    response
                }
                Err(error) => event_error(error),
            },
            IpAddr::V6(_) => StatusCode::PRECONDITION_FAILED.into_response(),
        },
        "UNSUBSCRIBE" => match state.events.unsubscribe(service, request.headers()).await {
            Ok(()) => Response::builder()
                .status(StatusCode::OK)
                .header(SERVER, SERVER_VALUE)
                .header(CONTENT_LENGTH, 0)
                .body(Body::empty())
                .unwrap(),
            Err(error) => event_error(error),
        },
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

fn event_error(error: SubscriptionError) -> Response<Body> {
    let status = match error {
        SubscriptionError::IncompatibleHeaders => StatusCode::BAD_REQUEST,
        SubscriptionError::InvalidCallback
        | SubscriptionError::UnsafeCallback
        | SubscriptionError::InvalidNt
        | SubscriptionError::InvalidSid => StatusCode::PRECONDITION_FAILED,
    };
    Response::builder()
        .status(status)
        .header(SERVER, SERVER_VALUE)
        .header(CONTENT_LENGTH, 0)
        .body(Body::empty())
        .unwrap()
}

async fn media(
    State(state): State<AppState>,
    AxumPath((id, _name)): AxumPath<(String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response<Body> {
    let entry = match read_catalog(&state).entry(&id).cloned() {
        Some(entry) => entry,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let media_kind = match entry.kind {
        EntryKind::Media(media_kind) => media_kind,
        EntryKind::Container => return StatusCode::NOT_FOUND.into_response(),
    };
    let metadata = match tokio::fs::metadata(&entry.path).await {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let file_size = metadata.len();
    let range = match parse_range(
        headers.get(RANGE).and_then(|value| value.to_str().ok()),
        file_size,
    ) {
        Ok(range) => range,
        Err(()) => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(CONTENT_RANGE, format!("bytes */{file_size}"))
                .header(CONTENT_LENGTH, 0)
                .body(Body::empty())
                .unwrap();
        }
    };
    let length = if file_size == 0 {
        0
    } else {
        range.end.saturating_sub(range.start).saturating_add(1)
    };
    let content_features = protocol_info(media_kind.mime_type, media_kind.dlna_profile)
        .split_once(':')
        .and_then(|(_, value)| value.split_once(':'))
        .and_then(|(_, value)| value.split_once(':'))
        .map_or("*", |(_, features)| features)
        .to_owned();

    let body = if method == Method::HEAD || length == 0 {
        Body::empty()
    } else {
        let mut file = match File::open(&entry.path).await {
            Ok(file) => file,
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        };
        if range.start > 0 && file.seek(SeekFrom::Start(range.start)).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Body::from_stream(ReaderStream::with_capacity(
            tokio::io::AsyncReadExt::take(file, length),
            STREAM_BUFFER_BYTES,
        ))
    };
    let mut response = Response::builder()
        .status(if range.partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(CONTENT_TYPE, media_kind.mime_type)
        .header(CONTENT_LENGTH, length)
        .header(ACCEPT_RANGES, "bytes")
        .header(SERVER, SERVER_VALUE)
        .header(EXT, "")
        .header(TRANSFER_MODE, "Streaming");
    if range.partial {
        response = response.header(
            CONTENT_RANGE,
            format!("bytes {}-{}/{file_size}", range.start, range.end),
        );
    }
    if headers
        .get(GET_CONTENT_FEATURES)
        .is_some_and(|value| value == "1")
    {
        response = response.header(CONTENT_FEATURES, content_features);
    }
    debug!(
        object_id = %id,
        path = %entry.path.display(),
        mime_type = media_kind.mime_type,
        range_start = range.start,
        range_end = range.end,
        partial = range.partial,
        "serving media"
    );
    response.body(body).unwrap()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    end: u64,
    partial: bool,
}

fn parse_range(value: Option<&str>, file_size: u64) -> Result<ByteRange, ()> {
    let Some(value) = value else {
        return Ok(ByteRange {
            start: 0,
            end: file_size.saturating_sub(1),
            partial: false,
        });
    };
    if file_size == 0 {
        return Err(());
    }
    let range = value.strip_prefix("bytes=").ok_or(())?;
    if range.contains(',') {
        return Err(());
    }
    let (start, end) = range.split_once('-').ok_or(())?;
    let (start, end) = if start.is_empty() {
        let suffix: u64 = end.parse().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        (file_size.saturating_sub(suffix), file_size - 1)
    } else {
        let start: u64 = start.parse().map_err(|_| ())?;
        let end = if end.is_empty() {
            file_size - 1
        } else {
            end.parse().map_err(|_| ())?
        };
        (start, end.min(file_size - 1))
    };
    if start >= file_size || start > end {
        return Err(());
    }
    Ok(ByteRange {
        start,
        end,
        partial: true,
    })
}

async fn run_scanner(
    config_path: PathBuf,
    shared_config: Arc<RwLock<Config>>,
    shared_catalog: Arc<RwLock<Arc<Catalog>>>,
    events: EventManager,
    initial_unsettled_files: usize,
    mut shutdown: watch::Receiver<bool>,
) {
    let (watch_tx, mut watch_rx) = mpsc::channel(64);
    let mut watcher = match notify::recommended_watcher(move |event| {
        let _ = watch_tx.try_send(event);
    }) {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            warn!(%error, "native filesystem watcher unavailable; using periodic rescans");
            None
        }
    };
    let canonical_config_path = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.clone());
    let mut watched_shares = HashMap::new();
    if let Some(watcher) = watcher.as_mut() {
        if let Some(parent) = canonical_config_path.parent()
            && let Err(error) = watcher.watch(parent, RecursiveMode::NonRecursive)
        {
            warn!(path = %parent.display(), %error, "failed to watch config directory");
        }
        reconcile_share_watches(watcher, &shared_config.read().unwrap(), &mut watched_shares);
    }

    let mut fallback_deadline = Instant::now() + shared_config.read().unwrap().rescan_interval();
    let mut settle_deadline = (initial_unsettled_files > 0)
        .then(|| Instant::now() + shared_config.read().unwrap().settle_time());
    loop {
        // While writes keep arriving, the settle deadline supersedes the fallback deadline. This
        // prevents the fallback poll from publishing a file in the middle of a long encode.
        let next_scan = settle_deadline.unwrap_or(fallback_deadline);
        tokio::select! {
            _ = tokio::time::sleep_until(next_scan) => {
                let trigger = if settle_deadline.is_some() {
                    "filesystem settled"
                } else {
                    "periodic fallback"
                };
                debug!(trigger, "rescanning media catalog");
                let outcome = rescan_catalog(
                    &config_path,
                    &shared_config,
                    &shared_catalog,
                    &events,
                ).await;
                if let Some(outcome) = &outcome
                    && let Some(watcher) = watcher.as_mut()
                {
                    reconcile_share_watches(watcher, &outcome.config, &mut watched_shares);
                }
                settle_deadline = outcome
                    .filter(|outcome| outcome.unsettled_files > 0)
                    .map(|outcome| Instant::now() + outcome.config.settle_time());
                fallback_deadline =
                    Instant::now() + shared_config.read().unwrap().rescan_interval();
            }
            message = watch_rx.recv(), if watcher.is_some() => {
                match message {
                    Some(Ok(event)) if event_is_relevant(
                        &event,
                        &canonical_config_path,
                        watched_shares.keys(),
                    ) => {
                        let settle_time = shared_config.read().unwrap().settle_time();
                        settle_deadline = Some(Instant::now() + settle_time);
                        debug!(
                            kind = ?event.kind,
                            paths = event.paths.len(),
                            settle_seconds = settle_time.as_secs(),
                            "filesystem change queued"
                        );
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        warn!(%error, "filesystem watcher reported an error; scheduling a rescan");
                        settle_deadline = Some(
                            Instant::now() + shared_config.read().unwrap().settle_time()
                        );
                    }
                    None => {
                        warn!("filesystem watcher stopped; using periodic rescans");
                        watcher = None;
                    }
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
        }
    }
}

#[derive(Debug)]
struct RescanOutcome {
    config: Config,
    unsettled_files: usize,
}

#[derive(Debug, Error)]
enum ReloadError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Scan(#[from] ScanError),
}

async fn rescan_catalog(
    config_path: &Path,
    shared_config: &RwLock<Config>,
    shared_catalog: &RwLock<Arc<Catalog>>,
    events: &EventManager,
) -> Option<RescanOutcome> {
    let config_path = config_path.to_owned();
    let reloaded = tokio::task::spawn_blocking(move || {
        let config = Config::load(&config_path)?;
        let scan = Catalog::scan_settled(&config)?;
        Ok::<_, ReloadError>((config, scan))
    })
    .await;
    let (new_config, scan) = match reloaded {
        Ok(Ok(reloaded)) => reloaded,
        Ok(Err(error)) => {
            warn!(%error, "config reload or rescan rejected; keeping current catalog");
            return None;
        }
        Err(error) => {
            warn!(%error, "rescan task failed; keeping current catalog");
            return None;
        }
    };
    if shared_config.read().unwrap().network != new_config.network {
        warn!(
            "network configuration changed; interface, port, and SSDP lifetime changes require a restart"
        );
    }
    let new_catalog = scan.catalog;
    let current_catalog = read_catalog_raw(shared_catalog);
    if !current_catalog.has_same_content(&new_catalog) {
        let update_id = new_catalog.update_id();
        info!(
            update_id,
            files = new_catalog.file_count(),
            bytes = new_catalog.total_bytes(),
            "media catalog changed"
        );
        *shared_catalog.write().unwrap() = Arc::new(new_catalog);
        events.notify_content_changed(update_id).await;
    }
    *shared_config.write().unwrap() = new_config.clone();
    if scan.unsettled_files > 0 {
        debug!(
            files = scan.unsettled_files,
            settle_seconds = new_config.scanner.settle_time_seconds,
            "recently modified media remains withheld"
        );
    }
    Some(RescanOutcome {
        config: new_config,
        unsettled_files: scan.unsettled_files,
    })
}

fn reconcile_share_watches(
    watcher: &mut RecommendedWatcher,
    config: &Config,
    watched: &mut HashMap<PathBuf, RecursiveMode>,
) {
    let desired = if config.scanner.watch_filesystem {
        config
            .shares
            .iter()
            .filter_map(|share| share.path.canonicalize().ok())
            .map(|path| {
                let mode = if config.scanner.recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                };
                (path, mode)
            })
            .collect::<HashMap<_, _>>()
    } else {
        HashMap::new()
    };

    let stale = watched
        .iter()
        .filter(|(path, mode)| desired.get(*path) != Some(*mode))
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    for path in stale {
        if let Err(error) = watcher.unwatch(&path) {
            warn!(path = %path.display(), %error, "failed to remove filesystem watch");
        }
        watched.remove(&path);
    }
    for (path, mode) in desired {
        if watched.get(&path) == Some(&mode) {
            continue;
        }
        match watcher.watch(&path, mode) {
            Ok(()) => {
                info!(path = %path.display(), recursive = mode == RecursiveMode::Recursive, "watching media share");
                watched.insert(path, mode);
            }
            Err(error) => {
                warn!(path = %path.display(), %error, "failed to watch media share; fallback rescan remains active");
            }
        }
    }
}

fn event_is_relevant<'a>(
    event: &Event,
    config_path: &Path,
    share_roots: impl Iterator<Item = &'a PathBuf>,
) -> bool {
    if event.paths.is_empty() {
        return true;
    }
    let share_roots = share_roots.collect::<Vec<_>>();
    event.paths.iter().any(|path| {
        path == config_path
            || path.file_name() == config_path.file_name()
            || share_roots.iter().any(|root| path.starts_with(root))
    })
}

async fn shutdown_signal(mut shutdown: watch::Receiver<bool>) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = wait_for_shutdown(&mut shutdown) => {}
    }
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
}

fn read_catalog(state: &AppState) -> Arc<Catalog> {
    read_catalog_raw(&state.catalog)
}

fn read_catalog_raw(catalog: &RwLock<Arc<Catalog>>) -> Arc<Catalog> {
    catalog.read().unwrap().clone()
}

fn stable_udn(config_path: &Path) -> String {
    let absolute = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_owned());
    let mut identity = host_identity();
    identity.extend_from_slice(absolute.as_os_str().as_encoded_bytes());
    let uuid = Uuid::new_v5(&Uuid::NAMESPACE_URL, &identity);
    format!("uuid:{uuid}")
}

fn host_identity() -> Vec<u8> {
    if let Some(hostname) = Command::new("hostname")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| output.stdout)
    {
        return hostname;
    }
    for name in ["HOSTNAME", "COMPUTERNAME"] {
        if let Some(value) = std::env::var_os(name) {
            return value.as_encoded_bytes().to_vec();
        }
    }
    Vec::new()
}

fn map_runtime_results(
    http_result: io::Result<()>,
    ssdp_result: Result<Result<(), SsdpError>, JoinError>,
) -> Result<(), ServerError> {
    match ssdp_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error.into()),
        Err(error) if error.is_cancelled() => {}
        Err(error) => return Err(ServerError::ScanTask(error)),
    }
    http_result.map_err(ServerError::Http)
}

async fn detect_local_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok()?;
    socket
        .connect((Ipv4Addr::new(192, 0, 2, 1), 9))
        .await
        .ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(address) if !address.is_loopback() => Some(address),
        _ => None,
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn soap_success(body: String) -> Response<Body> {
    let mut response = xml_response(StatusCode::OK, body);
    response
        .headers_mut()
        .insert(EXT, HeaderValue::from_static(""));
    response
}

fn soap_error(error: crate::soap::UpnpError) -> Response<Body> {
    let mut response = xml_response(StatusCode::INTERNAL_SERVER_ERROR, soap_fault(error));
    response
        .headers_mut()
        .insert(EXT, HeaderValue::from_static(""));
    response
}

fn xml_response(status: StatusCode, body: String) -> Response<Body> {
    response(status, XML_CONTENT_TYPE, body)
}

fn response(status: StatusCode, content_type: &'static str, body: String) -> Response<Body> {
    let content_length = body.len();
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .header(CONTENT_LENGTH, content_length)
        .header(SERVER, SERVER_VALUE)
        .body(Body::from(body))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use notify::EventKind;
    use test_case::test_case;

    use super::*;

    #[test_case(None, 10, Ok(ByteRange { start: 0, end: 9, partial: false }) ; "whole file")]
    #[test_case(Some("bytes=2-5"), 10, Ok(ByteRange { start: 2, end: 5, partial: true }) ; "bounded")]
    #[test_case(Some("bytes=7-"), 10, Ok(ByteRange { start: 7, end: 9, partial: true }) ; "open end")]
    #[test_case(Some("bytes=-3"), 10, Ok(ByteRange { start: 7, end: 9, partial: true }) ; "suffix")]
    #[test_case(Some("bytes=12-"), 10, Err(()) ; "past end")]
    #[test_case(Some("bytes=1-2,4-5"), 10, Err(()) ; "multiple unsupported")]
    fn parses_single_http_range(value: Option<&str>, size: u64, expected: Result<ByteRange, ()>) {
        assert_eq!(parse_range(value, size), expected);
    }

    #[test]
    fn filters_filesystem_events_to_config_and_shares() {
        let config = PathBuf::from("/config/joedlna.toml");
        let roots = [PathBuf::from("/media/movies")];
        let media_event =
            Event::new(EventKind::Any).add_path(PathBuf::from("/media/movies/new/movie.mp4"));
        let config_event =
            Event::new(EventKind::Any).add_path(PathBuf::from("/config/joedlna.toml"));
        let unrelated = Event::new(EventKind::Any).add_path(PathBuf::from("/tmp/unrelated.txt"));

        assert!(event_is_relevant(&media_event, &config, roots.iter()));
        assert!(event_is_relevant(&config_event, &config, roots.iter()));
        assert!(!event_is_relevant(&unrelated, &config, roots.iter()));
        assert!(event_is_relevant(
            &Event::new(EventKind::Any),
            &config,
            roots.iter()
        ));
    }
}
