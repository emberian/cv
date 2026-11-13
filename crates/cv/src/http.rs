//! Axum/Hyper transport for the synchronous core API. Bounded channels bridge staged
//! readers and uploads without buffering a pack or running disk work on Tokio workers.
use axum::{
    body::{Body, Bytes},
    response::{
        sse::{Event, Sse},
        IntoResponse,
    },
    Router,
};
use futures_util::TryStreamExt;
use std::{
    io::{self, Read},
    net::TcpListener,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio_stream::wrappers::ReceiverStream;

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub first_byte: Duration,
    pub request: Duration,
    pub connections: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            first_byte: Duration::from_secs(5),
            request: Duration::from_secs(30),
            connections: 64,
        }
    }
}

pub(crate) fn serve<H>(listener: TcpListener, limits: Limits, handler: H) -> anyhow::Result<()>
where
    H: Fn(&mut Request<'_>) + Send + Sync + 'static,
{
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let env = cv_core::env::Env::scoped();
    runtime.block_on(async move {
        listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let handler = Arc::new(handler);
        let workers = Arc::new(Semaphore::new(limits.connections));
        let app = Router::new().fallback(move |request: axum::extract::Request| {
            let handler = handler.clone();
            let env = env.clone();
            let workers = workers.clone();
            async move {
                let Ok(worker) = workers.try_acquire_owned() else {
                    return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                };
                let (parts, body) = request.into_parts();
                if parts.uri.to_string().len() + parts.method.as_str().len() + 12 > 8192 {
                    return axum::http::StatusCode::URI_TOO_LONG.into_response();
                }
                if parts
                    .headers
                    .iter()
                    .any(|(name, value)| name.as_str().len() + value.as_bytes().len() + 4 > 8192)
                {
                    return axum::http::StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE.into_response();
                }
                if parts.headers.len() > 128
                    || parts
                        .headers
                        .iter()
                        .map(|(n, v)| n.as_str().len() + v.as_bytes().len() + 4)
                        .sum::<usize>()
                        > 64 * 1024
                {
                    return axum::http::StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE.into_response();
                }
                let mut encodings = parts.headers.get_all("transfer-encoding").iter();
                if let Some(encoding) = encodings.next() {
                    if !encoding.as_bytes().eq_ignore_ascii_case(b"chunked") || encodings.next().is_some() {
                        return axum::http::StatusCode::BAD_REQUEST.into_response();
                    }
                }

                if parts.headers.get_all("host").iter().count() != 1 {
                    return axum::http::StatusCode::BAD_REQUEST.into_response();
                }
                let (tx, rx) = oneshot::channel();
                // Timeout the incoming body as a whole, rather than allowing each read
                // to reset the deadline. The synchronous reader is used only off-runtime.
                let deadline = tokio::time::Instant::now() + limits.request;
                let body = body.into_data_stream().map_err(io::Error::other);
                let body = tokio_util::io::StreamReader::new(body);
                let body = DeadlineReader {
                    inner: body,
                    deadline: Box::pin(tokio::time::sleep_until(deadline)),
                };
                let mut reader = tokio_util::io::SyncIoBridge::new(body);
                tokio::task::spawn_blocking(move || {
                    let _worker = worker;
                    let _env = env.map(cv_core::env::Env::reenter);
                    let mut request = Request {
                        parts,
                        reader: &mut reader,
                        response: Some(tx),
                    };
                    handler(&mut request);
                });
                rx.await
                    .unwrap_or_else(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response())
            }
        });
        let slots = Arc::new(Semaphore::new(limits.connections));
        let shed_slots = Arc::new(Semaphore::new(limits.connections));
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("cv serve: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let permit = slots.clone().try_acquire_owned();
            let shed = if permit.is_err() {
                match shed_slots.clone().try_acquire_owned() {
                    Ok(p) => Some(p),
                    Err(_) => continue,
                }
            } else {
                None
            };
            let app = app.clone();
            tokio::spawn(async move {
                let _shed = shed;
                let _permit = match permit {
                    Ok(p) => p,
                    Err(_) => {
                        // Hyper owns even the overload response's HTTP framing.
                        let service = hyper::service::service_fn(|_| async {
                            Ok::<_, std::convert::Infallible>(
                                axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
                            )
                        });
                        let mut builder = hyper::server::conn::http1::Builder::new();
                        builder
                            .keep_alive(false)
                            .timer(hyper_util::rt::TokioTimer::new())
                            .header_read_timeout(limits.first_byte);
                        let _ = tokio::time::timeout(
                            Duration::from_secs(1),
                            builder.serve_connection(hyper_util::rt::TokioIo::new(stream), service),
                        )
                        .await;
                        return;
                    }
                };
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder
                    .timer(hyper_util::rt::TokioTimer::new())
                    .header_read_timeout(limits.first_byte)
                    .max_headers(128)
                    .max_buf_size(64 * 1024);
                let service = hyper_util::service::TowerToHyperService::new(app);
                let _ = builder
                    .serve_connection(hyper_util::rt::TokioIo::new(WriteDeadline::new(stream)), service)
                    .await;
            });
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    })
}

/// Bound a stalled socket writer independently of channel backpressure: even a
/// small response can fit the channel while its peer never consumes the bytes.
struct WriteDeadline {
    inner: tokio::net::TcpStream,
    stall: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}
impl WriteDeadline {
    fn new(inner: tokio::net::TcpStream) -> Self {
        Self { inner, stall: None }
    }
    fn stalled(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<io::Result<usize>> {
        use std::future::Future;
        let timer = self
            .stall
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(Duration::from_secs(30))));
        if timer.as_mut().poll(cx).is_ready() {
            std::task::Poll::Ready(Err(io::ErrorKind::TimedOut.into()))
        } else {
            std::task::Poll::Pending
        }
    }
}
impl tokio::io::AsyncRead for WriteDeadline {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}
impl tokio::io::AsyncWrite for WriteDeadline {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        let this = self.get_mut();
        match std::pin::Pin::new(&mut this.inner).poll_write(cx, buf) {
            std::task::Poll::Pending => this.stalled(cx),
            ready => {
                this.stall = None;
                ready
            }
        }
    }
    fn poll_flush(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

struct DeadlineReader<R> {
    inner: R,
    deadline: std::pin::Pin<Box<tokio::time::Sleep>>,
}
impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for DeadlineReader<R> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        use std::future::Future;
        let this = self.get_mut();
        if this.deadline.as_mut().poll(cx).is_ready() {
            return std::task::Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
        }
        std::pin::Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

pub(crate) struct Request<'a> {
    parts: axum::http::request::Parts,
    reader: &'a mut dyn Read,
    response: Option<oneshot::Sender<axum::response::Response>>,
}
impl Read for Request<'_> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.reader.read(b)
    }
}
impl Request<'_> {
    pub(crate) fn method(&self) -> &str {
        self.parts.method.as_str()
    }
    pub(crate) fn url(&self) -> &str {
        self.parts.uri.path_and_query().map_or("/", |v| v.as_str())
    }
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.parts.headers.get(name)?.to_str().ok()
    }
    pub(crate) fn respond(&mut self, response: Response<'_>) {
        let Some(tx) = self.response.take() else {
            return;
        };
        let Response { status, headers, body } = response;
        let (mut out, pump): (_, Box<dyn FnOnce() + '_>) = match body {
            ResponseBody::Sized { mut body, len } => {
                let (send, recv) = mpsc::channel(2);
                let mut out = axum::response::Response::new(Body::from_stream(ReceiverStream::new(recv)));
                if status >= 200 && status != 204 && status != 304 {
                    out.headers_mut().insert(axum::http::header::CONTENT_LENGTH, len.into());
                }
                (
                    out,
                    Box::new(move || {
                        let result = (|| -> io::Result<()> {
                            let mut left = len;
                            let mut buffer = vec![0; 64 * 1024];
                            while left > 0 {
                                let want = left.min(buffer.len() as u64) as usize;
                                let n = body.read(&mut buffer[..want])?;
                                if n == 0 {
                                    return Err(io::ErrorKind::UnexpectedEof.into());
                                }
                                send_bounded(&send, Ok(Bytes::copy_from_slice(&buffer[..n])))?;
                                left -= n as u64;
                            }
                            if body.read(&mut [0])? != 0 {
                                return Err(io::Error::other("response exceeds declared length"));
                            }
                            Ok(())
                        })();
                        if let Err(e) = result {
                            let _ = send_bounded(&send, Err(e));
                        }
                    }),
                )
            }
            ResponseBody::Events(response) => (*response, Box::new(|| {})),
        };
        *out.status_mut() =
            axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        out.headers_mut().extend(*headers);
        if tx.send(out).is_ok() {
            pump();
        }
    }
}
fn send_bounded<T>(tx: &mpsc::Sender<T>, value: T) -> io::Result<()> {
    tokio::runtime::Handle::current().block_on(async {
        tokio::time::timeout(Duration::from_secs(30), tx.send(value))
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
    })
}
enum ResponseBody<'a> {
    Sized { body: Box<dyn Read + 'a>, len: u64 },
    Events(Box<axum::response::Response>),
}
pub(crate) struct Response<'a> {
    status: u16,
    headers: Box<axum::http::HeaderMap>,
    body: ResponseBody<'a>,
}
impl<'a> Response<'a> {
    pub(crate) fn bytes(status: u16, body: Vec<u8>) -> Response<'static> {
        let len = body.len() as u64;
        Response::stream(status, Box::new(io::Cursor::new(body)), len)
    }
    pub(crate) fn empty(status: u16) -> Response<'static> {
        Response::bytes(status, Vec::new())
    }
    pub(crate) fn stream(status: u16, body: Box<dyn Read + 'a>, len: u64) -> Self {
        Self {
            status,
            headers: Box::default(),
            body: ResponseBody::Sized { body, len },
        }
    }
    pub(crate) fn events(stream: impl futures_util::Stream<Item = Result<Event, io::Error>> + Send + 'static) -> Self {
        let mut r = Self {
            status: 200,
            headers: Box::default(),
            body: ResponseBody::Events(Box::new(Sse::new(stream).into_response())),
        };
        r.add_header("Cache-Control", "no-cache");
        r.add_header("X-Accel-Buffering", "no");
        r
    }
    pub(crate) fn add_header(&mut self, name: &str, value: &str) {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::try_from(name),
            axum::http::HeaderValue::try_from(value),
        ) {
            self.headers.append(name, value);
        }
    }
    #[cfg(all(test, feature = "probe-heap"))]
    pub(crate) fn write_body_to(self, mut out: impl io::Write) -> io::Result<()> {
        if let ResponseBody::Sized { mut body, len } = self.body {
            let sent = io::copy(&mut body.by_ref().take(len), &mut out)?;
            if sent != len {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Ok(())
    }
}
