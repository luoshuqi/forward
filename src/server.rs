use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs::File;
use std::future::Future;
use std::io;
use std::io::{BufReader, ErrorKind};
use std::net::SocketAddr;
use std::pin::Pin;
use std::str::from_utf8;
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use conerror::{conerror, Error};
use log::{debug, error, info};
use rustls_pemfile::{certs, private_key};
use structopt::StructOpt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::spawn;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;
use tokio::time::sleep;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

use crate::protocol::{unexpected_cmd, Command, Connection, Id};
use crate::util::async_drop::Dropper;
use crate::util::copy::copy_bidirectional;
use crate::util::error::StrError;
use crate::util::select::{select, Either};
use crate::util::signal::catch_signal;

#[derive(StructOpt)]
pub struct Options {
    /// 客户端监听地址，格式为 ip:port
    #[structopt(long)]
    client: SocketAddr,

    /// HTTP 监听地址，格式为 ip:port
    #[structopt(long)]
    http: SocketAddr,

    /// 证书私钥路径
    #[structopt(long)]
    key: String,

    /// 证书路径
    #[structopt(long)]
    cert: String,

    /// 密钥
    #[structopt(long)]
    secret: String,
}

#[derive(Clone)]
struct State {
    options: Arc<Options>,
    acceptor: TlsAcceptor,
    shutdown: Arc<Notify>,
    coordinator: Arc<Coordinator>,
}

#[conerror]
impl State {
    #[conerror]
    fn new(options: Options) -> conerror::Result<Self> {
        let listener = create_acceptor(&options.key, &options.cert)?;
        Ok(Self {
            options: Arc::new(options),
            acceptor: listener,
            shutdown: Arc::new(Notify::new()),
            coordinator: Arc::new(Coordinator::new()),
        })
    }
}

struct Coordinator {
    client: RwLock<HashMap<String, UnboundedSender<ForwardRequest>>>,
    http: Mutex<HashMap<Id, Either<Waker, Dropper<TlsStream<TcpStream>>>>>,
}

#[conerror]
impl Coordinator {
    fn new() -> Self {
        Self {
            client: RwLock::new(HashMap::new()),
            http: Mutex::new(HashMap::new()),
        }
    }

    fn add_client(&self, domain: &[String]) -> conerror::Result<UnboundedReceiver<ForwardRequest>> {
        let mut guard = self.client.write().unwrap();
        for v in domain {
            if guard.get(v).is_some() {
                return Err(Error::plain(format!("domain {} exists", v)));
            }
        }

        let (tx, rx) = unbounded_channel();
        for v in domain {
            guard.insert(v.clone(), tx.clone());
        }
        Ok(rx)
    }

    fn remove_client(&self, domain: &[String]) {
        let mut guard = self.client.write().unwrap();
        for v in domain {
            guard.remove(v);
        }
    }

    fn request_forward<'a>(&'a self, domain: &'a str) -> RequestForward<'a> {
        RequestForward {
            coordinator: self,
            domain,
            id: Some(Id::new()),
        }
    }

    fn notify_client(&self, id: Id, domain: &str) -> Option<()> {
        let req = ForwardRequest {
            id,
            domain: domain.to_string(),
        };
        let guard = self.client.read().unwrap();
        guard.get(domain)?.send(req).ok()
    }

    fn accept_forward(&self, id: Id, stream: Dropper<TlsStream<TcpStream>>) -> bool {
        let waker = {
            let mut guard = self.http.lock().unwrap();
            match guard.remove(&id) {
                Some(Either::Left(waker)) => {
                    guard.insert(id, Either::Right(stream));
                    waker
                }
                Some(Either::Right(_)) => return false,
                None => return false,
            }
        };
        waker.wake();
        true
    }
}

struct RequestForward<'a> {
    coordinator: &'a Coordinator,
    domain: &'a str,
    id: Option<Id>,
}

impl<'a> Future for RequestForward<'a> {
    type Output = Option<Dropper<TlsStream<TcpStream>>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.id.is_none() {
            panic!("poll completed future");
        }

        let id = self.id.unwrap();
        {
            let mut guard = self.coordinator.http.lock().unwrap();
            match guard.entry(id) {
                Entry::Vacant(entry) => {
                    entry.insert(Either::Left(cx.waker().clone()));
                }
                Entry::Occupied(mut entry) => {
                    return match entry.get() {
                        Either::Left(waker) => {
                            if !cx.waker().will_wake(waker) {
                                entry.insert(Either::Left(cx.waker().clone()));
                            }
                            Poll::Pending
                        }
                        Either::Right(_) => {
                            self.id = None;
                            entry.remove().right().into()
                        }
                    };
                }
            }
        }

        if self.coordinator.notify_client(id, self.domain).is_none() {
            self.coordinator.http.lock().unwrap().remove(&id);
            self.id = None;
            return None.into();
        }

        Poll::Pending
    }
}

