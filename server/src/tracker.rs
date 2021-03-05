use hashbrown::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use futures::{future::BoxFuture, prelude::*};
use pin_project::pin_project;
use tokio_util::sync::CancellationToken;
use std::time::{SystemTime, Instant};

/// Every future registered with a `TrackerRegistry` is assigned a unique
/// `TrackerId`
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct TrackerId(usize);

#[derive(Debug)]
pub struct TrackerSlot<T> {
    tracker: Arc<Tracker>,
    metadata: T,
}

impl <T> TrackerSlot<T> {
    fn finished(&self) -> bool {
        Arc::strong_count(&self.tracker) == 1
    }
}

#[derive(Debug)]
struct Tracker {
    start_time: SystemTime,
    poll_nanos: AtomicUsize,
    cancel_token: CancellationToken,
}

impl <T> TrackerSlot<T> {
    pub fn cancel(&self) {
        self.tracker.cancel_token.cancel();
    }

    pub fn metadata(&self) -> &T {
        &self.metadata
    }
}

/// Allows tracking the lifecycle of futures registered by
/// `TrackedFutureExt::track` with an accompanying metadata payload of type T
///
/// Additionally can trigger graceful cancellation of registered futures
#[derive(Debug)]
pub struct TrackerRegistry<T> {
    next_id: AtomicUsize,
    trackers: Mutex<HashMap<TrackerId, TrackerSlot<T>>>,
}

impl<T> Default for TrackerRegistry<T> {
    fn default() -> Self {
        Self {
            next_id: AtomicUsize::new(0),
            trackers: Default::default(),
        }
    }
}

impl<T> TrackerRegistry<T> {
    pub fn new() -> Self {
        Default::default()
    }

    /// Trigger graceful termination of a registered future
    ///
    /// Returns false if no future found with the provided ID
    ///
    /// Note: If the future is currently executing, termination
    /// will only occur when the future yields (returns from poll)
    #[allow(dead_code)]
    pub fn cancel(&self, id: TrackerId) -> bool {
        if let Some(meta) = self.trackers.lock().get_mut(&id) {
            meta.cancel();
            return true
        }
        false
    }

    /// Register a new tracker in the registry
    pub fn register(&self, metadata: T) -> TrackerRegistration {
        let id = TrackerId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let token = CancellationToken::new();

        let tracker = Arc::new(Tracker {
            start_time: SystemTime::now(),
            poll_nanos: AtomicUsize::new(0),
            cancel_token: token.clone(),
        });

        self.trackers.lock().insert(
            id,
            TrackerSlot {
                metadata,
                tracker: tracker.clone()
            }
        );

        TrackerRegistration {
            token,
            tracker
        }
    }

    /// Removes completed tasks from the registry and returns a list of them
    pub fn reclaim(&self) -> Vec<(TrackerId, T)> {
        self.trackers.lock().drain_filter(|_, v| v.finished()).map(|(id, data)| ((id, data.metadata))).collect()
    }
}

impl<T: Clone> TrackerRegistry<T> {
    /// Returns a list of trackers, including those that are
    #[allow(dead_code)]
    pub fn tracked(&self) -> Vec<(TrackerId, T)> {
        // TODO: Improve this - (#711)
        self.trackers.lock().iter().map(|(id, data)| (*id, data.metadata.clone())).collect()
    }

    /// Returns a list of active trackers
    #[allow(dead_code)]
    pub fn running(&self) -> Vec<(TrackerId, T)> {
        // TODO: Improve this - (#711)
        self.trackers.lock().iter().filter_map(|(id, data)| {
            if data.finished() {
                return None
            }
            Some((*id, data.metadata.clone()))
        }).collect()
    }
}

/// A registration returned by a `TrackerRegistry` that can be associated with a future
#[derive(Debug, Clone)]
pub struct TrackerRegistration {
    tracker: Arc<Tracker>,
    token: CancellationToken,
}

/// An extension trait that provides `self.track(registration)` allowing
/// associating this future with a `TrackerRegistration`
pub trait TrackedFutureExt: Future {
    fn track(self, reg: TrackerRegistration) -> TrackedFuture<Self>
        where
            Self: Sized,
    {
        let token = reg.token;

        // The future returned by CancellationToken::cancelled borrows the token
        // In order to ensure we get a future with a static lifetime
        // we box them up together and let async work its magic
        let abort = Box::pin(async move {
            token.cancelled().await
        });

        TrackedFuture {
            inner: self,
            abort,
            tracker: reg.tracker
        }
    }
}

