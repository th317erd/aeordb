use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use tokio::sync::{broadcast, mpsc, Notify};

use crate::engine::engine_event::EngineEvent;

const DEFAULT_CHANNEL_CAPACITY: usize = 1024;
const DEFAULT_PROJECTED_CHANNEL_BYTE_CAPACITY: usize = 1024 * 1024;
const DEFAULT_MAXIMUM_PROJECTED_SUBSCRIBERS: usize = 256;

type EventProjector = dyn Fn(&EngineEvent) -> Option<EngineEvent> + Send + Sync + 'static;

fn lock_projected_state<'state, T>(state: &'state Mutex<T>, state_name: &'static str) -> MutexGuard<'state, T> {
  match state.lock() {
    Ok(guard) => guard,
    Err(poisoned) => {
      tracing::error!(state_name, "Recovered poisoned projected event state");
      state.clear_poison();
      poisoned.into_inner()
    }
  }
}

struct EventBusInner {
  sender: broadcast::Sender<EngineEvent>,
  projected_subscribers: Mutex<HashMap<u64, Arc<ProjectedSubscriber>>>,
  next_projected_subscriber_id: AtomicU64,
  projected_channel_capacity: usize,
  projected_channel_byte_capacity: usize,
  maximum_projected_subscribers: usize,
}

struct ProjectedSubscriber {
  projector: Arc<EventProjector>,
  sender: mpsc::Sender<RetainedProjectedEvent>,
  delivery_state: Arc<Mutex<ProjectedDeliveryState>>,
  gap_notification: Arc<Notify>,
  maximum_retained_bytes: usize,
}

#[derive(Default)]
struct ProjectedDeliveryState {
  retained_bytes: usize,
  missed_events: u64,
}

struct RetainedProjectedEvent {
  event: EngineEvent,
  retained_bytes: usize,
}

/// One delivery from a subscriber-private projected event queue.
#[derive(Debug, Clone)]
pub enum ProjectedEvent {
  Event(EngineEvent),
  Gap { missed_events: u64 },
}

/// Receiver for a subscriber-private, item-and-byte-bounded event queue.
/// Dropping the receiver immediately deregisters its projection closure.
pub struct ProjectedEventReceiver {
  receiver: mpsc::Receiver<RetainedProjectedEvent>,
  delivery_state: Arc<Mutex<ProjectedDeliveryState>>,
  gap_notification: Arc<Notify>,
  event_bus: Weak<EventBusInner>,
  subscriber_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectedSubscriptionError {
  MaximumSubscribersReached { maximum_subscribers: usize },
}

impl std::fmt::Display for ProjectedSubscriptionError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::MaximumSubscribersReached { maximum_subscribers } => {
        write!(formatter, "maximum projected event subscribers reached ({maximum_subscribers})")
      }
    }
  }
}

impl std::error::Error for ProjectedSubscriptionError {}

impl ProjectedEventReceiver {
  /// Receive the next projected event or a local gap notification.
  ///
  /// A gap count includes every authorized queued event discarded because a
  /// refresh supersedes it, as well as the authorized event that crossed a
  /// queue bound and every authorized event rejected while the gap was pending.
  pub async fn receive(&mut self) -> Option<ProjectedEvent> {
    loop {
      if let Some(gap) = self.take_pending_gap() {
        return Some(gap);
      }

      tokio::select! {
        retained = self.receiver.recv() => {
          let Some(retained) = retained else {
            return self.take_pending_gap();
          };
          let mut delivery_state = lock_projected_state(&self.delivery_state, "delivery");
          delivery_state.retained_bytes = delivery_state.retained_bytes.saturating_sub(retained.retained_bytes);
          return Some(ProjectedEvent::Event(retained.event));
        }
        () = self.gap_notification.notified() => {}
      }
    }
  }