impl<'a> Drop for RequestForward<'a> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.coordinator.http.lock().unwrap().remove(&id);
        }
    }
}

struct ForwardRequest {
    id: Id,
    domain: String,
}

#[conerror]
pub async fn start(options: Options) -> conerror::Result<()> {
    let client_listener = TcpListener::bind(options.client).await?;
    let http_listener = TcpListener::bind(options.http).await?;
    info!("client listening on {}", client_listener.local_addr().unwrap());
    info!("http listening on {}", http_listener.local_addr().unwrap());

    let state = State::new(options)?;
    let result = select(
        select(accept_client(&state, client_listener), accept_http(&state, http_listener)),
        catch_signal(),
    )
    .await;
    state.shutdown.notify_waiters();
    sleep(Duration::from_secs(1)).await;
    match result {
        Either::Left(Either::Left(result)) => Ok(result?),
        Either::Left(Either::Right(result)) => Ok(result?),
        Either::Right(result) => Ok(result?),
    }
}

#[conerror]
fn create_acceptor(key: &str, cert: &str) -> conerror::Result<TlsAcceptor> {
    let key = load_key(key)?;
    let cert = load_certs(cert)?;
    let config = ServerConfig::builder().with_no_client_auth().with_single_cert(cert, key)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[conerror]
fn load_certs(path: &str) -> conerror::Result<Vec<CertificateDer<'static>>> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut v = Vec::new();
    for c in certs(&mut reader) {
        v.push(c?);
    }
    Ok(v)
}

#[conerror]
fn load_key(path: &str) -> conerror::Result<PrivateKeyDer<'static>> {
    let mut reader = BufReader::new(File::open(path)?);
    match private_key(&mut reader)? {
        Some(v) => Ok(v),
        None => Err(Error::plain(format!("no key found in {}", path))),
    }
}

#[conerror]
async fn accept_client(state: &State, listener: TcpListener) -> conerror::Result<()> {
    loop {
        let (stream, addr) = listener.accept().await?;
        debug!("[{}] client connect", addr);
        let state = state.clone();
        spawn(async move {
            match select(serve_client(&state, stream, addr), state.shutdown.notified()).await {
                Either::Left(Ok(())) => (),
                Either::Left(Err(err)) => error!("[{}] {}", addr, err),
                Either::Right(()) => debug!("[{}] accept_client canceled", addr),
            };
        });
    }
}

struct RemoveClient<'a> {
    state: &'a State,
    domain: &'a [String],
}

impl<'a> Drop for RemoveClient<'a> {
    fn drop(&mut self) {
        self.state.coordinator.remove_client(self.domain);
    }
}

#[conerror]
async fn serve_client(state: &State, stream: TcpStream, addr: SocketAddr) -> conerror::Result<()> {
    let stream = Dropper::new(state.acceptor.accept(stream).await?);
    let mut conn = Connection::new(stream);
    let domain;
    let _remove_client;
    let mut rx = match conn.input.read().await? {
        Some(Command::Register(register)) if register.verify_sign(&state.options.secret) => {
            match state.coordinator.add_client(&register.domain) {
                Ok(rx) => {
                    domain = register.domain;
                    _remove_client = RemoveClient { state, domain: &domain };
                    conn.output.write(&Command::Ok).await?;
                    rx
                }
                Err(err) => {
                    conn.output.write(&Command::Error(StrError::new(err.to_string()))).await?;
                    return Ok(());
                }
            }
        }
        Some(Command::Register(_)) => {
            conn.output.write(&Command::Error(StrError::new("bad signature"))).await?;
            return Ok(());
        }
        Some(Command::Accept(accept)) if accept.verify_sign(&state.options.secret) => {
            conn.output.write(&Command::Ok).await?;
            state.coordinator.accept_forward(accept.id, conn.unwrap());
            return Ok(());
        }
        Some(Command::Accept(_)) => {
            conn.output.write(&Command::Error(StrError::new("bad signature"))).await?;
            return Ok(());
        }
        Some(cmd) => {
            unexpected_cmd(&mut conn.output, &cmd).await?;
            return Ok(());
        }
        None => return Ok(()),
    };

    loop {
        match select(select(conn.input.read(), rx.recv()), sleep(Duration::from_secs(300))).await {
            Either::Left(Either::Left(cmd)) => match cmd? {
                Some(Command::Ping) => conn.output.write(&Command::Pong).await?,
                Some(cmd) => unexpected_cmd(&mut conn.output, &cmd).await?,
                None => break,
            },
            Either::Left(Either::Right(Some(req))) => {
                let cmd = Command::Forward {
                    id: req.id,
                    domain: req.domain,
                };
                conn.output.write(&cmd).await?;
            }
            Either::Left(Either::Right(None)) => (),
            Either::Right(_) => {
                info!("[{}] client inactive", addr);
                break;
            }
        }
    }
    Ok(())
}

