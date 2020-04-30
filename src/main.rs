use async_ctrlc::CtrlC;
use async_std::{
    fs::{remove_file, File},
    io::{copy, BufReader, Cursor},
    prelude::FutureExt as _,
    sync::RwLock,
    task::block_on,
};
use get_if_addrs::get_if_addrs;
use mime::{APPLICATION_OCTET_STREAM, IMAGE_SVG, TEXT_HTML_UTF_8};
use qrcode::{render::svg::Color, EcLevel, QrCode};
use serde::{Deserialize, Serialize};
use std::{
    error::Error,
    io::{self, ErrorKind},
    mem::swap,
    net::SocketAddr,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
use structopt::StructOpt;
use tempfile::tempdir;
use tide::{
    self,
    http::{headers::CONTENT_TYPE, StatusCode},
    Request, Response,
};

#[derive(StructOpt)]
struct Opt {
    /// Listening address
    #[structopt(short, long, default_value = "0.0.0.0:22888")]
    address: SocketAddr,

    /// Maximum number of snippets to store
    #[structopt(short, long, default_value = "8")]
    max_snippets: usize,
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
    opt: Opt,
    snippets: SharedSnippets,
    upload_dir: PathBuf,
    upload_counter: AtomicUsize,
}

impl State {
    fn upload_path(&self, id: usize) -> PathBuf {
        self.upload_dir.join(format!("{}", id))
    }

    async fn push_snippet(&self, mut snippet: Snippet) {
        {
            let mut snippets = self.snippets.write().await;

            if snippets.len() < self.opt.max_snippets {
                snippets.push(snippet);
                return;
            }
            swap(&mut snippets[0], &mut snippet);
            snippets.rotate_left(1);
        }
        self.delete_snippet(snippet).await;
    }

    async fn delete_snippet(&self, snippet: Snippet) {
        if let Snippet::File { id, .. } = snippet {
            let path = self.upload_path(id);
            let _ = remove_file(path).await;
        }
    }
}

fn main() {
    let opt = Opt::from_args();
    let upload_dir = tempdir().expect("cannot prepare upload directory");
    let listen_ctrlc = CtrlC::new().expect("cannot capture Ctrl+C");
    let listen_address = opt.address;

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

    let mut server = tide::with_state(State {
        opt,
        snippets: SharedSnippets::default(),
        upload_dir: upload_dir.path().to_owned(),
        upload_counter: AtomicUsize::new(0),
    });
    server.at("/").get(|_| respond_index());
    server
        .at("/snippets")
        .get(respond_get_snippets)
        .post(respond_post_snippets)
        .at(":i")
        .delete(respond_delete_snippet);
    server.at("/upload").post(respond_upload);
    server.at("/download/:i").get(respond_download);
    server
        .at("/ipaddrs")
        .get(|req| async { respond_ipaddrs(req) });
    server.at("/qrcode").get(respond_qrcode);

    block_on(server.listen(listen_address).race(async move {
        listen_ctrlc.await;
        Err(ErrorKind::Interrupted.into())
    }))
    .unwrap();
}

fn client_err<E: Error + Send + Sync + 'static>(e: E) -> tide::Error {
    tide::Error::new(StatusCode::BadRequest, e)
}

#[cfg(feature = "read_index_html_from_file_system")]
async fn respond_index() -> tide::Result<Response> {
    let mut path = PathBuf::from(file!());
    path.set_file_name("web");
    path.push("index.html");
    let content = BufReader::new(File::open(path).await?);
    Ok(Response::new(StatusCode::Ok)
        .body(content)
        .set_mime(TEXT_HTML_UTF_8))
}

#[cfg(not(feature = "read_index_html_from_file_system"))]
async fn respond_index() -> tide::Result<Response> {
    let content = &include_bytes!("web/index.min.html.gz")[..];
    Ok(Response::new(StatusCode::Ok)
        .body(content)
        .set_mime(TEXT_HTML_UTF_8)
        .set_header("Content-Encoding".parse().unwrap(), "gzip"))
}