  fn take_pending_gap(&mut self) -> Option<ProjectedEvent> {
    let mut delivery_state = lock_projected_state(&self.delivery_state, "delivery");
    if delivery_state.missed_events == 0 {
      return None;
    }

    let mut missed_events = delivery_state.missed_events;
    let receiver = &mut self.receiver;
    let retained_events = std::iter::from_fn(|| match receiver.try_recv() {
      Ok(retained) => Some(retained),
      Err(mpsc::error::TryRecvError::Empty) => None,
      Err(mpsc::error::TryRecvError::Disconnected) => None,
    });
    for retained in retained_events {
      delivery_state.retained_bytes = delivery_state.retained_bytes.saturating_sub(retained.retained_bytes);
      missed_events = missed_events.saturating_add(1);
    }
    delivery_state.missed_events = 0;
    Some(ProjectedEvent::Gap { missed_events })
  }
}

impl Drop for ProjectedEventReceiver {
  fn drop(&mut self) {
    self.receiver.close();
    let Some(event_bus) = self.event_bus.upgrade() else {
      return;
    };
    lock_projected_state(&event_bus.projected_subscribers, "subscriber registry").remove(&self.subscriber_id);
  }
}

/// Central event bus for distributing engine events to subscribers.
/// Preserves a raw Tokio broadcast for trusted internal consumers and supports
/// subscriber-private bounded queues whose projections run before retention.
#[derive(Clone)]
pub struct EventBus {
  inner: Arc<EventBusInner>,
}

impl EventBus {
  pub fn new() -> Self {
    Self::with_capacity(DEFAULT_CHANNEL_CAPACITY)
  }

  pub fn with_capacity(capacity: usize) -> Self {
    Self::with_projected_delivery_limits(capacity, DEFAULT_PROJECTED_CHANNEL_BYTE_CAPACITY, DEFAULT_MAXIMUM_PROJECTED_SUBSCRIBERS)
  }

  pub fn with_projected_delivery_limits(
    channel_capacity: usize,
    projected_channel_byte_capacity: usize,
    maximum_projected_subscribers: usize,
  ) -> Self {
    let (sender, _) = broadcast::channel(channel_capacity);
    EventBus {
      inner: Arc::new(EventBusInner {
        sender,
        projected_subscribers: Mutex::new(HashMap::new()),
        next_projected_subscriber_id: AtomicU64::new(1),
        projected_channel_capacity: channel_capacity,
        projected_channel_byte_capacity,
        maximum_projected_subscribers,
      }),
    }
  }

  /// Emit an event to all subscribers without awaiting downstream consumers.
  /// Raw subscribers retain the original event. Each projected subscriber can
  /// retain only the event returned by its projection closure.
  pub fn emit(&self, event: EngineEvent) {
    if self.inner.sender.send(event.clone()).is_err() {
      debug_assert_eq!(self.inner.sender.receiver_count(), 0, "broadcast send only fails when no receivers remain");
    }

    let subscribers: Vec<(u64, Arc<ProjectedSubscriber>)> = lock_projected_state(&self.inner.projected_subscribers, "subscriber registry")
      .iter()
      .map(|(subscriber_id, subscriber)| (*subscriber_id, Arc::clone(subscriber)))
      .collect();
    let mut closed_subscriber_ids = Vec::new();

    for (subscriber_id, subscriber) in subscribers {
      let projected = match std::panic::catch_unwind(AssertUnwindSafe(|| (subscriber.projector)(&event))) {
        Ok(projected) => projected,
        Err(panic_evidence) => {
          let panic_message = if let Some(message) = panic_evidence.downcast_ref::<&str>() {
            *message
          } else if let Some(message) = panic_evidence.downcast_ref::<String>() {
            message.as_str()
          } else {
            "non-string panic payload"
          };
          tracing::error!(subscriber_id, event_type = %event.event_type, panic_message, "Projected event subscriber panicked; event failed closed");
          continue;
        }
      };
      let Some(projected) = projected else {
        continue;
      };
      if !subscriber.try_deliver(projected) {
        closed_subscriber_ids.push(subscriber_id);
      }
    }

    if !closed_subscriber_ids.is_empty() {
      let mut projected_subscribers = lock_projected_state(&self.inner.projected_subscribers, "subscriber registry");
      for subscriber_id in closed_subscriber_ids {
        projected_subscribers.remove(&subscriber_id);
      }
    }
  }

