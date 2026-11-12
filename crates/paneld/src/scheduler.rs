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
//! # Where M4 plugs in
//! Both features are extra inputs checked at the top of `tick`, before the
//! playlist logic, and neither needs the playlist code to change:
//! - **Pinning**: a `pinned: Option<usize>` field; while set, `tick` returns
//!   that scene (still tracking elapsed) and leaves `current`/`since` alone.
//! - **Notifications**: a priority queue of `(priority, duration, scene)`.
//!   While one is active, `tick` returns its scene, with [`Active`] gaining a
//!   field saying it is an overlay. The playlist cursor is frozen by
//!   recording how much of the current scene's duration was used; when the
//!   queue empties, `since` is set to `now - used` so the playlist resumes
//!   where it left off. [`Active`] is a struct so adding fields is
//!   non-breaking for the engine.

use std::time::{Duration, Instant};

use crate::condition::Condition;
use crate::config::Config;
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

/// What to show this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Active {
    /// Index of the scene in the playlist.
    pub index: usize,
    /// Time since this scene became active.
    pub elapsed: Duration,
}

#[derive(Debug)]
pub struct Scheduler {
    entries: Vec<Entry>,
    /// Currently displayed entry and when it started.
    current: Option<(usize, Instant)>,
    /// Where the next search for an eligible entry starts when idle.
    cursor: usize,
}

impl Scheduler {
    pub fn new(entries: Vec<Entry>) -> Self {
        Scheduler {
            entries,
            current: None,
            cursor: 0,
        }
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
        Ok(Scheduler::new(entries))
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
    pub fn tick(&mut self, now: Instant, store: &StoreSnapshot) -> Option<Active> {
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
            index,
            elapsed: now.saturating_duration_since(since),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        a.map(|a| a.index)
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
            assert_eq!((a.index, a.elapsed), (want, secs(el)), "t={t}");
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
        assert_eq!((a.index, a.elapsed), (2, Duration::ZERO));

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
        assert_eq!((a.index, a.elapsed), (1, Duration::ZERO));
        assert_eq!(idx(s.tick(t0 + secs(65), &store.snapshot())), Some(1));

        // Current goes ineligible: None; then the other comes back first.
        w.set("a:y", Value::Bool(false));
        assert_eq!(s.tick(t0 + secs(66), &store.snapshot()), None);
        w.set("a:x", Value::Bool(true));
        let a = s.tick(t0 + secs(67), &store.snapshot()).unwrap();
        assert_eq!((a.index, a.elapsed), (0, Duration::ZERO));
    }

    #[test]
    fn single_scene_stays_and_elapsed_keeps_growing() {
        let t0 = Instant::now();
        let snap = Store::new().snapshot();
        let mut s = Scheduler::new(vec![entry(5, None)]);
        for t in [0, 4, 5, 6, 11, 1000] {
            let a = s.tick(t0 + secs(t), &snap).unwrap();
            assert_eq!((a.index, a.elapsed), (0, secs(t)), "t={t}");
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
            assert_eq!((a.index, a.elapsed), (1, secs(t)));
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
        assert_eq!((a.index, a.elapsed), (1, Duration::ZERO));
        let a = s
            .tick(t0 + day + Duration::from_millis(500), &snap)
            .unwrap();
        assert_eq!((a.index, a.elapsed), (1, Duration::from_millis(500)));
        assert_eq!(idx(s.tick(t0 + day + secs(1), &snap)), Some(2));
    }

    #[test]
    fn clock_going_backwards_does_not_panic() {
        let t0 = Instant::now() + secs(100);
        let snap = Store::new().snapshot();
        let mut s = Scheduler::new(vec![entry(10, None), entry(10, None)]);
        s.tick(t0, &snap).unwrap();
        let a = s.tick(t0 - secs(50), &snap).unwrap();
        assert_eq!((a.index, a.elapsed), (0, Duration::ZERO));
    }

    #[test]
    fn empty_playlist_is_none() {
        let snap = Store::new().snapshot();
        let mut s = Scheduler::new(vec![]);
        assert_eq!(s.tick(Instant::now(), &snap), None);
        assert!(s.is_empty());
    }
}