async fn respond_get_snippets(req: Request<State>) -> tide::Result<Response> {
    let state = req.state();
    let snippets = state.snippets.read().await;
    Ok(Response::new(StatusCode::Ok).body_json(&*snippets)?)
}

async fn respond_post_snippets(mut req: Request<State>) -> tide::Result<Response> {
    let snippet = req.body_json::<Snippet>().await.map_err(client_err)?;
    if let Snippet::File { .. } = snippet {
        return Ok(Response::new(StatusCode::BadRequest)
            .body_string("cannot upload a file using POST /snippets".to_owned()));
    }

    let state = req.state();
    state.push_snippet(snippet).await;
    Ok(Response::new(StatusCode::Ok))
}

async fn respond_delete_snippet(req: Request<State>) -> tide::Result<Response> {
    let i = req.param::<usize>("i").map_err(client_err)?;
    let state = req.state();
    let mut snippets = state.snippets.write().await;
    if i < snippets.len() {
        let snippet = snippets.remove(i);
        state.delete_snippet(snippet).await;
        Ok(Response::new(StatusCode::Ok))
    } else {
        Ok(Response::new(StatusCode::NotFound))
    }
}

async fn respond_upload(mut req: Request<State>) -> tide::Result<Response> {
    #[derive(Deserialize)]
    struct Query {
        f: String,
    }

    let name = req.query::<Query>()?.f;

    let id;
    let path;
    {
        let state = req.state();
        id = state.upload_counter.fetch_add(1, Ordering::Relaxed);
        path = state.upload_path(id);
    }

    let mut f = File::create(&path).await?;
    let size_result = copy(&mut req, &mut f).await;
    drop(f);
    let size = match size_result {
        Ok(size) => size,
        Err(e) => {
            let _ = remove_file(path).await;
            return Err(e.into());
        }
    };

    let state = req.state();
    let snippet = Snippet::File {
        id,
        size,
        name,
        mime: req
            .header(&CONTENT_TYPE)
            .and_then(|values| values.last())
            .map_or_else(|| APPLICATION_OCTET_STREAM.to_string(), ToString::to_string),
    };
    state.push_snippet(snippet).await;
    Ok(Response::new(StatusCode::Ok))
}

async fn respond_download(req: Request<State>) -> tide::Result<Response> {
    let i = req.param::<usize>("i").map_err(client_err)?;
    let state = req.state();
    let snippets = state.snippets.read().await;
    let (id, name, mime) = match snippets.get(i) {
        Some(Snippet::File { id, name, mime, .. }) => (*id, name, mime),
        _ => return Ok(Response::new(StatusCode::NotFound)),
    };
    let path = state.upload_path(id);
    let reader = BufReader::new(File::open(path).await?);

    Ok(Response::new(StatusCode::Ok)
        .body(reader)
        .set_header(CONTENT_TYPE, mime)
        .set_header(
            "Content-Disposition".parse().unwrap(),
            format!("attachment; filename=\"{}\"", name.replace('"', "\\\"")),
        ))
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

fn respond_ipaddrs(req: Request<State>) -> tide::Result<Response> {
    let addresses = get_server_addresses(req.state().opt.address)?;
    Ok(Response::new(StatusCode::Ok).body_json(&addresses)?)
}

async fn respond_qrcode(req: Request<State>) -> tide::Result<Response> {
    #[derive(Deserialize)]
    struct Query {
        s: String,
    }

    let input = req.query::<Query>()?.s;
    let image = QrCode::with_error_correction_level(input, EcLevel::L)?
        .render()
        .dark_color(Color("#eee"))
        .light_color(Color("transparent"))
        .quiet_zone(false)
        .build();

    Ok(Response::new(StatusCode::Ok)
        .body(Cursor::new(image))
        .set_mime(IMAGE_SVG))
}
