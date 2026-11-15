//! The playlist scheduler (DESIGN.md §5.5): decides which scene is on screen.
//!
//! The scheduler is pure state: time is passed into [`Scheduler::tick`] as an
//! `Instant` and the store as a snapshot, so tests drive it with a fabricated
//! clock and never sleep. It does no I/O and is called once per frame from
//! the render loop.
//!
//! # Behaviour
//! - Scenes rotate in config order. A scene is shown for its duration, then
//!   the next *eligible* scene (condition absent or true) takes over,
//!   wrapping around.
//! - Conditions are re-checked every tick. If the current scene's condition
//!   turns false it is replaced immediately by the next eligible scene.
//! - If nothing is eligible, [`Scheduler::tick`] returns `None`. When a scene
//!   becomes eligible again, rotation resumes from where it left off (the
//!   search starts after the last scene shown).
//! - With a single eligible scene, it just stays on screen; its elapsed time
//!   keeps growing rather than restarting every duration.
//! - `elapsed` restarts from zero on every switch to a *different* scene.
//! - **Time jumps:** a tick performs at most one switch, and the new scene
//!   starts at the `now` of that tick. If the render loop stalls for ten
//!   durations, the next tick moves one scene forward, not ten; the playlist
//!   never catches up on missed time and never loops. A `now` earlier than
//!   the scene start counts as zero elapsed.
//!
//! # Pinning (M4)
//! While a scene is pinned, `tick` returns it (still tracking elapsed time)
//! regardless of its condition, and leaves the playlist's own state alone.
//!
//! # Notifications (M4)
//! A notification is an alert: a priority, a duration per showing, a repeat
//! count and content (a configured scene, or inline text/icon). The queue
//! lives in [`crate::notifications`]; `tick` consults it before the pin and
//! the playlist. The rules:
//! - **Order:** the highest priority shows first; equal priorities are FIFO
//!   by arrival.
//! - **Interrupt:** a *strictly* higher priority interrupts the one showing
//!   (equal priority never does). The interrupted notification goes back to
//!   the queue keeping its arrival order, its remaining repeats and the
//!   **remaining time of the current repeat**, so it later continues where
//!   it stopped (a scene notification's `scene_elapsed` continues too)
//!   rather than being lost or restarted: every alert gets its full time.
//! - **Repeat:** `repeat` is how many times it is shown, **back-to-back**: a
//!   finished repeat re-enters the queue with its original arrival order, so
//!   it follows straight on unless something of higher priority is waiting
//!   (`A` with repeat 3 and a later equal-priority `B` shows `A A A B`).
//!   Interleaving with the playlist was rejected: a repeat is meant to get
//!   attention, and the playlist's frozen clock would make it confusing.
//! - **Frozen playlist:** while any notification shows, the playlist clock
//!   stops. When the queue empties the playlist resumes where it left off:
//!   the same scene with the same remaining duration (its `since` is moved
//!   forward by the time away). Conditions are re-checked as usual after
//!   the resume.
//! - **Pin:** a pinned scene is interrupted by notifications too (they are
//!   alerts and must not be hidden by a pin); once the queue empties the
//!   pin is shown again, its elapsed time continuing.
//! - **Clear:** [`Scheduler::clear_notifications`] empties the queue and
//!   ends the one showing.
//! - **Cap:** at most [`MAX_QUEUE`](crate::notifications::MAX_QUEUE)
//!   notifications; on overflow the lowest priority (newest among equals)
//!   waiting one is dropped, with a warning.
//! - **Reload:** the engine moves the queue to the new scheduler
//!   ([`Scheduler::take_queue`] / [`Scheduler::adopt_queue`]); scene
//!   notifications whose scene disappeared are dropped.
//! - Like the playlist, a tick makes at most one change of what shows.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::condition::Condition;
use crate::config::Config;
use crate::control::{Notification, QueuedNotification};
use crate::notifications::{Content, InlineNotice, NotificationQueue};
use crate::store::StoreSnapshot;

/// One playlist slot. Indices into the playlist are indices into the
/// scene list the engine owns.
#[derive(Debug, Clone)]
pub struct Entry {
    pub duration: Duration,
    pub condition: Option<Condition>,
}

impl Entry {
    fn eligible(&self, store: &StoreSnapshot) -> bool {
        self.condition.as_ref().is_none_or(|c| c.eval(store))
    }
}

