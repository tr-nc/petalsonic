//! In-process latest-value publication with control-side destruction.

use crossbeam_channel::{Receiver, Sender, TrySendError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublishError {
    Disconnected,
}

struct Shared<T> {
    latest: Mutex<Option<Arc<T>>>,
    consumer_connected: AtomicBool,
}

/// Constructs the two concrete endpoints of a latest-value publication.
///
/// This is deliberately an in-process module rather than a generic transport
/// seam. A publisher may replace an unconsumed value on its own thread. A
/// realtime consumer never destroys a value it has consumed: replacement and
/// rejection are returned through the bounded retirement path and destruction
/// happens when the publisher drains it on a control thread.
pub(crate) struct RealtimeLatest<T>(std::marker::PhantomData<T>);

impl<T> RealtimeLatest<T> {
    pub(crate) fn bounded(retirement_capacity: usize) -> (Publisher<T>, RealtimeConsumer<T>) {
        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            consumer_connected: AtomicBool::new(true),
        });
        let (retirement_sender, retirement_receiver) =
            crossbeam_channel::bounded(retirement_capacity);
        (
            Publisher {
                shared: shared.clone(),
                retirement_receiver,
            },
            RealtimeConsumer {
                shared,
                retirement_sender,
                pending_retirement: None,
            },
        )
    }
}

/// The producer/control endpoint. Clones publish into the same latest slot and
/// may drain the same retirement queue; the owning control module decides which
/// clone performs draining.
pub(crate) struct Publisher<T> {
    shared: Arc<Shared<T>>,
    retirement_receiver: Receiver<Arc<T>>,
}

impl<T> Clone for Publisher<T> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            retirement_receiver: self.retirement_receiver.clone(),
        }
    }
}

impl<T> Publisher<T> {
    pub(crate) fn publish(&self, value: Arc<T>) -> Result<(), PublishError> {
        if !self.shared.consumer_connected.load(Ordering::Acquire) {
            return Err(PublishError::Disconnected);
        }
        let mut latest = self
            .shared
            .latest
            .lock()
            .map_err(|_| PublishError::Disconnected)?;
        if !self.shared.consumer_connected.load(Ordering::Acquire) {
            return Err(PublishError::Disconnected);
        }
        *latest = Some(value);
        Ok(())
    }

    /// Destroys every returned value on the calling control thread.
    pub(crate) fn drain_retired(&self) -> usize {
        self.retirement_receiver.try_iter().count()
    }

    /// Closes publication and destroys an unconsumed latest value on the
    /// calling control thread. Consumed values remain owned by the consumer
    /// until its non-realtime owner shuts down.
    pub(crate) fn close(&self) {
        self.shared
            .consumer_connected
            .store(false, Ordering::Release);
        if let Ok(mut latest) = self.shared.latest.lock() {
            latest.take();
        }
        self.drain_retired();
    }

    #[cfg(test)]
    pub(crate) fn latest(&self) -> Option<Arc<T>> {
        self.shared.latest.lock().ok()?.clone()
    }
}

impl<T> Drop for RealtimeConsumer<T> {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// The realtime endpoint. It owns at most one pending retirement and refuses
/// to consume another latest value while that retirement cannot be returned.
pub(crate) struct RealtimeConsumer<T> {
    shared: Arc<Shared<T>>,
    retirement_sender: Sender<Arc<T>>,
    pending_retirement: Option<Arc<T>>,
}

impl<T> RealtimeConsumer<T> {
    pub(crate) fn consume(&mut self) -> Option<Arc<T>> {
        if let Some(pending) = self.pending_retirement.take() {
            match self.retirement_sender.try_send(pending) {
                Ok(()) => {}
                Err(TrySendError::Full(pending) | TrySendError::Disconnected(pending)) => {
                    self.pending_retirement = Some(pending);
                    return None;
                }
            }
        }
        if !self.shared.consumer_connected.load(Ordering::Acquire) {
            return None;
        }
        self.shared.latest.try_lock().ok()?.take()
    }

    /// Returns a consumed value without ever destroying it on the realtime
    /// thread. `consume` cannot produce another value while this one is pending.
    pub(crate) fn retire(&mut self, value: Arc<T>) {
        assert!(
            self.pending_retirement.is_none(),
            "consume must flush a pending retirement before producing another value"
        );
        if let Err(error) = self.retirement_sender.try_send(value) {
            self.pending_retirement = Some(error.into_inner());
        }
    }

    /// Prevents future publication. Payload destruction is intentionally left
    /// to the control owner after the render thread has joined.
    pub(crate) fn disconnect(&mut self) {
        self.shared
            .consumer_connected
            .store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn replace_before_consume_observes_only_the_latest_value() {
        let (publisher, mut consumer) = RealtimeLatest::bounded(1);
        publisher.publish(Arc::new(1)).unwrap();
        publisher.publish(Arc::new(2)).unwrap();

        assert_eq!(consumer.consume().as_deref(), Some(&2));
        assert!(consumer.consume().is_none());
    }

    #[test]
    fn full_retirement_defers_the_next_consume_until_control_drains() {
        let (publisher, mut consumer) = RealtimeLatest::bounded(1);
        publisher.publish(Arc::new(1)).unwrap();
        let first = consumer.consume().unwrap();
        consumer.retire(first);
        publisher.publish(Arc::new(2)).unwrap();
        let second = consumer.consume().unwrap();
        consumer.retire(second);
        publisher.publish(Arc::new(3)).unwrap();

        assert!(consumer.consume().is_none());
        assert_eq!(publisher.drain_retired(), 1);
        assert_eq!(consumer.consume().as_deref(), Some(&3));
        assert_eq!(publisher.drain_retired(), 1);
    }

    #[test]
    fn disconnected_consumer_rejects_publication() {
        let (publisher, mut consumer) = RealtimeLatest::<usize>::bounded(1);
        consumer.disconnect();

        assert_eq!(
            publisher.publish(Arc::new(1)),
            Err(PublishError::Disconnected)
        );
    }

    #[test]
    fn realtime_replacement_never_destroys_a_value() {
        let drops = Arc::new(AtomicUsize::new(0));
        let (publisher, mut consumer) = RealtimeLatest::bounded(1);
        publisher
            .publish(Arc::new(DropProbe(drops.clone())))
            .unwrap();
        let first = consumer.consume().unwrap();
        consumer.retire(first);

        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert_eq!(publisher.drain_retired(), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn shutdown_rejects_publication_and_destroys_unconsumed_on_control() {
        let drops = Arc::new(AtomicUsize::new(0));
        let (publisher, _consumer) = RealtimeLatest::bounded(1);
        publisher
            .publish(Arc::new(DropProbe(drops.clone())))
            .unwrap();

        publisher.close();

        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_eq!(
            publisher.publish(Arc::new(DropProbe(drops.clone()))),
            Err(PublishError::Disconnected)
        );
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }
}
