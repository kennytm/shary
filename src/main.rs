use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, Query, State as AppState},
    http::{
        HeaderMap, StatusCode,
        header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::{IntoResponse, Sse, sse::Event},
    routing::{delete, get, post},
};
use futures_util::{Stream, TryStreamExt, stream};
use if_addrs::get_if_addrs;
use mime::{APPLICATION_OCTET_STREAM, Mime};
use qrcode::{EcLevel, QrCode, render::svg::Color};
use serde::{Deserialize, Serialize};
use std::{
    convert::Infallible,
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tempfile::tempdir;
use tokio::{
    fs::{File, remove_file},
    io::{AsyncWriteExt, copy},
    net::TcpListener,
    sync::{RwLock, watch},
};
use tokio_util::io::{ReaderStream, StreamReader};

xflags::xflags! {
    cmd shary {
        /// Listening address (default: 0.0.0.0:22888)
        optional -a, --address address: SocketAddr

        /// Maximum number of snippets to store (default: 8)
        optional -m, --max-snippets count: usize

        /// Print version information
        optional -V, --version
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum Snippet {
    Text {
        content: String,
    },
    Password {
        content: String,
    },
    File {
        id: usize,
        size: u64,
        name: String,
        mime: String,
    },
}

type SharedSnippets = RwLock<Vec<Snippet>>;

struct State {
    address: SocketAddr,
    max_snippets: usize,
    snippets: SharedSnippets,
    upload_dir: PathBuf,
    upload_counter: AtomicUsize,
    snippets_updated: watch::Sender<()>,
}

impl State {
    fn upload_path(&self, id: usize) -> PathBuf {
        self.upload_dir.join(format!("{}", id))
    }

    async fn push_snippet(&self, snippet: Snippet) {
        let mut snippets = self.snippets.write().await;
        snippets.push(snippet);
        let evicted = (snippets.len() > self.max_snippets).then(|| snippets.remove(0));
        drop(snippets);

        if let Some(snippet) = evicted {
            self.delete_snippet(snippet).await;
        } else {
            self.snippets_updated.send_replace(());
        }
    }

    async fn delete_snippet(&self, snippet: Snippet) {
        if let Snippet::File { id, .. } = snippet {
            let path = self.upload_path(id);
            let _ = remove_file(path).await;
        }
        self.snippets_updated.send_replace(());
    }
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let flags = Shary::from_env_or_exit();
    if flags.version {
        println!("shary {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let upload_dir = tempdir().expect("cannot prepare upload directory");
    let listener = TcpListener::bind(
        flags
            .address
            .unwrap_or_else(|| ([0, 0, 0, 0], 22888).into()),
    )
    .await?;
    let listen_address = listener.local_addr()?;

    println!("Files will be uploaded to {}", upload_dir.path().display());
    if let Ok(Some(real_address)) =
        get_server_addresses(listen_address).map(|v| v.into_iter().next())
    {
        let url = format!("http://{}/", real_address);
        let qrcode = QrCode::with_error_correction_level(&url, EcLevel::L)
            .expect("failed to generate QR code")
            .render::<char>()
            .build();
        println!("Visit <{}>\n{}", url, qrcode);
    }

    let state = Arc::new(State {
        address: listen_address,
        max_snippets: flags.max_snippets.unwrap_or(8),
        snippets: SharedSnippets::default(),
        upload_dir: upload_dir.path().to_owned(),
        upload_counter: AtomicUsize::new(0),
        snippets_updated: watch::channel(()).0,
    });
    let app = Router::new()
        .route("/", get(respond_index))
        .route(
            "/snippets",
            get(respond_get_snippets)
                .post(respond_post_snippets)
                .layer(DefaultBodyLimit::disable()),
        )
        .route("/snippets/{i}", delete(respond_delete_snippet))
        .route("/upload", post(respond_upload))
        .route("/download/{i}", get(respond_download))
        .route("/ipaddrs", get(respond_ipaddrs))
        .route("/qrcode", get(respond_qrcode))
        .route("/updated", get(snippet_update_monitor))
        .with_state(state);

    tokio::select! {
        result = axum::serve(listener, app) => result,
        result = tokio::signal::ctrl_c() => result,
    }
}

fn internal_error(error: impl std::fmt::Display) -> StatusCode {
    eprintln!("{error}");
    StatusCode::INTERNAL_SERVER_ERROR
}

#[cfg(feature = "read_index_html_from_file_system")]
async fn respond_index() -> Result<impl IntoResponse, StatusCode> {
    let mut path = PathBuf::from(file!());
    path.set_file_name("web");
    path.push("index.html");
    let content = tokio::fs::read(path).await.map_err(internal_error)?;
    Ok(([(CONTENT_TYPE, "text/html; charset=utf-8")], content))
}

#[cfg(not(feature = "read_index_html_from_file_system"))]
async fn respond_index() -> impl IntoResponse {
    let content = &include_bytes!("web/index.min.html.gz")[..];
    (
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (axum::http::header::CONTENT_ENCODING, "gzip"),
        ],
        content,
    )
}

async fn respond_get_snippets(AppState(state): AppState<Arc<State>>) -> impl IntoResponse {
    let snippets = state.snippets.read().await;
    Json(&*snippets).into_response()
}

async fn respond_post_snippets(
    AppState(state): AppState<Arc<State>>,
    body: Bytes,
) -> Result<StatusCode, (StatusCode, &'static str)> {
    // The browser sends JSON with a text/plain content type.
    let snippet =
        serde_json::from_slice(&body).map_err(|_| (StatusCode::UNPROCESSABLE_ENTITY, ""))?;
    if let Snippet::File { .. } = snippet {
        return Err((
            StatusCode::BAD_REQUEST,
            "cannot upload a file using POST /snippets",
        ));
    }

    state.push_snippet(snippet).await;
    Ok(StatusCode::OK)
}

async fn respond_delete_snippet(
    AppState(state): AppState<Arc<State>>,
    Path(i): Path<usize>,
) -> StatusCode {
    let mut snippets = state.snippets.write().await;
    if i < snippets.len() {
        let snippet = snippets.remove(i);
        state.delete_snippet(snippet).await;
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

#[derive(Deserialize)]
struct UploadQuery {
    f: String,
}

async fn respond_upload(
    AppState(state): AppState<Arc<State>>,
    Query(UploadQuery { f: name }): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, StatusCode> {
    let id = state.upload_counter.fetch_add(1, Ordering::Relaxed);
    let path = state.upload_path(id);
    let mut f = File::create(&path).await.map_err(internal_error)?;
    let mut reader = StreamReader::new(body.into_data_stream().map_err(io::Error::other));
    let size_result = async {
        let size = copy(&mut reader, &mut f).await?;
        f.flush().await?;
        Ok::<_, io::Error>(size)
    }
    .await;
    drop(f);
    let size = match size_result {
        Ok(size) => size,
        Err(e) => {
            let _ = remove_file(path).await;
            return Err(internal_error(e));
        }
    };

    let mime = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Mime>().ok())
        .unwrap_or(APPLICATION_OCTET_STREAM)
        .to_string();
    let snippet = Snippet::File {
        id,
        size,
        name,
        mime,
    };
    state.push_snippet(snippet).await;
    Ok(StatusCode::OK)
}

async fn respond_download(
    AppState(state): AppState<Arc<State>>,
    Path(i): Path<usize>,
) -> Result<impl IntoResponse, StatusCode> {
    let snippets = state.snippets.read().await;
    let Some(Snippet::File {
        id,
        size,
        name,
        mime,
    }) = snippets.get(i)
    else {
        return Err(StatusCode::NOT_FOUND);
    };
    let file = File::open(state.upload_path(*id))
        .await
        .map_err(internal_error)?;
    let headers = [
        (CONTENT_LENGTH, size.to_string()),
        (CONTENT_TYPE, mime.clone()),
        (
            CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", name.replace('"', "\\\"")),
        ),
    ];
    Ok((headers, Body::from_stream(ReaderStream::new(file))))
}

fn get_server_addresses(address: SocketAddr) -> io::Result<Vec<String>> {
    Ok(if address.ip().is_unspecified() {
        let port = address.port();
        let host = hostname::get()
            .ok()
            .and_then(|hn| hn.into_string().ok())
            .map(|hn| format!("{}.local:{}", hn, port));
        let ips = get_if_addrs()?
            .into_iter()
            .map(|intf| SocketAddr::new(intf.ip(), port).to_string());
        host.into_iter().chain(ips).collect()
    } else {
        vec![address.to_string()]
    })
}

async fn respond_ipaddrs(
    AppState(state): AppState<Arc<State>>,
) -> Result<Json<Vec<String>>, StatusCode> {
    get_server_addresses(state.address)
        .map(Json)
        .map_err(internal_error)
}

#[derive(Deserialize)]
struct QrCodeQuery {
    s: String,
}

async fn respond_qrcode(
    Query(QrCodeQuery { s: input }): Query<QrCodeQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    let image = QrCode::with_error_correction_level(input, EcLevel::L)
        .map_err(internal_error)?
        .render()
        .dark_color(Color("#eee"))
        .light_color(Color("transparent"))
        .quiet_zone(false)
        .build();
    Ok(([(CONTENT_TYPE, "image/svg+xml")], image))
}

async fn snippet_update_monitor(
    AppState(state): AppState<Arc<State>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let updates = state.snippets_updated.subscribe();
    Sse::new(stream::unfold(updates, |mut updates| async {
        updates.changed().await.ok()?;
        let event = Event::default().event("updated").data("1");
        Some((Ok(event), updates))
    }))
}
