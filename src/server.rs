use crate::{Args, BoxResult};

use async_walkdir::WalkDir;
use async_zip::read::seek::ZipFileReader;
use async_zip::write::{EntryOptions, ZipFileWriter};
use async_zip::Compression;
use futures::stream::StreamExt;
use futures::TryStreamExt;
use headers::{
    AccessControlAllowHeaders, AccessControlAllowOrigin, ContentType, ETag, HeaderMap,
    HeaderMapExt, IfModifiedSince, IfNoneMatch, LastModified,
};
use hyper::header::{HeaderValue, ACCEPT, CONTENT_TYPE, ORIGIN, RANGE, WWW_AUTHENTICATE};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, StatusCode};
use percent_encoding::percent_decode;
use serde::Serialize;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::fs::File;
use tokio::io::AsyncWrite;
use tokio::{fs, io};
use tokio_util::codec::{BytesCodec, FramedRead};
use tokio_util::io::{ReaderStream, StreamReader};

type Request = hyper::Request<Body>;
type Response = hyper::Response<Body>;

const INDEX_HTML: &str = include_str!("assets/index.html");
const INDEX_CSS: &str = include_str!("assets/index.css");
const INDEX_JS: &str = include_str!("assets/index.js");
const BUF_SIZE: usize = 1024 * 16;

pub async fn serve(args: Args) -> BoxResult<()> {
    let address = args.address()?;
    let inner = Arc::new(InnerService::new(args));
    let make_svc = make_service_fn(move |_| {
        let inner = inner.clone();
        async {
            Ok::<_, Infallible>(service_fn(move |req| {
                let inner = inner.clone();
                inner.call(req)
            }))
        }
    });

    let server = hyper::Server::try_bind(&address)?.serve(make_svc);
    let address = server.local_addr();
    eprintln!("Files served on http://{}", address);
    server.await?;

    Ok(())
}

struct InnerService {
    args: Args,
}

impl InnerService {
    pub fn new(args: Args) -> Self {
        Self { args }
    }

    pub async fn call(self: Arc<Self>, req: Request) -> Result<Response, hyper::Error> {
        let method = req.method().clone();
        let uri = req.uri().clone();
        let cors = self.args.cors;

        let mut res = self.handle(req).await.unwrap_or_else(|e| {
            let mut res = Response::default();
            *res.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            *res.body_mut() = Body::from(e.to_string());
            res
        });

        info!(r#""{} {}" - {}"#, method, uri, res.status());

        if cors {
            add_cors(&mut res);
        }
        Ok(res)
    }

    pub async fn handle(self: Arc<Self>, req: Request) -> BoxResult<Response> {
        let mut res = Response::default();

        if !self.auth_guard(&req, &mut res) {
            return Ok(res);
        }

        let path = req.uri().path();

        let filepath = match self.extract_path(path) {
            Some(v) => v,
            None => {
                *res.status_mut() = StatusCode::FORBIDDEN;
                return Ok(res);
            }
        };
        let filepath = filepath.as_path();

        let query = req.uri().query().unwrap_or_default();

        let meta = fs::metadata(filepath).await.ok();
        let is_miss = meta.is_none();
        let is_dir = meta.map(|v| v.is_dir()).unwrap_or_default();

        let readonly = self.args.readonly;

        match *req.method() {
            Method::GET if is_dir && query == "zip" => {
                self.handle_zip_dir(filepath, &mut res).await?
            }
            Method::GET if is_dir && query.starts_with("q=") => {
                self.handle_query_dir(filepath, &query[3..], &mut res)
                    .await?
            }
            Method::GET if !is_dir && !is_miss => {
                self.handle_send_file(filepath, req.headers(), &mut res)
                    .await?
            }
            Method::GET if is_miss && path.ends_with('/') => {
                self.handle_ls_dir(filepath, false, &mut res).await?
            }
            Method::GET => self.handle_ls_dir(filepath, true, &mut res).await?,
            Method::OPTIONS => *res.status_mut() = StatusCode::NO_CONTENT,
            Method::PUT if readonly => *res.status_mut() = StatusCode::FORBIDDEN,
            Method::PUT => self.handle_upload(filepath, req, &mut res).await?,
            Method::DELETE if !is_miss && readonly => *res.status_mut() = StatusCode::FORBIDDEN,
            Method::DELETE if !is_miss => self.handle_delete(filepath, is_dir).await?,
            _ => *res.status_mut() = StatusCode::NOT_FOUND,
        }

        Ok(res)
    }

    async fn handle_upload(
        &self,
        path: &Path,
        mut req: Request,
        res: &mut Response,
    ) -> BoxResult<()> {
        let ensure_parent = match path.parent() {
            Some(parent) => match fs::metadata(parent).await {
                Ok(meta) => meta.is_dir(),
                Err(_) => {
                    fs::create_dir_all(parent).await?;
                    true
                }
            },
            None => false,
        };
        if !ensure_parent {
            *res.status_mut() = StatusCode::FORBIDDEN;
            return Ok(());
        }

        let mut file = fs::File::create(&path).await?;

        let body_with_io_error = req
            .body_mut()
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err));

