use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use conerror::{conerror, Error};
use log::{debug, error, info, warn};
use rustls_native_certs::load_native_certs;
use structopt::StructOpt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::spawn;
use tokio::sync::{mpsc, Notify};
use tokio::time::sleep;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

use crate::protocol::{unexpected_cmd, Accept, Command, Connection, Id, Register};
use crate::util::async_drop::Dropper;
use crate::util::copy::copy_bidirectional;
use crate::util::error::StrError;
use crate::util::select::{select, Either};
use crate::util::signal::catch_signal;

#[derive(StructOpt)]
pub struct Options {
    /// 服务器地址
    #[structopt(long)]
    server: String,

    /// 转发配置，格式为"域名:转发地址"。示例："a.foo.com:127.0.0.1:80" 表示把对 a.foo.com 的请求转发到127.0.0.1:80
    #[structopt(long)]
    forward: Vec<ForwardOption>,

    /// 密钥
    #[structopt(long)]
    secret: String,
}

#[conerror]
impl Options {
    fn server_name(&self) -> conerror::Result<ServerName> {
        match self.server.split(":").next() {
            Some(domain) => match ServerName::try_from(domain) {
                Ok(v) => Ok(v),
                Err(e) => Err(Error::plain(format!("{}: {}", domain, e))),
            },
            None => Err(Error::plain(format!("{}: invalid address", self.server))),
        }
    }

    fn domain(&self) -> Vec<String> {
        let mut domain = Vec::with_capacity(self.forward.len());
        for v in &self.forward {
            domain.push(v.domain.clone());
        }
        domain
    }

    fn dest(&self, domain: &str) -> Option<SocketAddr> {
        for v in &self.forward {
            if v.domain == domain {
                return Some(v.destination);
            }
        }
        None
    }
}

struct ForwardOption {
    domain: String,
    destination: SocketAddr,
}

impl FromStr for ForwardOption {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut split = s.splitn(2, ":");
        let domain = split.next().ok_or_else(|| format!("{}: invalid format", s))?;
        let dest = split.next().ok_or_else(|| format!("{}: invalid format", s))?;
        Ok(Self {
            domain: domain.into(),
            destination: dest.parse().map_err(|err| format!("{}: {}", dest, err))?,
        })
    }
}

#[derive(Clone)]
struct State {
    options: Arc<Options>,
    connector: TlsConnector,
    shutdown: Arc<Notify>,
}

#[conerror]
impl State {
    #[conerror]
    fn new(options: Options) -> conerror::Result<Self> {
        Ok(Self {
            options: Arc::new(options),
            connector: create_connector()?,
            shutdown: Arc::new(Notify::new()),
        })
    }
}

#[conerror]
pub async fn start(options: Options) -> conerror::Result<()> {
    if options.forward.is_empty() {
        return Err(Error::plain("missing required argument --forward <forward>"));
    }

    let state = State::new(options)?;
    let result = select(run(&state), catch_signal()).await;
    state.shutdown.notify_waiters();
    sleep(Duration::from_secs(1)).await;
    match result {
        Either::Left(result) => Ok(result?),
        Either::Right(result) => Ok(result?),
    }
}

#[conerror]
fn create_connector() -> conerror::Result<TlsConnector> {
    let mut root = RootCertStore::empty();
    root.add_parsable_certificates(load_native_certs()?);
    let config = ClientConfig::builder().with_root_certificates(root).with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

#[conerror]
async fn connect_server(state: &State) -> conerror::Result<Dropper<TlsStream<TcpStream>>> {
    debug!("connect server");
    let server_name = state.options.server_name()?;
    let stream = TcpStream::connect(&state.options.server).await?;
    let stream = state.connector.connect(server_name.to_owned(), stream).await?;
    Ok(Dropper::new(stream))
}

#[conerror]
async fn run(state: &State) -> conerror::Result<()> {
    let mut conn = Connection::new(connect_server(&state).await?);
    register(&state, &mut conn).await?;
    debug!("register success");

    let mut ping = 0;
    let (reject_tx, mut reject_rx) = mpsc::unbounded_channel::<Command>();
    loop {
        match select(conn.input.read(), select(sleep(Duration::from_secs(60)), reject_rx.recv())).await {
            Either::Left(cmd) => {
                let cmd = cmd?;
                debug!("receive {:?}", cmd);
                ping = 0;
                match cmd {
                    Some(Command::Forward { domain, id }) => match state.options.dest(&domain) {
                        Some(dst) => forward(state.clone(), dst, id, reject_tx.clone()),
                        None => {
                            warn!("unexpected domain: {}", domain);
                            conn.output.write(&Command::Error(StrError::new("unexpected domain"))).await?;
                        }
                    },
                    Some(Command::Pong) => (),
                    Some(cmd) => unexpected_cmd(&mut conn.output, &cmd).await?,
                    None => {
                        info!("server shutdown");
                        return Ok(());
                    }
                }
            }
            Either::Right(v) => match v {
                Either::Left(()) if ping >= 3 => return Err(StrError::new("server not responding"))?,
                Either::Left(()) => {
                    ping += 1;
                    conn.output.write(&Command::Ping).await?;
                }
                Either::Right(Some(cmd)) => conn.output.write(&cmd).await?,
                Either::Right(None) => (),
            },
        }
    }
}

#[conerror]
async fn register(state: &State, conn: &mut Connection<impl AsyncRead + AsyncWrite + Unpin>) -> conerror::Result<()> {
    info!("start register");
    let cmd = Command::Register(Register::new(state.options.domain(), &state.options.secret));
    conn.output.write(&cmd).await?;
    match conn.input.read().await? {
        Some(Command::Ok) => Ok(()),
        Some(Command::Error(err)) => Err(err)?,
        Some(cmd) => Err(StrError::new(format!("unexpected cmd {:?}", cmd)))?,
        None => Err(Error::plain("server shutdown")),
    }
}

#[conerror]
fn forward(state: State, dst: SocketAddr, id: Id, reject_tx: mpsc::UnboundedSender<Command>) {
    spawn(async move {
        if let Either::Left(Err(err)) = select(forward_inner(&state, dst, id, reject_tx), state.shutdown.notified()).await {
            error!("{}", err);
        }
    });
}

#[conerror]
async fn forward_inner(
    state: &State,
    dst: SocketAddr,
    id: Id,
    reject_tx: mpsc::UnboundedSender<Command>,
) -> conerror::Result<()> {
    let dst_stream = match TcpStream::connect(dst).await {
        Ok(stream) => Dropper::new(stream),
        Err(err) => {
            _ = reject_tx.send(Command::Error(StrError::new(err.to_string())));
            return Err(err)?;
        }
    };
    let server_stream = connect_server(&state).await?;
    let mut conn = Connection::new(server_stream);
    conn.output
        .write(&Command::Accept(Accept::new(&state.options.secret, id)))
        .await?;
    match conn.input.read().await? {
        Some(Command::Ok) => {
            let n = copy_bidirectional(conn.unwrap(), dst_stream).await?;
            debug!("[{}] forward end {} {}", dst, n.0, n.1);
        }
        Some(Command::Error(err)) => error!("{}", err),
        Some(cmd) => unexpected_cmd(&mut conn.output, &cmd).await?,
        None => (),
    }
    Ok(())
}
