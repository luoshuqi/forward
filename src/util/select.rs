use std::future::{poll_fn, Future, IntoFuture};
use std::pin::pin;
use std::task::{ready, Poll};

#[derive(Debug)]
pub enum Either<T, U> {
    Left(T),
    Right(U),
}

impl<T, U> Either<T, U> {
    pub fn right(self) -> Option<U> {
        match self {
            Either::Right(v) => Some(v),
            _ => None,
        }
    }
}

pub async fn select<T, U>(a: impl IntoFuture<Output = T>, b: impl IntoFuture<Output = U>) -> Either<T, U> {
    let mut a = pin!(a.into_future());
    let mut b = pin!(b.into_future());
    poll_fn(|cx| match a.as_mut().poll(cx) {
        Poll::Ready(v) => Either::Left(v).into(),
        Poll::Pending => Either::Right(ready!(b.as_mut().poll(cx))).into(),
    })
    .await
}