        let body_reader = StreamReader::new(body_with_io_error);

        futures::pin_mut!(body_reader);

        io::copy(&mut body_reader, &mut file).await?;

        let req_query = req.uri().query().unwrap_or_default();
        if req_query == "unzip" {
            let root = path.parent().unwrap();
            let mut zip = ZipFileReader::new(File::open(&path).await?).await?;
            for i in 0..zip.entries().len() {
                let entry = &zip.entries()[i];
                let entry_name = entry.name();
                let entry_path = root.join(entry_name);
                if entry_name.ends_with('/') {
                    fs::create_dir_all(entry_path).await?;
                } else {
                    if let Some(parent) = entry_path.parent() {
                        if fs::symlink_metadata(parent).await.is_err() {
                            fs::create_dir_all(&parent).await?;
                        }
                    }
                    let mut outfile = fs::File::create(&entry_path).await?;
                    let mut reader = zip.entry_reader(i).await?;
                    io::copy(&mut reader, &mut outfile).await?;
                }
            }
            fs::remove_file(&path).await?;
        }

        Ok(())
    }

    async fn handle_delete(&self, path: &Path, is_dir: bool) -> BoxResult<()> {
        match is_dir {
            true => fs::remove_dir_all(path).await?,
            false => fs::remove_file(path).await?,
        }
        Ok(())
    }

    async fn handle_ls_dir(&self, path: &Path, exist: bool, res: &mut Response) -> BoxResult<()> {
        let mut paths: Vec<PathItem> = vec![];
        if exist {
            let mut rd = fs::read_dir(path).await?;
            while let Some(entry) = rd.next_entry().await? {
                let entry_path = entry.path();
                if let Ok(item) = to_pathitem(entry_path, path.to_path_buf()).await {
                    paths.push(item);
                }
            }
        }
        self.send_index(path, paths, res)
    }

    async fn handle_query_dir(
        &self,
        path: &Path,
        query: &str,
        res: &mut Response,
    ) -> BoxResult<()> {
        let mut paths: Vec<PathItem> = vec![];
        let mut walkdir = WalkDir::new(path);
        while let Some(entry) = walkdir.next().await {
            if let Ok(entry) = entry {
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(&query.to_lowercase())
                {
                    continue;
                }
                if fs::symlink_metadata(entry.path()).await.is_err() {
                    continue;
                }
                if let Ok(item) = to_pathitem(entry.path(), path.to_path_buf()).await {
                    paths.push(item);
                }
            }
        }
        self.send_index(path, paths, res)
    }

    async fn handle_zip_dir(&self, path: &Path, res: &mut Response) -> BoxResult<()> {
        let (mut writer, reader) = tokio::io::duplex(BUF_SIZE);
        let path = path.to_owned();
        tokio::spawn(async move {
            if let Err(e) = dir_zip(&mut writer, &path).await {
                error!("Fail to zip {}, {}", path.display(), e.to_string());
            }
        });
        let stream = ReaderStream::new(reader);
        *res.body_mut() = Body::wrap_stream(stream);
        Ok(())
    }

    async fn handle_send_file(
        &self,
        path: &Path,
        headers: &HeaderMap<HeaderValue>,
        res: &mut Response,
    ) -> BoxResult<()> {
        let (file, meta) = tokio::join!(fs::File::open(path), fs::metadata(path),);
        let (file, meta) = (file?, meta?);
        if let Ok(mtime) = meta.modified() {
            let timestamp = to_timestamp(&mtime);
            let size = meta.len();
            let etag = format!(r#""{}-{}""#, timestamp, size)
                .parse::<ETag>()
                .unwrap();
            let last_modified = LastModified::from(mtime);
            let fresh = {
                // `If-None-Match` takes presedence over `If-Modified-Since`.
                if let Some(if_none_match) = headers.typed_get::<IfNoneMatch>() {
                    !if_none_match.precondition_passes(&etag)
                } else if let Some(if_modified_since) = headers.typed_get::<IfModifiedSince>() {
                    !if_modified_since.is_modified(mtime)
                } else {
                    false
                }
            };
            res.headers_mut().typed_insert(last_modified);
            res.headers_mut().typed_insert(etag);
            if fresh {
                *res.status_mut() = StatusCode::NOT_MODIFIED;
                return Ok(());
            }
        }
        if let Some(mime) = mime_guess::from_path(&path).first() {
            res.headers_mut().typed_insert(ContentType::from(mime));
        }
        let stream = FramedRead::new(file, BytesCodec::new());
        let body = Body::wrap_stream(stream);
        *res.body_mut() = body;

        Ok(())
    }

    fn send_index(
        &self,
        path: &Path,
        mut paths: Vec<PathItem>,
        res: &mut Response,
    ) -> BoxResult<()> {
        paths.sort_unstable();
        let rel_path = match self.args.path.parent() {
            Some(p) => path.strip_prefix(p).unwrap(),
            None => path,
        };
        let data = IndexData {
            breadcrumb: normalize_path(rel_path),
            paths,
            readonly: self.args.readonly,
        };
        let data = serde_json::to_string(&data).unwrap();
        let output = INDEX_HTML.replace(
            "__SLOT__",
            &format!(
                r#"
<title>Files in {}/ - Duf</title>
<style>{}</style>
<script>var DATA = {}; {}</script>
"#,
                rel_path.display(),
                INDEX_CSS,
                data,
                INDEX_JS
            ),
        );
        *res.body_mut() = output.into();

        Ok(())
    }

    fn auth_guard(&self, req: &Request, res: &mut Response) -> bool {
        let pass = {
            match &self.args.auth {
                None => true,
                Some(auth) => match req.headers().get("Authorization") {
                    Some(value) => match value.to_str().ok().map(|v| {
                        let mut it = v.split(' ');
                        (it.next(), it.next())
                    }) {
                        Some((Some("Basic "), Some(tail))) => base64::decode(tail)
                            .ok()
                            .and_then(|v| String::from_utf8(v).ok())
                            .map(|v| v.as_str() == auth)
                            .unwrap_or_default(),
                        _ => false,
                    },
                    None => self.args.no_auth_read && req.method() == Method::GET,
                },
            }
        };
        if !pass {
            *res.status_mut() = StatusCode::UNAUTHORIZED;
            res.headers_mut()
                .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Basic"));
        }
        pass
    }

    fn extract_path(&self, path: &str) -> Option<PathBuf> {
        let decoded_path = percent_decode(path[1..].as_bytes()).decode_utf8().ok()?;
        let slashes_switched = if cfg!(windows) {
            decoded_path.replace('/', "\\")
        } else {
            decoded_path.into_owned()
        };
        let full_path = self.args.path.join(&slashes_switched);
        if full_path.starts_with(&self.args.path) {
            Some(full_path)
        } else {
            None
        }
    }
}

