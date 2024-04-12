use crate::util::async_drop::{AsyncDrop, Dropper};
use crate::util::select::{guard_select, Either};
use conerror::conerror;
use std::io;
use std::mem::forget;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const BUF_SIZE: usize = 8192;

#[conerror]
pub async fn copy_bidirectional(
    mut a: Dropper<impl AsyncRead + AsyncWrite + Unpin + AsyncDrop>,
    mut b: Dropper<impl AsyncRead + AsyncWrite + Unpin + AsyncDrop>,
) -> conerror::Result<(usize, usize)> {
    let mut a_eof = false;
    let mut b_eof = false;
    let mut a_to_b = 0;
    let mut b_to_a = 0;
    let mut a_buf = Vec::with_capacity(BUF_SIZE);
    let mut b_buf = Vec::with_capacity(BUF_SIZE);
    unsafe {
        a_buf.set_len(BUF_SIZE);
        b_buf.set_len(BUF_SIZE);
    };

    while !a_eof || !b_eof {
        match guard_select((a.read(&mut a_buf), !a_eof), (b.read(&mut b_buf), !b_eof)).await {
            Either::Left(n) => {
                let n = ignore_unexpected_eof(n)?;
                if n == 0 {
                    a_eof = true;
                    let _ = b.shutdown().await;
                } else {
                    b.write_all(&a_buf[..n]).await?;
                    a_to_b += n;
                }
            }
            Either::Right(n) => {
                let n = ignore_unexpected_eof(n)?;
                if n == 0 {
                    b_eof = true;
                    let _ = a.shutdown().await;
                } else {
                    a.write_all(&b_buf[..n]).await?;
                    b_to_a += n;
                }
            }
        }
    }

    forget(a);
    forget(b);
    Ok((a_to_b, b_to_a))
}

fn ignore_unexpected_eof(v: io::Result<usize>) -> io::Result<usize> {
    match v {
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
        _ => v,
    }
}
