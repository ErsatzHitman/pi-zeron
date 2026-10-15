//! Which of the session panel's sections are active, in the order they
//! became active, and how far each has appeared. Pure: every call takes an
//! explicit `now`, so it is unit-tested without a window.
//!
//! A section is there only while what it reports is (a goal, a checklist,
//! work running under the chat, background commands). A newcomer takes the
//! bottom seat and grows in; one that ends keeps its seat and its last data
//! while it folds away, so the sections above it never jump.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::motion::{self, MotionSpec};

/// The panel's sections. Declaration order breaks ties between sections
/// that start together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum SectionKey {
    Goal,
    Todos,
    Agents,
    /// A Pi workflow run: live, stopped early, or just finished.
    Workflow,
    Background,
    /// Auto-compaction near its limit, or running.
    Compact,
}

impl SectionKey {
    /// Element-id stem, shared with the fold keys.
    pub fn id(self) -> &'static str {
        match self {
            Self::Goal => "goal",
            Self::Todos => "todos",
            Self::Agents => "agents",
            Self::Workflow => "workflow",
            Self::Background => "background",
            Self::Compact => "compact",
        }
    }
}

/// How far a section is shown, 0..1: in over [`motion::PANEL_ENTER`], out over
/// the quicker [`motion::MENU_OUT`].
#[derive(Debug, Clone, Copy)]
pub(super) struct Reveal {
    from: f32,
    to: f32,
    started: Instant,
}

impl Reveal {
    pub fn shown(now: Instant) -> Self {
        Self {
            from: 1.0,
            to: 1.0,
            started: now,
        }
    }

    pub fn entering(now: Instant) -> Self {
        Self {
            from: 0.0,
            to: 1.0,
            started: now,
        }
    }

    fn spec(&self) -> MotionSpec {
        if self.to > 0.5 {
            motion::PANEL_ENTER
        } else {
            motion::MENU_OUT
        }
    }

    fn total(&self) -> Duration {
        self.spec().total().mul_f32(motion::speed_scale())
    }

    pub fn value(&self, now: Instant, reduced: bool) -> f32 {
        if reduced || self.from == self.to {
            return self.to;
        }
        let raw =
            now.saturating_duration_since(self.started).as_secs_f32() / self.total().as_secs_f32();
        if raw >= 1.0 {
            return self.to;
        }
        motion::lerp(self.from, self.to, self.spec().progress(raw))
    }

    pub fn settled(&self, now: Instant, reduced: bool) -> bool {
        reduced
            || self.from == self.to
            || now.saturating_duration_since(self.started) >= self.total()
    }

    /// Whether it is on its way out.
    #[cfg(test)]
    pub fn leaving(&self) -> bool {
        self.to < 0.5
    }

    /// Head for `to` from wherever it is now, so a reversal never jumps.
    fn retarget(&mut self, to: f32, now: Instant, reduced: bool) {
        if self.to == to {
            return;
        }
        let from = if reduced {
            to
        } else {
            self.value(now, reduced)
        };
        *self = Self {
            from,
            to,
            started: now,
        };
    }

    fn gone(&self, now: Instant, reduced: bool) -> bool {
        self.to == 0.0 && self.settled(now, reduced)
    }
}

pub(super) struct Entry<D> {
    pub key: SectionKey,
    pub reveal: Reveal,
    /// The last data the section was active with; a leaving section keeps
    /// drawing from it while it folds away.
    pub data: D,
    /// The content's painted height, written by a probe each frame.
    pub measured: Rc<Cell<f32>>,
}

/// What a reconcile changed, for the frame pump.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Pulse {
    /// A section is mid-animation: keep frames coming.
    pub animating: bool,
    /// A section just started growing in.
    pub entered: bool,
}

pub(super) struct ActiveSections<D> {
    chat: Option<String>,
    /// Activation order, oldest first.
    entries: Vec<Entry<D>>,
    /// The chat's first frame has been seen: what is already active then
    /// appears at once, and only later arrivals animate.
    seeded: bool,
}

impl<D> Default for ActiveSections<D> {
    fn default() -> Self {
        Self {
            chat: None,
            entries: Vec::new(),
            seeded: false,
        }
    }
}

