//! Fan-out of [`Event`]s to subscribers.
//!
//! The reactor publishes; IPC connections and the menu bar subscribe. A subscriber is a
//! delivery closure that returns `false` when it is gone or too slow, and is then
//! dropped, so a stuck client can never back-pressure the reactor.

use std::sync::Mutex;

use dowm_protocol::{Event, Topic};

type Deliver = Box<dyn FnMut(Event) -> bool + Send>;

struct Subscriber {
    /// Empty means every topic.
    topics: Vec<Topic>,
    deliver: Deliver,
}

#[derive(Default)]
pub struct EventBus {
    subscribers: Mutex<Vec<Subscriber>>,
}

impl EventBus {
    pub fn new() -> Self { Self::default() }

    pub fn subscribe(&self, topics: &[Topic], deliver: impl FnMut(Event) -> bool + Send + 'static) {
        self.lock().push(Subscriber { topics: topics.to_vec(), deliver: Box::new(deliver) });
    }

    pub fn publish(&self, event: Event) {
        let topic = event.topic();
        self.lock().retain_mut(|sub| {
            if !sub.topics.is_empty() && !sub.topics.contains(&topic) {
                return true;
            }
            (sub.deliver)(event.clone())
        });
    }

    pub fn subscriber_count(&self) -> usize { self.lock().len() }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Subscriber>> {
        // A panicking delivery closure must not take the reactor's publishing down too.
        self.subscribers.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    fn ws(name: &str) -> Event { Event::WorkspaceChanged { workspace: name.into(), previous: None } }

    #[test]
    fn subscribers_only_get_the_topics_they_asked_for() {
        let bus = EventBus::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        bus.subscribe(&[Topic::Mode], move |e| {
            sink.lock().unwrap().push(e);
            true
        });
        bus.publish(ws("1"));
        bus.publish(Event::ModeChanged { mode: "resize".into() });
        assert_eq!(*seen.lock().unwrap(), vec![Event::ModeChanged { mode: "resize".into() }]);
    }

    #[test]
    fn an_empty_topic_list_means_everything() {
        let bus = EventBus::new();
        let seen = Arc::new(Mutex::new(0));
        let sink = Arc::clone(&seen);
        bus.subscribe(&[], move |_| {
            *sink.lock().unwrap() += 1;
            true
        });
        bus.publish(ws("1"));
        bus.publish(Event::FocusChanged { window: None });
        assert_eq!(*seen.lock().unwrap(), 2);
    }

    #[test]
    fn a_subscriber_that_reports_failure_is_dropped() {
        let bus = EventBus::new();
        bus.subscribe(&[], |_| false);
        bus.subscribe(&[], |_| true);
        assert_eq!(bus.subscriber_count(), 2);
        bus.publish(ws("1"));
        assert_eq!(bus.subscriber_count(), 1);
    }
}
