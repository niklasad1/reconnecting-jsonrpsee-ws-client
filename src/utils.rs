use futures::{stream::FuturesUnordered, Future, Stream, StreamExt};
use std::{
    pin::Pin,
    task::{Context, Poll, Waker},
};

/// A wrapper around `FuturesUnordered` that doesn't return `None` when it's empty.
pub struct MaybePendingFutures<Fut> {
    futs: FuturesUnordered<Fut>,
    waker: Option<Waker>,
}

impl<Fut> MaybePendingFutures<Fut> {
    pub fn new() -> Self {
        Self {
            futs: FuturesUnordered::new(),
            waker: None,
        }
    }

    pub fn push(&mut self, fut: Fut) {
        self.futs.push(fut);

        if let Some(w) = self.waker.take() {
            w.wake();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.futs.is_empty()
    }
}

impl<Fut: Future> Stream for MaybePendingFutures<Fut> {
    type Item = Fut::Output;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.futs.is_empty() {
            self.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }

        self.futs.poll_next_unpin(cx)
    }
}