/// What to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A scene, by index in the playlist (the scene list the engine owns).
    Scene(usize),
    /// Inline notification content, drawn full-screen. `key` identifies the
    /// showing (new for every repeat) so the caller can cache what it built.
    Inline {
        key: (u64, u32),
        notice: Arc<InlineNotice>,
    },
}

/// What to show this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Active {
    pub target: Target,
    /// Time since this scene (or notification repeat) became active.
    pub elapsed: Duration,
    /// Whether this is a notification rather than the playlist or the pin.
    pub notification: bool,
}

impl Active {
    /// The scene index, for scene targets.
    pub fn index(&self) -> Option<usize> {
        match self.target {
            Target::Scene(i) => Some(i),
            Target::Inline { .. } => None,
        }
    }
}

#[derive(Debug)]
pub struct Scheduler {
    entries: Vec<Entry>,
    /// Currently displayed entry and when it started.
    current: Option<(usize, Instant)>,
    /// Where the next search for an eligible entry starts when idle.
    cursor: usize,
    /// Pinned entry and when it was pinned (M4). While set, `tick` shows it
    /// regardless of its condition and the playlist state is left alone.
    pinned: Option<(usize, Instant)>,
    /// Scene names, parallel to `entries`; scene notifications refer to them.
    names: Vec<String>,
    queue: NotificationQueue,
    /// When the playlist clock was frozen by notifications, if it is.
    frozen_at: Option<Instant>,
}

impl Scheduler {
    pub fn new(entries: Vec<Entry>) -> Self {
        Scheduler {
            entries,
            current: None,
            cursor: 0,
            pinned: None,
            names: Vec::new(),
            queue: NotificationQueue::default(),
            frozen_at: None,
        }
    }

    /// Name the entries (same order), so scene notifications can find them.
    pub fn with_names(mut self, names: Vec<String>) -> Self {
        self.names = names;
        self
    }