impl<T: ?Sized> TrackedFutureExt for T where T: Future {}

/// The `Future` returned by `TrackedFutureExt::track()`
/// Unregisters the future from the registered `TrackerRegistry` on drop
/// and provides the early termination functionality used by
/// `TrackerRegistry::terminate`
#[pin_project]
pub struct TrackedFuture<F: Future> {
    #[pin]
    inner: F,
    #[pin]
    abort: BoxFuture<'static, ()>,
    tracker: Arc<Tracker>,
}

impl<F: Future> Future for TrackedFuture<F> {
    type Output = Result<F::Output, future::Aborted>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.as_mut().project().abort.poll(cx).is_ready() {
            return Poll::Ready(Err(future::Aborted{}))
        }

        let start = Instant::now();
        let poll = self.as_mut().project().inner.poll(cx);
        let delta = start.elapsed().as_nanos() as usize;

        self.tracker.poll_nanos.fetch_add(delta, Ordering::Relaxed);

        poll.map(|x| Ok(x))
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use super::*;

    #[tokio::test]
    async fn test_lifecycle() {
        let (sender, receive) = oneshot::channel();
        let registry = TrackerRegistry::new();
        let registration = registry.register(());

        let task = tokio::spawn(receive.track(registration));

        assert_eq!(registry.running().len(), 1);

        sender.send(()).unwrap();
        task.await.unwrap().unwrap().unwrap();

        assert_eq!(registry.running().len(), 0);
    }

    #[tokio::test]
    async fn test_interleaved() {
        let (sender1, receive1) = oneshot::channel();
        let (sender2, receive2) = oneshot::channel();
        let registry = TrackerRegistry::new();
        let registration1 = registry.register(1);
        let registration2 = registry.register(2);

        let task1 = tokio::spawn(receive1.track(registration1));
        let task2 = tokio::spawn(receive2.track(registration2));

        let mut tracked: Vec<_> = registry.tracked().iter().map(|x| x.1).collect();
        tracked.sort_unstable();
        assert_eq!(tracked, vec![1, 2]);

        sender2.send(()).unwrap();
        task2.await.unwrap().unwrap().unwrap();

        let tracked: Vec<_> = registry.tracked().iter().map(|x| x.1).collect();
        assert_eq!(tracked, vec![1]);

        sender1.send(42).unwrap();
        let ret = task1.await.unwrap().unwrap().unwrap();

        assert_eq!(ret, 42);
        assert_eq!(registry.running().len(), 0);
    }

    #[tokio::test]
    async fn test_drop() {
        let registry = TrackerRegistry::new();
        let registration = registry.register(());

        {
            let f = futures::future::pending::<()>().track(registration);

            assert_eq!(registry.running().len(), 1);

            std::mem::drop(f);
        }

        assert_eq!(registry.running().len(), 0);
    }

    #[tokio::test]
    async fn test_drop_multiple() {
        let registry = TrackerRegistry::new();
        let registration = registry.register(());

        {
            let f = futures::future::pending::<()>().track(registration.clone());
            {
                let f = futures::future::pending::<()>().track(registration);
                assert_eq!(registry.running().len(), 1);
                std::mem::drop(f);
            }
            assert_eq!(registry.running().len(), 1);
            std::mem::drop(f);
        }

        assert_eq!(registry.running().len(), 0);
    }

    #[tokio::test]
    async fn test_terminate() {
        let registry = TrackerRegistry::new();
        let registration = registry.register(());

        let task = tokio::spawn(futures::future::pending::<()>().track(registration));

        let tracked = registry.running();
        assert_eq!(tracked.len(), 1);

        registry.cancel(tracked[0].0);
        let result = task.await.unwrap();

        assert!(result.is_err());
        assert_eq!(registry.running().len(), 0);
    }

    #[tokio::test]
    async fn test_terminate_multiple() {
        let registry = TrackerRegistry::new();
        let registration = registry.register(());

        let task1 = tokio::spawn(futures::future::pending::<()>().track(registration.clone()));
        let task2 = tokio::spawn(futures::future::pending::<()>().track(registration));

        let tracked = registry.running();
        assert_eq!(tracked.len(), 1);

        registry.cancel(tracked[0].0);

        let result1 = task1.await.unwrap();
        let result2 = task2.await.unwrap();

        assert!(result1.is_err());
        assert!(result2.is_err());
        assert_eq!(registry.running().len(), 0);
    }
}