#[derive(Debug, Serialize, Eq, PartialEq, Ord, PartialOrd)]
struct IndexData {
    breadcrumb: String,
    paths: Vec<PathItem>,
    readonly: bool,
}

#[derive(Debug, Serialize, Eq, PartialEq, Ord, PartialOrd)]
struct PathItem {
    path_type: PathType,
    name: String,
    mtime: u64,
    size: Option<u64>,
}

#[derive(Debug, Serialize, Eq, PartialEq, Ord, PartialOrd)]
enum PathType {
    Dir,
    SymlinkDir,
    File,
    SymlinkFile,
}

async fn to_pathitem<P: AsRef<Path>>(path: P, base_path: P) -> BoxResult<PathItem> {
    let path = path.as_ref();
    let rel_path = path.strip_prefix(base_path).unwrap();
    let (meta, meta2) = tokio::join!(fs::metadata(&path), fs::symlink_metadata(&path));
    let (meta, meta2) = (meta?, meta2?);
    let is_dir = meta.is_dir();
    let is_symlink = meta2.file_type().is_symlink();
    let path_type = match (is_symlink, is_dir) {
        (true, true) => PathType::SymlinkDir,
        (false, true) => PathType::Dir,
        (true, false) => PathType::SymlinkFile,
        (false, false) => PathType::File,
    };
    let mtime = to_timestamp(&meta.modified()?);
    let size = match path_type {
        PathType::Dir | PathType::SymlinkDir => None,
        PathType::File | PathType::SymlinkFile => Some(meta.len()),
    };
    let name = normalize_path(rel_path);
    Ok(PathItem {
        path_type,
        name,
        mtime,
        size,
    })
}

fn to_timestamp(time: &SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn normalize_path<P: AsRef<Path>>(path: P) -> String {
    let path = path.as_ref().to_str().unwrap_or_default();
    if cfg!(windows) {
        path.replace('\\', "/")
    } else {
        path.to_string()
    }
}

fn add_cors(res: &mut Response) {
    res.headers_mut()
        .typed_insert(AccessControlAllowOrigin::ANY);
    res.headers_mut().typed_insert(
        vec![RANGE, CONTENT_TYPE, ACCEPT, ORIGIN, WWW_AUTHENTICATE]
            .into_iter()
            .collect::<AccessControlAllowHeaders>(),
    );
}

async fn dir_zip<W: AsyncWrite + Unpin>(writer: &mut W, dir: &Path) -> BoxResult<()> {
    let mut writer = ZipFileWriter::new(writer);
    let mut walkdir = WalkDir::new(dir);
    while let Some(entry) = walkdir.next().await {
        if let Ok(entry) = entry {
            let meta = match fs::symlink_metadata(entry.path()).await {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if meta.is_file() {
                let filepath = entry.path();
                let filename = match filepath.strip_prefix(dir).ok().and_then(|v| v.to_str()) {
                    Some(v) => v,
                    None => continue,
                };
                let entry_options = EntryOptions::new(filename.to_owned(), Compression::Deflate);
                let mut file = File::open(&filepath).await?;
                let mut file_writer = writer.write_entry_stream(entry_options).await?;
                io::copy(&mut file, &mut file_writer).await?;
                file_writer.close().await?;
            }
        }
    }
    writer.close().await?;
    Ok(())
}