  /// Subscribe to events. Returns a broadcast Receiver.
  /// If the subscriber falls behind, it receives Lagged(n).
  pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
    self.inner.sender.subscribe()
  }

  /// Subscribe through a private bounded queue. Events rejected by the
  /// projection closure never consume this subscriber's queue capacity.
  pub fn subscribe_projected<F>(&self, projector: F) -> Result<ProjectedEventReceiver, ProjectedSubscriptionError>
  where
    F: Fn(&EngineEvent) -> Option<EngineEvent> + Send + Sync + 'static,
  {
    self.subscribe_projected_with_limits(self.inner.projected_channel_capacity, self.inner.projected_channel_byte_capacity, projector)
  }

  /// Subscribe through a private queue with explicit item and serialized-byte
  /// retention limits. Intended for bounded delivery surfaces and their tests.
  pub fn subscribe_projected_with_limits<F>(
    &self,
    maximum_retained_events: usize,
    maximum_retained_bytes: usize,
    projector: F,
  ) -> Result<ProjectedEventReceiver, ProjectedSubscriptionError>
  where
    F: Fn(&EngineEvent) -> Option<EngineEvent> + Send + Sync + 'static,
  {
    assert!(maximum_retained_events > 0, "projected event item capacity must be greater than zero");
    let subscriber_id = self.inner.next_projected_subscriber_id.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = mpsc::channel(maximum_retained_events);
    let delivery_state = Arc::new(Mutex::new(ProjectedDeliveryState::default()));
    let gap_notification = Arc::new(Notify::new());
    let mut projected_subscribers = lock_projected_state(&self.inner.projected_subscribers, "subscriber registry");
    if projected_subscribers.len() >= self.inner.maximum_projected_subscribers {
      return Err(ProjectedSubscriptionError::MaximumSubscribersReached { maximum_subscribers: self.inner.maximum_projected_subscribers });
    }
    projected_subscribers.insert(
      subscriber_id,
      Arc::new(ProjectedSubscriber {
        projector: Arc::new(projector),
        sender,
        delivery_state: Arc::clone(&delivery_state),
        gap_notification: Arc::clone(&gap_notification),
        maximum_retained_bytes,
      }),
    );
    drop(projected_subscribers);

    Ok(ProjectedEventReceiver { receiver, delivery_state, gap_notification, event_bus: Arc::downgrade(&self.inner), subscriber_id })
  }

  /// Get the current number of active raw broadcast subscribers.
  pub fn subscriber_count(&self) -> usize {
    self.inner.sender.receiver_count()
  }

  /// Get the current number of subscriber-private projected queues.
  pub fn projected_subscriber_count(&self) -> usize {
    lock_projected_state(&self.inner.projected_subscribers, "subscriber registry").len()
  }
}

impl ProjectedSubscriber {
  /// Returns false only when the receiver is already closed.
  fn try_deliver(&self, event: EngineEvent) -> bool {
    let retained_bytes = match serde_json::to_vec(&event) {
      Ok(serialized) => serialized.len(),
      Err(error) => {
        tracing::error!(%error, event_type = %event.event_type, "Projected event serialization failed; event failed closed");
        return true;
      }
    };
    let mut delivery_state = lock_projected_state(&self.delivery_state, "delivery");
    if delivery_state.missed_events > 0 {
      delivery_state.missed_events = delivery_state.missed_events.saturating_add(1);
      self.gap_notification.notify_one();
      return true;
    }
    let Some(next_retained_bytes) = delivery_state.retained_bytes.checked_add(retained_bytes) else {
      delivery_state.missed_events = 1;
      self.gap_notification.notify_one();
      return true;
    };
    if next_retained_bytes > self.maximum_retained_bytes {
      delivery_state.missed_events = 1;
      self.gap_notification.notify_one();
      return true;
    }

    match self.sender.try_send(RetainedProjectedEvent { event, retained_bytes }) {
      Ok(()) => {
        delivery_state.retained_bytes = next_retained_bytes;
        true
      }
      Err(mpsc::error::TrySendError::Full(_)) => {
        delivery_state.missed_events = 1;
        self.gap_notification.notify_one();
        true
      }
      Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
  }
}

impl Default for EventBus {
  fn default() -> Self {
    Self::new()
  }
}

impl std::fmt::Debug for EventBus {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("EventBus")
      .field("subscriber_count", &self.subscriber_count())
      .field("projected_subscriber_count", &self.projected_subscriber_count())
      .finish()
  }
}