impl<D> ActiveSections<D> {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Whether `key` is active (not merely leaving). Reads false while the
    /// entries are lent out by [`Self::take`].
    #[cfg(test)]
    pub fn contains(&self, key: SectionKey) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.key == key && !entry.reveal.leaving())
    }

    /// Reconcile with this frame's active sections: `(key, when what it
    /// reports started, data)`.
    pub fn observe(
        &mut self,
        chat: &str,
        mut active: Vec<(SectionKey, Option<DateTime<Utc>>, D)>,
        now: Instant,
        reduced: bool,
    ) -> Pulse {
        if self.chat.as_deref() != Some(chat) {
            self.reset();
            self.chat = Some(chat.to_owned());
        }
        let mut pulse = Pulse::default();
        for entry in &mut self.entries {
            match active.iter().position(|(key, ..)| *key == entry.key) {
                Some(ix) if !entry.reveal.gone(now, reduced) => {
                    entry.data = active.swap_remove(ix).2;
                    entry.reveal.retarget(1.0, now, reduced);
                }
                _ => entry.reveal.retarget(0.0, now, reduced),
            }
        }
        // Newcomers: earliest start first, unknown last, then by key.
        active.sort_by_key(|(key, since, _)| (since.is_none(), *since, *key));
        let animate = self.seeded && !reduced;
        for (key, _, data) in active {
            pulse.entered |= animate;
            self.entries.push(Entry {
                key,
                data,
                reveal: if animate {
                    Reveal::entering(now)
                } else {
                    Reveal::shown(now)
                },
                measured: Rc::new(Cell::new(0.0)),
            });
        }
        self.seeded = true;
        self.entries
            .retain(|entry| !entry.reveal.gone(now, reduced));
        pulse.animating = self
            .entries
            .iter()
            .any(|entry| !entry.reveal.settled(now, reduced));
        pulse
    }

    /// Lend the entries out for rendering.
    pub fn take(&mut self) -> Vec<Entry<D>> {
        std::mem::take(&mut self.entries)
    }

    pub fn restore(&mut self, entries: Vec<Entry<D>>) {
        self.entries = entries;
    }
}

/// When each row of the sections first appeared. A row that turns up in a
/// section already on screen (a sub-agent, a todo, a background command, a
/// workflow task) arrives over [`motion::PANEL_ENTER`]; the rows a section
/// first shows with arrive with the section itself.
#[derive(Default)]
pub(super) struct RowArrivals {
    chat: Option<String>,
    rows: HashMap<String, Arrival>,
    /// Sections fully shown last frame: a newcomer to one of these animates.
    seeded: HashSet<&'static str>,
    shown: HashSet<&'static str>,
    drawn: HashSet<String>,
}

struct Arrival {
    /// When it began arriving; `None` once it is in place.
    started: Option<Instant>,
    /// The row's painted height, written by a probe while it arrives.
    measured: Rc<Cell<f32>>,
}

impl RowArrivals {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Start a frame of `chat`'s panel; another chat starts afresh.
    pub fn begin(&mut self, chat: &str) {
        if self.chat.as_deref() != Some(chat) {
            self.reset();
            self.chat = Some(chat.to_owned());
        }
    }

    /// `section` is fully shown this frame.
    pub fn section_shown(&mut self, section: &'static str) {
        self.shown.insert(section);
    }

    /// How far row `key` of `section` has arrived, 0..1, and its height probe.
    pub fn arrive(
        &mut self,
        section: &'static str,
        key: &str,
        now: Instant,
        reduced: bool,
    ) -> (f32, Rc<Cell<f32>>) {
        let key = format!("{section}/{key}");
        let animate = !reduced && self.seeded.contains(section);
        let arrival = self.rows.entry(key.clone()).or_insert_with(|| Arrival {
            started: animate.then_some(now),
            measured: Rc::new(Cell::new(0.0)),
        });
        self.drawn.insert(key);
        let t = match arrival.started {
            Some(started) if !reduced => {
                let total = motion::PANEL_ENTER.total().mul_f32(motion::speed_scale());
                let raw = now.saturating_duration_since(started).as_secs_f32()
                    / total.as_secs_f32().max(f32::EPSILON);
                if raw >= 1.0 {
                    arrival.started = None;
                    1.0
                } else {
                    motion::PANEL_ENTER.progress(raw)
                }
            }
            _ => {
                arrival.started = None;
                1.0
            }
        };
        (t, arrival.measured.clone())
    }

    /// End the frame. A row not drawn is forgotten, so one that comes back
    /// arrives again. Returns whether a row is still arriving.
    pub fn finish(&mut self) -> bool {
        let drawn = std::mem::take(&mut self.drawn);
        self.rows.retain(|key, _| drawn.contains(key));
        self.seeded = std::mem::take(&mut self.shown);
        self.rows.values().any(|row| row.started.is_some())
    }
}

