//! Event Bus (Layer 5)
//!
//! Decouples state changes from subscribers.
//! State Machine emits events, Event Bus broadcasts to all subscribers.

use tokio::sync::broadcast;

/// Event Bus for broadcasting state machine events to subscribers
///
/// The Event Bus receives events from the State Machine (Layer 4) and
/// broadcasts them to multiple subscribers. This decouples event producers
/// from consumers, allowing multiple listeners to react to state changes.
///
/// # Type Parameters
/// * `E` - Event type (must be Clone for broadcasting)
pub struct EventBus<E> {
    /// Broadcast channel sender
    tx: broadcast::Sender<E>,
}

impl<E: Clone> EventBus<E> {
    /// Create a new EventBus with the specified channel capacity
    ///
    /// # Arguments
    /// * `capacity` - Maximum number of events to buffer per subscriber
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publish an event to all subscribers
    ///
    /// If a subscriber's buffer is full, the oldest event will be dropped.
    ///
    /// # Arguments
    /// * `event` - The event to broadcast
    ///
    /// # Returns
    /// Number of subscribers that received the event
    pub fn publish(&self, event: E) -> usize {
        match self.tx.send(event) {
            Ok(receiver_count) => receiver_count,
            Err(_) => 0, // No subscribers
        }
    }

    /// Publish multiple events to all subscribers
    ///
    /// # Arguments
    /// * `events` - Vector of events to broadcast
    ///
    /// # Returns
    /// Total number of event deliveries across all subscribers
    pub fn publish_batch(&self, events: Vec<E>) -> usize {
        let mut total = 0;
        for event in events {
            total += self.publish(event);
        }
        total
    }

    /// Subscribe to events
    ///
    /// Returns a receiver that will receive all future events.
    /// The receiver uses a bounded buffer, so if events arrive faster
    /// than they can be processed, old events will be dropped.
    pub fn subscribe(&self) -> broadcast::Receiver<E> {
        self.tx.subscribe()
    }

    /// Get the number of active subscribers
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl<E: Clone> Default for EventBus<E> {
    fn default() -> Self {
        Self::new(100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    enum TestEvent {
        Created { id: u64 },
        Updated { id: u64, value: String },
        Deleted { id: u64 },
    }

    #[tokio::test]
    async fn test_event_bus_publish_subscribe() {
        let bus = EventBus::new(10);

        // Subscribe before publishing
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        // Publish event
        let event = TestEvent::Created { id: 1 };
        let count = bus.publish(event.clone());

        assert_eq!(count, 2); // Two subscribers

        // Both subscribers should receive it
        assert_eq!(rx1.recv().await.unwrap(), event);
        assert_eq!(rx2.recv().await.unwrap(), event);
    }

    #[tokio::test]
    async fn test_event_bus_no_subscribers() {
        let bus: EventBus<TestEvent> = EventBus::new(10);

        // Publish with no subscribers
        let event = TestEvent::Created { id: 1 };
        let count = bus.publish(event);

        assert_eq!(count, 0); // No subscribers
    }

    #[tokio::test]
    async fn test_event_bus_late_subscriber() {
        let bus = EventBus::new(10);

        // Publish before subscribing
        bus.publish(TestEvent::Created { id: 1 });

        // Late subscriber won't receive past events
        let mut rx = bus.subscribe();

        // But will receive future events
        let future_event = TestEvent::Updated {
            id: 1,
            value: "new".to_string(),
        };
        bus.publish(future_event.clone());

        assert_eq!(rx.recv().await.unwrap(), future_event);
    }

    #[tokio::test]
    async fn test_event_bus_publish_batch() {
        let bus = EventBus::new(10);
        let mut rx = bus.subscribe();

        let events = vec![
            TestEvent::Created { id: 1 },
            TestEvent::Updated {
                id: 1,
                value: "foo".to_string(),
            },
            TestEvent::Deleted { id: 1 },
        ];

        let total_deliveries = bus.publish_batch(events.clone());
        assert_eq!(total_deliveries, 3); // 1 subscriber × 3 events

        // Receive all events
        for expected_event in events {
            let received = rx.recv().await.unwrap();
            assert_eq!(received, expected_event);
        }
    }

    #[tokio::test]
    async fn test_event_bus_multiple_subscribers() {
        let bus = EventBus::new(10);

        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        let mut rx3 = bus.subscribe();

        assert_eq!(bus.subscriber_count(), 3);

        let event = TestEvent::Created { id: 42 };
        let count = bus.publish(event.clone());

        assert_eq!(count, 3);

        // All three subscribers receive the event
        assert_eq!(rx1.recv().await.unwrap(), event);
        assert_eq!(rx2.recv().await.unwrap(), event);
        assert_eq!(rx3.recv().await.unwrap(), event);
    }

    #[tokio::test]
    async fn test_event_bus_subscriber_drop() {
        let bus = EventBus::new(10);

        let rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        assert_eq!(bus.subscriber_count(), 2);

        // Drop one subscriber
        drop(rx1);

        let event = TestEvent::Created { id: 1 };
        let count = bus.publish(event.clone());

        assert_eq!(count, 1); // Only one subscriber now

        assert_eq!(rx2.recv().await.unwrap(), event);
    }
}
