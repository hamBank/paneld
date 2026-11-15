//! The notification queue (DESIGN.md §5.5). Pure state, like the scheduler
//! that owns it: time is passed in, nothing sleeps. The semantics are
//! documented in the [`crate::scheduler`] module docs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::color::Color;
use crate::control::{Notification, NotificationContent, QueuedNotification};

/// Most notifications held at once (showing + waiting).
pub const MAX_QUEUE: usize = 50;

/// Inline content: text with an optional icon, in one colour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineNotice {
    pub text: String,
    pub icon: Option<String>,
    pub color: Color,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Scene(String),
    Inline(Arc<InlineNotice>),
}

impl Content {
    fn summary(&self) -> String {
        match self {
            Content::Scene(n) => format!("scene:{n}"),
            Content::Inline(i) => i.text.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct Item {
    /// Arrival order: the FIFO tie-break. Kept when interrupted or repeated.
    seq: u64,
    content: Content,
    duration: Duration,
    priority: i32,
    repeats_left: u32,
    /// Time already shown in the current repeat (non-zero after an
    /// interruption).
    used: Duration,
}

#[derive(Debug, Clone)]
struct Showing {
    item: Item,
    since: Instant,
}

impl Showing {
    fn shown_for(&self, now: Instant) -> Duration {
        self.item.used + now.saturating_duration_since(self.since)
    }
}

/// What is on screen now.
#[derive(Debug, Clone)]
pub struct Current {
    pub content: Content,
    /// Identifies this showing: `(seq, repeats_left)`. Stable across an
    /// interruption, new for each repeat.
    pub key: (u64, u32),
    /// Time shown in this repeat so far.
    pub elapsed: Duration,
}

#[derive(Debug, Default)]
pub struct NotificationQueue {
    pending: Vec<Item>,
    showing: Option<Showing>,
    next_seq: u64,
}

impl NotificationQueue {
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.showing.is_none()
    }

    pub fn len(&self) -> usize {
        self.pending.len() + usize::from(self.showing.is_some())
    }

    /// Queue a notification. When over [`MAX_QUEUE`], the lowest priority
    /// waiting one (the newest among equals) is dropped, with a warning.
    /// The one showing is never dropped.
    pub fn push(&mut self, n: Notification) {
        let content = match n.content {
            NotificationContent::Scene(name) => Content::Scene(name),
            NotificationContent::Inline { text, icon, color } => {
                Content::Inline(Arc::new(InlineNotice { text, icon, color }))
            }
        };
        self.pending.push(Item {
            seq: self.next_seq,
            content,
            duration: n.duration,
            priority: n.priority,
            repeats_left: n.repeat.max(1),
            used: Duration::ZERO,
        });
        self.next_seq += 1;
        while self.len() > MAX_QUEUE && !self.pending.is_empty() {
            let (i, _) = self
                .pending
                .iter()
                .enumerate()
                .min_by_key(|(_, it)| (it.priority, std::cmp::Reverse(it.seq)))
                .expect("pending is non-empty");
            let dropped = self.pending.remove(i);
            tracing::warn!(
                summary = %dropped.content.summary(),
                priority = dropped.priority,
                "notification queue full ({MAX_QUEUE}): dropped the lowest priority"
            );
        }
    }

    /// Drop everything, including the one showing.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.showing = None;
    }

    /// Drop scene notifications whose scene is not in `scenes` (after a
    /// config reload).
    pub fn retain_scenes(&mut self, scenes: &[String]) {
        let ok = |c: &Content| match c {
            Content::Scene(n) => scenes.contains(n),
            Content::Inline(_) => true,
        };
        let before = self.len();
        self.pending.retain(|i| ok(&i.content));
        if self.showing.as_ref().is_some_and(|s| !ok(&s.item.content)) {
            self.showing = None;
        }
        if self.len() != before {
            tracing::warn!(
                dropped = before - self.len(),
                "dropped scene notifications whose scene is gone"
            );
        }
    }

    /// Advance to `now`: finish the showing one if its time is up, let a
    /// strictly higher priority interrupt it, then start the best waiting
    /// one if the screen is free. At most one change of what shows per call.
    pub fn advance(&mut self, now: Instant) {
        if let Some(s) = &self.showing {
            if s.shown_for(now) >= s.item.duration {
                let mut item = self.showing.take().expect("checked").item;
                item.repeats_left -= 1;
                item.used = Duration::ZERO;
                if item.repeats_left > 0 {
                    self.pending.push(item);
                }
            }
        }
        if let (Some(s), Some(best)) = (&self.showing, self.best()) {
            if self.pending[best].priority > s.item.priority {
                let used = s.shown_for(now);
                let mut item = self.showing.take().expect("checked").item;
                item.used = used.min(item.duration);
                self.pending.push(item);
            }
        }
        if self.showing.is_none() {
            if let Some(best) = self.best() {
                let item = self.pending.remove(best);
                self.showing = Some(Showing { item, since: now });
            }
        }
    }

    /// Index in `pending` of the one to show next: highest priority, then
    /// earliest arrival.
    fn best(&self) -> Option<usize> {
        self.pending
            .iter()
            .enumerate()
            .max_by_key(|(_, i)| (i.priority, std::cmp::Reverse(i.seq)))
            .map(|(i, _)| i)
    }

    /// The notification on screen at `now` (after [`advance`](Self::advance)).
    pub fn current(&self, now: Instant) -> Option<Current> {
        self.showing.as_ref().map(|s| Current {
            content: s.item.content.clone(),
            key: (s.item.seq, s.item.repeats_left),
            elapsed: s.shown_for(now),
        })
    }

    /// The showing one first, then the waiting ones in the order they will
    /// show.
    pub fn status(&self) -> Vec<QueuedNotification> {
        let q = |i: &Item, showing| QueuedNotification {
            summary: i.content.summary(),
            priority: i.priority,
            repeats_left: i.repeats_left,
            showing,
        };
        let mut waiting: Vec<&Item> = self.pending.iter().collect();
        waiting.sort_by_key(|i| (std::cmp::Reverse(i.priority), i.seq));
        self.showing
            .iter()
            .map(|s| q(&s.item, true))
            .chain(waiting.into_iter().map(|i| q(i, false)))
            .collect()
    }
}
