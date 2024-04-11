use std::io::{Error, ErrorKind};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use conerror::conerror;
use log::error;
use serde::{Deserialize, Serialize};
use sha2::digest::Output;
use sha2::{Digest, Sha256};
use tokio::io::{split, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};

use crate::util::error::StrError;

const MAX_MESSAGE_SIZE: usize = 1024 * 1024;

#[derive(Serialize, Deserialize, Debug)]
pub enum Command {
    Register(Register),
    Forward { id: Id, domain: String },
    Accept(Accept),
    Ping,
    Pong,
    Ok,
    Error(StrError),
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct Id(NonZeroU64);

static ID_COUNTER: AtomicU32 = AtomicU32::new(0);

impl Id {
    pub fn new() -> Self {
        let h = (ID_COUNTER.fetch_add(1, Ordering::Relaxed) as u64) << 32;
        let l = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() & u32::MAX as u64;
        Self(NonZeroU64::new(h | l).unwrap())
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Accept {
    pub id: Id,
    sign: Vec<u8>,
}

impl Accept {
    pub fn new(secret: &str, id: Id) -> Self {
        let sign = Self::sign(secret, id).to_vec();
        Self { id, sign }
    }

    pub fn verify_sign(&self, secret: &str) -> bool {
        Self::sign(secret, self.id).as_slice() == self.sign
    }

    fn sign(secret: &str, id: Id) -> Output<Sha256> {
        let mut hasher = Sha256::new();
        hasher.update(&id.0.get().to_be_bytes());
        hasher.update(secret);
        hasher.finalize()
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Register {
    pub domain: Vec<String>,
    sign: Vec<u8>,
}

impl Register {
    pub fn new(domain: Vec<String>, secret: &str) -> Self {
        let sign = Self::sign(secret, &domain).to_vec();
        Self { domain, sign }
    }

    pub fn verify_sign(&self, secret: &str) -> bool {
        Self::sign(secret, &self.domain).as_slice() == self.sign
    }

    fn sign(secret: &str, domain: &[String]) -> Output<Sha256> {
        let mut hasher = Sha256::new();
        for v in domain {
            hasher.update(v);
        }
        hasher.update(secret);
        hasher.finalize()
    }
}

pub struct Reader<T> {
    stream: T,
    buf: Vec<u8>,
    state: State,
}

enum State {
    ReadLen { filled: usize },
    ReadPayload { len: usize, filled: usize },
}

impl<T> Reader<T>
where
    T: AsyncRead + Unpin,
{
    pub fn new(stream: T) -> Self {
        Self {
            stream,
            buf: Vec::with_capacity(128),
            state: State::ReadLen { filled: 0 },
        }
    }

    pub fn unwrap(self) -> T {
        self.stream
    }

    /// Cancel safe
    #[conerror]
    pub async fn read(&mut self) -> conerror::Result<Option<Command>> {
        loop {
            match self.state {
                State::ReadLen { mut filled } => {
                    if filled == 0 {
                        self.buf.clear();
                        self.buf.reserve(4);
                        unsafe { self.buf.set_len(4) };
                    }
                    let n = self.stream.read(&mut self.buf[filled..]).await?;
                    if n == 0 {
                        return if filled > 0 {
                            Err(Error::from(ErrorKind::UnexpectedEof))?
                        } else {
                            Ok(None)
                        };
                    }
                    filled += n;
                    debug_assert!(filled <= 4);
                    if filled == 4 {
                        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
                        self.state = State::ReadPayload { len, filled: 0 };
                    } else {
                        self.state = State::ReadLen { filled };
                    }
                }
                State::ReadPayload { len, mut filled } => {
                    if filled == 0 {
                        if len > MAX_MESSAGE_SIZE {
                            return Err(StrError::new("message too large"))?;
                        }
                        self.buf.clear();
                        self.buf.reserve(len);
                        unsafe { self.buf.set_len(len) };
                    }
                    let n = self.stream.read(&mut self.buf[filled..]).await?;
                    if n == 0 {
                        return Err(Error::from(ErrorKind::UnexpectedEof))?;
                    }
                    filled += n;
                    debug_assert!(filled <= len);
                    if filled == len {
                        self.state = State::ReadLen { filled: 0 };
                        return Ok(Some(bincode::deserialize(&self.buf)?));
                    } else {
                        self.state = State::ReadPayload { len, filled };
                    }
                }
            }
        }
    }
}

pub struct Writer<T>
where
    T: AsyncWrite + Unpin,
{
    stream: T,
}

impl<T> Writer<T>
where
    T: AsyncWrite + Unpin,
{
    pub fn new(stream: T) -> Self {
        Self { stream }
    }

    pub fn unwrap(self) -> T {
        self.stream
    }

    #[conerror]
    pub async fn write(&mut self, cmd: &Command) -> conerror::Result<()> {
        let bytes = bincode::serialize(cmd)?;
        let len = bytes.len() as u32;
        self.stream.write_all(&len.to_be_bytes()).await?;
        self.stream.write_all(&bytes).await?;
        Ok(())
    }
}

pub struct Connection<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    pub input: Reader<ReadHalf<T>>,
    pub output: Writer<WriteHalf<T>>,
}

impl<T> Connection<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(stream: T) -> Self {
        let (r, w) = split(stream);
        Self {
            input: Reader::new(r),
            output: Writer::new(w),
        }
    }

    pub fn unwrap(self) -> T {
        self.input.unwrap().unsplit(self.output.unwrap())
    }
}

#[conerror]
pub async fn unexpected_cmd(stream: &mut Writer<impl AsyncWrite + Unpin>, cmd: &Command) -> conerror::Result<()> {
    error!("unexpected cmd: {:?}", cmd);
    stream.write(&Command::Error(StrError::new("unexpected cmd"))).await?;
    Ok(())
}