#[conerror]
async fn accept_http(state: &State, listener: TcpListener) -> conerror::Result<()> {
    loop {
        let (stream, addr) = listener.accept().await?;
        debug!("[{}] http connect", addr);
        let state = state.clone();
        spawn(async move {
            match select(serve_http(&state, stream, addr), state.shutdown.notified()).await {
                Either::Left(Ok(())) => (),
                Either::Left(Err(err)) => error!("[{}] {}", addr, err),
                Either::Right(()) => debug!("[{}] accept_http canceled", addr),
            }
        });
    }
}

#[conerror]
async fn serve_http(state: &State, stream: TcpStream, addr: SocketAddr) -> conerror::Result<()> {
    let mut stream = Dropper::new(state.acceptor.accept(stream).await?);
    debug!("[{}] reading host", addr);
    let result = match select(read_host(&mut stream), sleep(Duration::from_secs(15))).await {
        Either::Left(host) => match host? {
            Some((buf, host)) => {
                debug!("[{}] [{}] read host success", addr, host);
                match select(state.coordinator.request_forward(&host), sleep(Duration::from_secs(15))).await {
                    Either::Left(Some(mut dst)) => {
                        debug!("[{}] [{}] forward start", addr, host);
                        if !buf.is_empty() {
                            dst.write_all(&buf).await?;
                        }
                        let n = copy_bidirectional(dst, stream).await?;
                        debug!("[{}] [{}] forward end {} {}", addr, host, n.0, n.1);
                        return Ok(());
                    }
                    Either::Left(None) => Ok(BAD_GATEWAY.write(&mut stream).await?),
                    Either::Right(()) => Ok(GATEWAY_TIMEOUT.write(&mut stream).await?),
                }
            }
            None => {
                debug!("[{}] read host failed", addr);
                Ok(())
            }
        },
        Either::Right(()) => {
            debug!("[{}] read host timeout", addr);
            Ok(())
        }
    };
    let _ = stream.shutdown().await;
    result
}

const BUF_SIZE: usize = 1024;
const MAX_BUF_SIZE: usize = 8 * BUF_SIZE;

#[conerror]
async fn read_host(stream: &mut (impl AsyncRead + AsyncWrite + Unpin)) -> conerror::Result<Option<(Vec<u8>, String)>> {
    let mut buf = Vec::with_capacity(BUF_SIZE);
    unsafe { buf.set_len(BUF_SIZE) };
    let mut filled = 0;
    let mut from = 0;
    loop {
        let n = stream.read(&mut buf[filled..]).await?;
        if n == 0 {
            return if filled == 0 {
                Ok(None)
            } else {
                return Err(io::Error::from(ErrorKind::UnexpectedEof))?;
            };
        }
        filled += n;
        match find_host(&buf[from..filled]) {
            Ok(host) => {
                let host = from_utf8(host)?.trim().to_string();
                unsafe { buf.set_len(filled) };
                return Ok(Some((buf, host)));
            }
            Err(offset) => {
                from += offset;
                if filled == buf.len() {
                    if filled < MAX_BUF_SIZE {
                        buf.reserve(BUF_SIZE);
                        unsafe { buf.set_len(buf.len() + BUF_SIZE) };
                    } else {
                        return Err(Error::plain("header too large"));
                    }
                }
            }
        }
    }
}

fn find_host(buf: &[u8]) -> Result<&[u8], usize> {
    let search = b"host";
    let mut offset = 0;
    for i in 0..buf.len() {
        if buf[i] == search[offset] || buf[i] == search[offset] - 32 {
            if offset == search.len() - 1 {
                if i == buf.len() - 1 {
                    return Err(i - offset);
                }
                if buf[i + 1] != b':' {
                    offset = 0;
                    continue;
                }

                let mut colon = 0;
                for j in i + 2..buf.len() {
                    if buf[j] == b':' {
                        colon = j;
                    } else if buf[j] == b'\r' {
                        if colon == 0 {
                            colon = j;
                        }
                        return Ok(&buf[i + 2..colon]);
                    }
                }
                return Err(i - offset);
            }
            offset += 1;
        } else {
            offset = 0;
        }
    }
    Err(buf.len() - offset)
}

struct Response {
    code: u16,
    reason_phrase: &'static str,
}

const BAD_GATEWAY: Response = Response {
    code: 502,
    reason_phrase: "Bad Gateway",
};

const GATEWAY_TIMEOUT: Response = Response {
    code: 504,
    reason_phrase: "Gateway Timeout",
};

#[conerror]
impl Response {
    #[conerror]
    pub async fn write(self, writer: &mut (impl AsyncWrite + Unpin)) -> conerror::Result<()> {
        let s = format!("HTTP/1.1 {} {}\r\ncontent-length: 0\r\n\r\n", self.code, self.reason_phrase);
        writer.write_all(s.as_bytes()).await?;
        Ok(())
    }
}