    /// Build the playlist for the scenes at `indices` of `cfg.scenes`, in
    /// that order, using the global default duration where a scene has none.
    pub fn from_config(cfg: &Config, indices: &[usize]) -> anyhow::Result<Self> {
        let default = cfg.playlist.default_duration();
        let entries = indices
            .iter()
            .map(|&i| {
                let s = &cfg.scenes[i];
                let condition = s
                    .condition
                    .as_deref()
                    .map(Condition::parse)
                    .transpose()
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "scene {:?}: invalid condition {:?}: {e}",
                            s.name,
                            s.condition
                        )
                    })?;
                Ok(Entry {
                    duration: s.duration_s.map_or(default, crate::config::seconds),
                    condition,
                })
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Scheduler::new(entries).with_names(
            indices
                .iter()
                .map(|&i| cfg.scenes[i].name.clone())
                .collect(),
        ))
    }

    /// Drop every condition, so all entries are always eligible. Used when
    /// a single scene is requested by name.
    pub fn ignore_conditions(&mut self) {
        for e in &mut self.entries {
            e.condition = None;
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// First eligible entry scanning from `start`, wrapping. A scan visits
    /// every entry once, `start` first, so passing `current + 1` visits the
    /// current entry last.
    fn find_from(&self, start: usize, store: &StoreSnapshot) -> Option<usize> {
        let n = self.entries.len();
        (0..n)
            .map(|k| (start + k) % n)
            .find(|&i| self.entries[i].eligible(store))
    }

    /// Advance to `now` and return the scene to show, or `None` if no scene
    /// is eligible.
    /// Pin entry `index` (shown from `now`, regardless of its condition), or
    /// release the pin with `None`. While pinned, the playlist's own clock
    /// keeps running untouched; on release `tick` carries on from it, so a
    /// scene whose duration ran out during the pin moves on at the next tick.
    /// Out-of-range indices are ignored.
    pub fn pin(&mut self, index: Option<usize>, now: Instant) {
        self.pinned = match index {
            Some(i) if i < self.entries.len() => Some((i, now)),
            Some(_) => self.pinned,
            None => None,
        };
    }

    /// The pinned entry, if any.
    pub fn pinned(&self) -> Option<usize> {
        self.pinned.map(|(i, _)| i)
    }

    /// Queue a notification (see the module docs for the rules). It starts
    /// showing at the next `tick`.
    pub fn notify(&mut self, n: Notification) {
        self.queue.push(n);
    }

    /// Empty the queue and end the notification showing.
    pub fn clear_notifications(&mut self) {
        self.queue.clear();
    }

    /// The showing notification first, then the waiting ones in order.
    pub fn notification_status(&self) -> Vec<QueuedNotification> {
        self.queue.status()
    }

    /// Hand the queue over (to the scheduler built by a config reload).
    pub fn take_queue(&mut self) -> NotificationQueue {
        std::mem::take(&mut self.queue)
    }

    /// Adopt a queue from the previous scheduler, dropping scene
    /// notifications whose scene no longer exists.
    pub fn adopt_queue(&mut self, mut queue: NotificationQueue) {
        queue.retain_scenes(&self.names);
        self.queue = queue;
    }

    pub fn tick(&mut self, now: Instant, store: &StoreSnapshot) -> Option<Active> {
        self.queue.advance(now);
        // Skip a scene notification whose scene is unknown (cannot normally
        // happen: they are validated and filtered on reload).
        while let Some(cur) = self.queue.current(now) {
            let target = match &cur.content {
                Content::Scene(name) => match self.names.iter().position(|n| n == name) {
                    Some(i) => Target::Scene(i),
                    None => {
                        tracing::warn!(scene = %name, "dropping notification for unknown scene");
                        self.queue.retain_scenes(&self.names);
                        self.queue.advance(now);
                        continue;
                    }
                },
                Content::Inline(notice) => Target::Inline {
                    key: cur.key,
                    notice: Arc::clone(notice),
                },
            };
            self.frozen_at.get_or_insert(now);
            return Some(Active {
                target,
                elapsed: cur.elapsed,
                notification: true,
            });
        }
        if let Some(t) = self.frozen_at.take() {
            // Resume where the playlist (and pin) left off.
            let away = now.saturating_duration_since(t);
            if let Some((_, since)) = &mut self.current {
                *since += away;
            }
            if let Some((_, since)) = &mut self.pinned {
                // A pin set while frozen starts counting now.
                *since = if *since <= t { *since + away } else { now };
            }
        }
        if let Some((index, since)) = self.pinned {
            return Some(Active {
                target: Target::Scene(index),
                elapsed: now.saturating_duration_since(since),
                notification: false,
            });
        }
        let n = self.entries.len();
        if n == 0 {
            return None;
        }
        match self.current {
            None => {
                let i = self.find_from(self.cursor, store)?;
                self.current = Some((i, now));
            }
            Some((cur, since)) => {
                let entry = &self.entries[cur];
                if !entry.eligible(store) {
                    // Condition flipped mid-scene: leave now. The current
                    // entry is ineligible, so the scan can't pick it.
                    match self.find_from(cur + 1, store) {
                        Some(i) => self.current = Some((i, now)),
                        None => {
                            self.current = None;
                            self.cursor = (cur + 1) % n;
                            return None;
                        }
                    }
                } else if now.saturating_duration_since(since) >= entry.duration {
                    // At most one switch per tick. Scanning from cur + 1
                    // visits `cur` last, so a lone eligible scene stays.
                    match self.find_from(cur + 1, store) {
                        Some(i) if i != cur => self.current = Some((i, now)),
                        _ => {}
                    }
                }
            }
        }
        let (index, since) = self.current?;
        self.cursor = (index + 1) % n;
        Some(Active {
            target: Target::Scene(index),
            elapsed: now.saturating_duration_since(since),
            notification: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_overrides_conditions_and_release_returns_to_the_playlist() {
        let empty = StoreSnapshot::default();
        // Entry 1 is never eligible (its key is missing).
        let mut s = Scheduler::new(vec![
            Entry {
                duration: Duration::from_secs(10),
                condition: None,
            },
            Entry {
                duration: Duration::from_secs(10),
                condition: Some(Condition::parse("x:missing").unwrap()),
            },
        ]);
        let t0 = Instant::now();
        assert_eq!(s.tick(t0, &empty).and_then(|a| a.index()), Some(0));
        s.pin(Some(1), t0 + Duration::from_secs(2));
        assert_eq!(s.pinned(), Some(1));
        let a = s.tick(t0 + Duration::from_secs(5), &empty).unwrap();
        assert_eq!((a.index().unwrap(), a.elapsed), (1, Duration::from_secs(3)));
        // Out of range: ignored, pin kept.
        s.pin(Some(9), t0);
        assert_eq!(s.pinned(), Some(1));
        s.pin(None, t0 + Duration::from_secs(6));
        assert_eq!(s.pinned(), None);
        let a = s.tick(t0 + Duration::from_secs(6), &empty).unwrap();
        assert_eq!((a.index().unwrap(), a.elapsed), (0, Duration::from_secs(6)));
    }
    use crate::store::{Store, Value};

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn entry(duration_s: u64, cond: Option<&str>) -> Entry {
        Entry {
            duration: secs(duration_s),
            condition: cond.map(|c| Condition::parse(c).unwrap()),
        }
    }

    fn store_with(entries: &[(&str, f64)]) -> (Store, StoreSnapshot) {
        let store = Store::new();
        let w = store.writer("t");
        for (k, v) in entries {
            w.set(
                *k,
                Value::Number {
                    value: *v,
                    unit: None,
                },
            );
        }
        let snap = store.snapshot();
        (store, snap)
    }

    fn idx(a: Option<Active>) -> Option<usize> {
        a.and_then(|a| a.index())
    }

    #[test]
    fn rotates_on_duration_across_cycles() {
        let t0 = Instant::now();
        let snap = Store::new().snapshot();
        let mut s = Scheduler::new(vec![entry(10, None), entry(5, None), entry(20, None)]);
        // (offset, expected index, expected elapsed)
        for (t, want, el) in [
            (0, 0, 0),
            (9, 0, 9),
            (10, 1, 0),
            (14, 1, 4),
            (15, 2, 0),
            (34, 2, 19),
            (35, 0, 0), // wraps
            (44, 0, 9),
            (45, 1, 0),
            (50, 2, 0),
            (70, 0, 0),
        ] {
            let a = s.tick(t0 + secs(t), &snap).unwrap();
            assert_eq!((a.index().unwrap(), a.elapsed), (want, secs(el)), "t={t}");
        }
    }

    #[test]
    fn skips_ineligible_scenes() {
        let t0 = Instant::now();
        let (_, snap) = store_with(&[("a:on", 0.0)]);
        let mut s = Scheduler::new(vec![
            entry(10, None),
            entry(10, Some("a:on > 0")),
            entry(10, None),
        ]);
        assert_eq!(idx(s.tick(t0, &snap)), Some(0));
        assert_eq!(idx(s.tick(t0 + secs(10), &snap)), Some(2));
        assert_eq!(idx(s.tick(t0 + secs(20), &snap)), Some(0));
    }

    #[test]
    fn first_scene_ineligible_at_start() {
        let t0 = Instant::now();
        let snap = Store::new().snapshot();
        let mut s = Scheduler::new(vec![entry(10, Some("a:x")), entry(10, None)]);
        assert_eq!(idx(s.tick(t0, &snap)), Some(1));
    }

    #[test]
    fn condition_flipping_mid_scene_switches_immediately() {
        let t0 = Instant::now();
        let store = Store::new();
        let w = store.writer("t");
        w.set("a:on", Value::Bool(true));
        let mut s = Scheduler::new(vec![
            entry(100, None),
            entry(100, Some("a:on")),
            entry(100, None),
        ]);
        assert_eq!(idx(s.tick(t0, &store.snapshot())), Some(0));
        assert_eq!(idx(s.tick(t0 + secs(100), &store.snapshot())), Some(1));
        assert_eq!(idx(s.tick(t0 + secs(101), &store.snapshot())), Some(1));

        w.set("a:on", Value::Bool(false));
        let a = s.tick(t0 + secs(102), &store.snapshot()).unwrap();
        assert_eq!((a.index().unwrap(), a.elapsed), (2, Duration::ZERO));

        // Flipping back doesn't interrupt scene 2; it joins the rotation.
        w.set("a:on", Value::Bool(true));
        assert_eq!(idx(s.tick(t0 + secs(150), &store.snapshot())), Some(2));
        assert_eq!(idx(s.tick(t0 + secs(202), &store.snapshot())), Some(0));
        assert_eq!(idx(s.tick(t0 + secs(302), &store.snapshot())), Some(1));
    }

    #[test]
    fn none_when_all_ineligible_then_recovers() {
        let t0 = Instant::now();
        let store = Store::new();
        let w = store.writer("t");
        let mut s = Scheduler::new(vec![entry(10, Some("a:x")), entry(10, Some("a:y"))]);
        assert_eq!(s.tick(t0, &store.snapshot()), None);
        assert_eq!(s.tick(t0 + secs(50), &store.snapshot()), None);

        w.set("a:y", Value::Bool(true));
        let a = s.tick(t0 + secs(60), &store.snapshot()).unwrap();
        assert_eq!((a.index().unwrap(), a.elapsed), (1, Duration::ZERO));
        assert_eq!(idx(s.tick(t0 + secs(65), &store.snapshot())), Some(1));

        // Current goes ineligible: None; then the other comes back first.
        w.set("a:y", Value::Bool(false));
        assert_eq!(s.tick(t0 + secs(66), &store.snapshot()), None);
        w.set("a:x", Value::Bool(true));
        let a = s.tick(t0 + secs(67), &store.snapshot()).unwrap();
        assert_eq!((a.index().unwrap(), a.elapsed), (0, Duration::ZERO));
    }

    #[test]
    fn single_scene_stays_and_elapsed_keeps_growing() {
        let t0 = Instant::now();
        let snap = Store::new().snapshot();
        let mut s = Scheduler::new(vec![entry(5, None)]);
        for t in [0, 4, 5, 6, 11, 1000] {
            let a = s.tick(t0 + secs(t), &snap).unwrap();
            assert_eq!((a.index().unwrap(), a.elapsed), (0, secs(t)), "t={t}");
        }
    }

    #[test]
    fn only_one_eligible_among_many_stays() {
        let t0 = Instant::now();
        let snap = Store::new().snapshot();
        let mut s = Scheduler::new(vec![
            entry(5, Some("a:no")),
            entry(5, None),
            entry(5, Some("a:no")),
        ]);
        for t in [0, 5, 10, 17] {
            let a = s.tick(t0 + secs(t), &snap).unwrap();
            assert_eq!((a.index().unwrap(), a.elapsed), (1, secs(t)));
        }
    }

    #[test]
    fn large_time_jump_moves_exactly_one_scene() {
        let t0 = Instant::now();
        let snap = Store::new().snapshot();
        let mut s = Scheduler::new(vec![entry(1, None), entry(1, None), entry(1, None)]);
        assert_eq!(idx(s.tick(t0, &snap)), Some(0));
        // A stall of a full day: one step forward, restarting at the jump time.
        let day = secs(86_400);
        let a = s.tick(t0 + day, &snap).unwrap();
        assert_eq!((a.index().unwrap(), a.elapsed), (1, Duration::ZERO));
        let a = s
            .tick(t0 + day + Duration::from_millis(500), &snap)
            .unwrap();
        assert_eq!(
            (a.index().unwrap(), a.elapsed),
            (1, Duration::from_millis(500))
        );
        assert_eq!(idx(s.tick(t0 + day + secs(1), &snap)), Some(2));
    }

    #[test]
    fn clock_going_backwards_does_not_panic() {
        let t0 = Instant::now() + secs(100);
        let snap = Store::new().snapshot();
        let mut s = Scheduler::new(vec![entry(10, None), entry(10, None)]);
        s.tick(t0, &snap).unwrap();
        let a = s.tick(t0 - secs(50), &snap).unwrap();
        assert_eq!((a.index().unwrap(), a.elapsed), (0, Duration::ZERO));
    }

    #[test]
    fn empty_playlist_is_none() {
        let snap = Store::new().snapshot();
        let mut s = Scheduler::new(vec![]);
        assert_eq!(s.tick(Instant::now(), &snap), None);
        assert!(s.is_empty());
    }
}

#[cfg(test)]
mod notification_tests {
    use super::*;
    use crate::color::Color;
    use crate::control::NotificationContent;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn inline(text: &str, priority: i32, dur: u64, repeat: u32) -> Notification {
        Notification {
            content: NotificationContent::Inline {
                text: text.into(),
                icon: None,
                color: Color::WHITE,
            },
            duration: secs(dur),
            priority,
            repeat,
        }
    }

    fn scene_n(name: &str, priority: i32, dur: u64) -> Notification {
        Notification {
            content: NotificationContent::Scene(name.into()),
            duration: secs(dur),
            priority,
            repeat: 1,
        }
    }

    fn playlist(durations: &[u64]) -> Scheduler {
        let names = ["a", "b", "c"];
        Scheduler::new(
            durations
                .iter()
                .map(|d| Entry {
                    duration: secs(*d),
                    condition: None,
                })
                .collect(),
        )
        .with_names(
            names[..durations.len()]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
    }

    /// What shows at `t0 + t`: the inline text, or `#<index>` for a scene,
    /// plus the elapsed seconds.
    fn at(s: &mut Scheduler, t0: Instant, t: u64) -> (String, u64) {
        let snap = StoreSnapshot::default();
        let a = s.tick(t0 + secs(t), &snap).expect("something shows");
        let what = match &a.target {
            Target::Scene(i) => format!("#{i}"),
            Target::Inline { notice, .. } => notice.text.clone(),
        };
        (what, a.elapsed.as_secs())
    }

    fn shown(s: &mut Scheduler, t0: Instant, t: u64) -> String {
        at(s, t0, t).0
    }

    #[test]
    fn highest_priority_first_then_fifo() {
        let t0 = Instant::now();
        let mut s = playlist(&[100]);
        s.notify(inline("a", 1, 2, 1));
        s.notify(inline("b", 5, 2, 1));
        s.notify(inline("c", 5, 2, 1));
        s.notify(inline("d", 1, 2, 1));
        let order: Vec<_> = (0..8).step_by(2).map(|t| shown(&mut s, t0, t)).collect();
        assert_eq!(order, ["b", "c", "a", "d"]);
        assert_eq!(shown(&mut s, t0, 8), "#0");
        assert!(s.notification_status().is_empty());
    }

    #[test]
    fn higher_priority_interrupts_and_the_interrupted_one_resumes_with_remaining_time() {
        let t0 = Instant::now();
        let mut s = playlist(&[100]);
        s.notify(inline("low", 1, 10, 1));
        assert_eq!(at(&mut s, t0, 0), ("low".into(), 0));
        // Equal priority never interrupts.
        s.notify(inline("same", 1, 5, 1));
        assert_eq!(at(&mut s, t0, 3), ("low".into(), 3));
        s.notify(inline("high", 9, 4, 1));
        assert_eq!(at(&mut s, t0, 3), ("high".into(), 0));
        let st = s.notification_status();
        let summaries: Vec<_> = st.iter().map(|q| (q.summary.as_str(), q.showing)).collect();
        assert_eq!(summaries, [("high", true), ("low", false), ("same", false)]);
        // `low` continues with 7 s left (3 s used), ahead of the later `same`.
        assert_eq!(at(&mut s, t0, 7), ("low".into(), 3));
        assert_eq!(at(&mut s, t0, 13), ("low".into(), 9));
        assert_eq!(at(&mut s, t0, 14), ("same".into(), 0));
        assert_eq!(shown(&mut s, t0, 19), "#0");
    }

    #[test]
    fn repeats_run_back_to_back() {
        let t0 = Instant::now();
        let mut s = playlist(&[100]);
        s.notify(inline("a", 0, 2, 3));
        s.notify(inline("b", 0, 2, 1));
        let shows: Vec<_> = (0..8).step_by(2).map(|t| shown(&mut s, t0, t)).collect();
        assert_eq!(shows, ["a", "a", "a", "b"]);

        let mut s = playlist(&[100]);
        s.notify(inline("a", 0, 2, 3));
        at(&mut s, t0, 0);
        assert_eq!(s.notification_status()[0].repeats_left, 3);

        // Each repeat starts its own clock.
        let mut s = playlist(&[100]);
        s.notify(inline("a", 0, 2, 2));
        assert_eq!(at(&mut s, t0, 1), ("a".into(), 0));
        assert_eq!(at(&mut s, t0, 3), ("a".into(), 0));
        assert_eq!(at(&mut s, t0, 4), ("a".into(), 1));
        assert_eq!(shown(&mut s, t0, 5), "#0");
    }

    #[test]
    fn clear_ends_the_showing_one_and_empties_the_queue() {
        let t0 = Instant::now();
        let mut s = playlist(&[100]);
        s.notify(inline("a", 0, 50, 1));
        s.notify(inline("b", 0, 50, 1));
        assert_eq!(shown(&mut s, t0, 0), "a");
        s.clear_notifications();
        assert_eq!(shown(&mut s, t0, 1), "#0");
        assert!(s.notification_status().is_empty());
    }

    #[test]
    fn playlist_resumes_where_it_left_off_with_remaining_time() {
        let t0 = Instant::now();
        let mut s = playlist(&[10, 10]);
        assert_eq!(at(&mut s, t0, 0), ("#0".into(), 0));
        assert_eq!(at(&mut s, t0, 4), ("#0".into(), 4));
        s.notify(inline("n", 0, 20, 1));
        assert_eq!(shown(&mut s, t0, 5), "n");
        assert_eq!(shown(&mut s, t0, 24), "n");
        // 5 s of scene 0 were used; the 20 s away don't count.
        assert_eq!(at(&mut s, t0, 25), ("#0".into(), 5));
        assert_eq!(at(&mut s, t0, 29), ("#0".into(), 9));
        assert_eq!(at(&mut s, t0, 30), ("#1".into(), 0));
    }

    #[test]
    fn scene_notifications_show_that_scene_from_zero() {
        let t0 = Instant::now();
        let mut s = playlist(&[10, 10, 10]);
        assert_eq!(at(&mut s, t0, 0), ("#0".into(), 0));
        s.notify(scene_n("c", 0, 5));
        assert_eq!(at(&mut s, t0, 2), ("#2".into(), 0));
        assert_eq!(at(&mut s, t0, 4), ("#2".into(), 2));
        assert_eq!(s.notification_status()[0].summary, "scene:c");
        assert_eq!(at(&mut s, t0, 7), ("#0".into(), 2));
    }

    #[test]
    fn a_pin_is_interrupted_and_shown_again_afterwards() {
        let t0 = Instant::now();
        let mut s = playlist(&[10, 10]);
        let snap = StoreSnapshot::default();
        s.tick(t0, &snap);
        s.pin(Some(1), t0 + secs(1));
        assert_eq!(at(&mut s, t0, 3), ("#1".into(), 2));
        s.notify(inline("n", 0, 10, 1));
        assert_eq!(shown(&mut s, t0, 4), "n");
        assert_eq!(at(&mut s, t0, 14), ("#1".into(), 3));
        s.pin(None, t0 + secs(14));
        // The playlist was frozen too: scene 0 had used 4 s by t=4.
        assert_eq!(at(&mut s, t0, 15), ("#0".into(), 5));
    }

    #[test]
    fn overflow_drops_the_lowest_priority_newest_first() {
        let t0 = Instant::now();
        let mut s = playlist(&[100]);
        for i in 0..crate::notifications::MAX_QUEUE {
            s.notify(inline(&format!("n{i}"), 0, 5, 1));
        }
        assert_eq!(s.notification_status().len(), 50);
        s.notify(inline("urgent", 7, 5, 1));
        let st = s.notification_status();
        assert_eq!(st.len(), 50);
        assert_eq!(st[0].summary, "urgent");
        assert!(
            !st.iter().any(|q| q.summary == "n49"),
            "newest low one dropped"
        );
        assert!(st.iter().any(|q| q.summary == "n48"));
        // A new lowest-priority one is itself dropped.
        s.notify(inline("late", -1, 5, 1));
        assert!(!s.notification_status().iter().any(|q| q.summary == "late"));
        // The one showing is never dropped.
        assert_eq!(shown(&mut s, t0, 0), "urgent");
        for _ in 0..60 {
            s.notify(inline("flood", 100, 5, 1));
        }
        let st = s.notification_status();
        assert_eq!(st.len(), 50);
        assert_eq!(st.iter().filter(|q| q.showing).count(), 1);
        assert_eq!(st[0].summary, "urgent");
    }

    #[test]
    fn reload_keeps_the_queue_and_drops_scenes_that_vanished() {
        let t0 = Instant::now();
        let mut old = playlist(&[10, 10]);
        old.notify(scene_n("b", 9, 5));
        old.notify(scene_n("a", 1, 5));
        old.notify(inline("text", 0, 5, 1));
        assert_eq!(at(&mut old, t0, 0), ("#1".into(), 0));
        // The new config only has scene "a".
        let mut new = playlist(&[10]);
        new.adopt_queue(old.take_queue());
        let st = new.notification_status();
        let summaries: Vec<_> = st.iter().map(|q| (q.summary.as_str(), q.showing)).collect();
        assert_eq!(summaries, [("scene:a", false), ("text", false)]);
        assert_eq!(at(&mut new, t0, 1), ("#0".into(), 0));
        assert!(old.notification_status().is_empty());
    }
}