/// A section's height before its first measurement: the top gap, the
/// divider, the header, the rows and their gaps.
pub(super) fn stack_height(rows: usize) -> f32 {
    use crate::popover::MENU_GAP;
    MENU_GAP
        + 5.0
        + super::SECTION_HEADER_HEIGHT
        + rows as f32 * super::AGENT_ROW_HEIGHT
        + (rows as f32 + 1.0) * MENU_GAP
}

#[cfg(test)]
mod tests {
    use super::*;

    type Active = Vec<(SectionKey, Option<DateTime<Utc>>, u32)>;

    fn at(secs: i64) -> Option<DateTime<Utc>> {
        DateTime::<Utc>::from_timestamp(secs, 0)
    }

    fn keys(sections: &ActiveSections<u32>) -> Vec<SectionKey> {
        sections.entries.iter().map(|entry| entry.key).collect()
    }

    fn in_total() -> Duration {
        motion::PANEL_ENTER.total().mul_f32(motion::speed_scale())
    }

    fn arrive(rows: &mut RowArrivals, section: &'static str, key: &str, now: Instant) -> f32 {
        rows.arrive(section, key, now, false).0
    }

    #[test]
    fn rows_a_section_opens_with_are_in_place_and_later_ones_arrive() {
        let mut rows = RowArrivals::default();
        let t0 = Instant::now();
        rows.begin("a");
        assert_eq!(arrive(&mut rows, "agents", "one", t0), 1.0);
        rows.section_shown("agents");
        assert!(!rows.finish());

        rows.begin("a");
        assert_eq!(arrive(&mut rows, "agents", "one", t0), 1.0);
        assert_eq!(arrive(&mut rows, "agents", "two", t0), 0.0);
        // A section still growing in brings its rows with it.
        assert_eq!(arrive(&mut rows, "todos", "first", t0), 1.0);
        rows.section_shown("agents");
        assert!(rows.finish());

        let mid = t0 + in_total() / 2;
        rows.begin("a");
        arrive(&mut rows, "agents", "one", mid);
        let t = arrive(&mut rows, "agents", "two", mid);
        assert!(t > 0.0 && t < 1.0, "{t}");
        rows.section_shown("agents");
        assert!(rows.finish());

        let done = t0 + in_total();
        rows.begin("a");
        arrive(&mut rows, "agents", "one", done);
        assert_eq!(arrive(&mut rows, "agents", "two", done), 1.0);
        rows.section_shown("agents");
        assert!(!rows.finish());
    }

    #[test]
    fn a_row_that_leaves_arrives_again_and_another_chat_starts_settled() {
        let mut rows = RowArrivals::default();
        let t0 = Instant::now();
        rows.begin("a");
        arrive(&mut rows, "background", "x", t0);
        rows.section_shown("background");
        rows.finish();
        // Gone for a frame (its fold closed): it arrives again.
        rows.begin("a");
        rows.section_shown("background");
        rows.finish();
        rows.begin("a");
        assert_eq!(arrive(&mut rows, "background", "x", t0), 0.0);
        rows.section_shown("background");
        rows.finish();
        // The same keys in another chat are simply there.
        rows.begin("b");
        assert_eq!(arrive(&mut rows, "background", "x", t0), 1.0);
        assert_eq!(arrive(&mut rows, "background", "y", t0), 1.0);
        // Keys are per section.
        rows.section_shown("background");
        rows.finish();
        rows.begin("b");
        assert_eq!(arrive(&mut rows, "agents", "x", t0), 1.0);
    }

    #[test]
    fn reduced_motion_rows_are_always_in_place() {
        let mut rows = RowArrivals::default();
        let t0 = Instant::now();
        rows.begin("a");
        rows.arrive("agents", "one", t0, true);
        rows.section_shown("agents");
        rows.finish();
        rows.begin("a");
        assert_eq!(rows.arrive("agents", "two", t0, true).0, 1.0);
        rows.section_shown("agents");
        assert!(!rows.finish());
    }

    fn out_total() -> Duration {
        motion::MENU_OUT.total().mul_f32(motion::speed_scale())
    }

    #[test]
    fn seeded_sections_order_by_start_and_do_not_animate() {
        let mut sections = ActiveSections::default();
        let t0 = Instant::now();
        let active: Active = vec![
            (SectionKey::Agents, at(20), 0),
            (SectionKey::Goal, at(10), 0),
            (SectionKey::Todos, None, 0),
        ];
        let pulse = sections.observe("a", active, t0, false);
        assert_eq!(
            keys(&sections),
            [SectionKey::Goal, SectionKey::Agents, SectionKey::Todos]
        );
        assert!(!pulse.animating && !pulse.entered);
        assert!(
            sections
                .entries
                .iter()
                .all(|entry| entry.reveal.value(t0, false) == 1.0)
        );
    }

