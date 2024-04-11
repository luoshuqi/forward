use std::future::Future;
use std::io::Error;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::spawn;

pub trait AsyncDrop {
    fn async_drop(self) -> impl Future<Output = ()> + Send + 'static;
}

pub struct Dropper<T>(Option<T>)
where
    T: AsyncDrop;

impl<T> Dropper<T>
where
    T: AsyncDrop,
{
    pub fn new(v: T) -> Self {
        Self(Some(v))
    }
}

impl<T> Deref for Dropper<T>
where
    T: AsyncDrop,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("use after drop")
    }
}

impl<T> DerefMut for Dropper<T>
where
    T: AsyncDrop,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("use after drop")
    }
}

impl<T> AsyncRead for Dropper<T>
where
    T: AsyncRead + AsyncDrop + Unpin,
{
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut **self).poll_read(cx, buf)
    }
}

impl<T> AsyncWrite for Dropper<T>
where
    T: AsyncWrite + AsyncDrop + Unpin,
{
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Error>> {
        Pin::new(&mut **self).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut **self).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut **self).poll_shutdown(cx)
    }
}

impl<T> Drop for Dropper<T>
where
    T: AsyncDrop,
{
    fn drop(&mut self) {
        if let Some(v) = self.0.take() {
            spawn(v.async_drop());
        }
    }
}

impl AsyncDrop for TcpStream {
    fn async_drop(mut self) -> impl Future<Output = ()> + Send + 'static {
        async move {
            let _ = self.shutdown().await;
        }
    }
}

impl<T> AsyncDrop for tokio_rustls::server::TlsStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    fn async_drop(mut self) -> impl Future<Output = ()> + Send + 'static {
        async move {
            let _ = self.shutdown().await;
        }
    }
}

impl<T> AsyncDrop for tokio_rustls::client::TlsStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    fn async_drop(mut self) -> impl Future<Output = ()> + Send + 'static {
        async move {
            let _ = self.shutdown().await;
        }
    }
}
