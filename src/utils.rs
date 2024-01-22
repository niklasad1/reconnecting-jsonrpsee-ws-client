//! Utils.

use futures::{stream::FuturesUnordered, Future, Stream, StreamExt};
use std::{
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    sync::Arc,
    task::{Context, Poll, Waker},
};
use tokio::sync::Notify;

#[derive(Default, Debug)]
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

    pub fn len(&self) -> usize {
        self.futs.len()
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

#[derive(Clone, Debug)]
pub struct ReconnectCounter(Arc<AtomicUsize>);

impl Default for ReconnectCounter {
    fn default() -> Self {
        Self::new()
    }
}

impl ReconnectCounter {
    pub fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    pub fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

pub fn reconnect_channel() -> (ReconnectTx, ReconnectRx) {
    let count = ReconnectCounter::new();
    let notify = Arc::new(Notify::new());
    (
        ReconnectTx {
            inner: notify.clone(),
            count: count.clone(),
        },
        ReconnectRx {
            inner: notify,
            count,
        },
    )
}

#[derive(Debug, Clone)]
pub struct ReconnectTx {
    inner: Arc<Notify>,
    count: ReconnectCounter,
}

impl ReconnectTx {
    pub fn reconnect(&self) {
        self.inner.notify_one();
        self.count.inc();
    }

    pub fn count(&self) -> usize {
        self.count.get()
    }
}

#[derive(Debug, Clone)]
pub struct ReconnectRx {
    inner: Arc<Notify>,
    count: ReconnectCounter,
}

impl ReconnectRx {
    pub async fn on_reconnect(&self) {
        self.inner.notified().await;
    }

    pub fn count(&self) -> usize {
        self.count.get()
    }
}