    #[test]
    fn a_later_section_stacks_at_the_end_and_grows_in() {
        let mut sections = ActiveSections::default();
        let t0 = Instant::now();
        sections.observe("a", vec![(SectionKey::Goal, at(10), 0)], t0, false);
        let active: Active = vec![(SectionKey::Goal, at(10), 0), (SectionKey::Todos, None, 0)];
        let pulse = sections.observe("a", active, t0, false);
        assert!(pulse.entered && pulse.animating);
        assert_eq!(keys(&sections), [SectionKey::Goal, SectionKey::Todos]);
        let todos = &sections.entries[1].reveal;
        assert_eq!(todos.value(t0, false), 0.0);
        assert_eq!(todos.value(t0 + in_total(), false), 1.0);
    }

    #[test]
    fn a_leaving_section_keeps_its_seat_and_data_until_it_has_folded() {
        let mut sections = ActiveSections::default();
        let t0 = Instant::now();
        let both: Active = vec![(SectionKey::Goal, at(1), 1), (SectionKey::Todos, at(2), 2)];
        sections.observe("a", both, t0, false);
        sections.observe("a", vec![(SectionKey::Todos, at(2), 3)], t0, false);
        assert_eq!(keys(&sections), [SectionKey::Goal, SectionKey::Todos]);
        assert_eq!(sections.entries[0].data, 1);
        assert_eq!(sections.entries[1].data, 3);
        assert!(!sections.contains(SectionKey::Goal));
        let later = t0 + out_total() + Duration::from_millis(1);
        sections.observe("a", vec![(SectionKey::Todos, at(2), 3)], later, false);
        assert_eq!(keys(&sections), [SectionKey::Todos]);
    }

    #[test]
    fn a_section_back_during_its_exit_reverses_from_where_it_is() {
        let mut sections = ActiveSections::default();
        let t0 = Instant::now();
        let both: Active = vec![(SectionKey::Goal, at(1), 0), (SectionKey::Todos, at(2), 0)];
        sections.observe("a", both.clone(), t0, false);
        sections.observe("a", vec![(SectionKey::Todos, at(2), 0)], t0, false);
        let mid = t0 + out_total() / 2;
        sections.observe("a", both, mid, false);
        assert_eq!(keys(&sections), [SectionKey::Goal, SectionKey::Todos]);
        let value = sections.entries[0].reveal.value(mid, false);
        assert!(value > 0.0 && value < 1.0, "{value}");
    }

    #[test]
    fn a_section_back_after_its_exit_takes_a_new_seat_at_the_end() {
        let mut sections = ActiveSections::default();
        let t0 = Instant::now();
        let both: Active = vec![(SectionKey::Goal, at(1), 0), (SectionKey::Todos, at(2), 0)];
        sections.observe("a", both.clone(), t0, false);
        sections.observe("a", vec![(SectionKey::Todos, at(2), 0)], t0, false);
        let later = t0 + out_total() + Duration::from_millis(1);
        sections.observe("a", vec![(SectionKey::Todos, at(2), 0)], later, false);
        sections.observe("a", both, later, false);
        assert_eq!(keys(&sections), [SectionKey::Todos, SectionKey::Goal]);
    }

    #[test]
    fn reduced_motion_snaps_in_and_out() {
        let mut sections = ActiveSections::default();
        let t0 = Instant::now();
        sections.observe("a", vec![(SectionKey::Goal, None, 0)], t0, true);
        let pulse = sections.observe(
            "a",
            vec![(SectionKey::Goal, None, 0), (SectionKey::Todos, None, 0)],
            t0,
            true,
        );
        assert!(!pulse.entered && !pulse.animating);
        sections.observe("a", vec![(SectionKey::Todos, None, 0)], t0, true);
        assert_eq!(keys(&sections), [SectionKey::Todos]);
    }

    #[test]
    fn changing_chat_resets_and_snaps() {
        let mut sections = ActiveSections::default();
        let t0 = Instant::now();
        sections.observe("a", vec![(SectionKey::Goal, None, 0)], t0, false);
        let pulse = sections.observe("b", vec![(SectionKey::Todos, None, 0)], t0, false);
        assert!(!pulse.entered);
        assert_eq!(keys(&sections), [SectionKey::Todos]);
    }

    #[test]
    fn stack_height_counts_header_rows_and_gaps() {
        assert_eq!(stack_height(0), 35.0);
        assert_eq!(stack_height(2), 95.0);
    }
}
