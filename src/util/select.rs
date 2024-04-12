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

pub async fn guard_select<T, U>(a: (impl IntoFuture<Output = T>, bool), b: (impl IntoFuture<Output = U>, bool)) -> Either<T, U> {
    let a_guard = a.1;
    let b_guard = b.1;
    if !a_guard && !b_guard {
        panic!("all branches are disabled");
    }

    let mut a = pin!(a.0.into_future());
    let mut b = pin!(b.0.into_future());
    poll_fn(|cx| {
        if a_guard {
            if let Poll::Ready(v) = a.as_mut().poll(cx) {
                return Either::Left(v).into();
            }
        }
        if b_guard {
            if let Poll::Ready(v) = b.as_mut().poll(cx) {
                return Either::Right(v).into();
            }
        }
        Poll::Pending
    })
    .await
}
