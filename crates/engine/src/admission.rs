//! Global admission control for concurrently RUNNING turns.
//!
//! Every chat's turn holds one slot from the moment it starts working until it
//! settles (Idle / Errored) or parks on a question (AwaitingInput). When the
//! cap is full, new fresh runs queue here in order and start as slots free;
//! the queue is visible on the chat's session row (`queue_position`). The
//! bookkeeping is pure and synchronous; [`crate::sessions`] owns the wiring.
//!
//! Deadlock rule: an orchestrator root (or any parent) stays Working while it
//! waits on its children, so cap-many parents could starve the very children
//! they wait for. Tree members therefore queue ahead of everyone else (deeper
//! first) and a member start is admitted over the cap while fewer than
//! [`MEMBER_FLOOR`] member turns run.

use std::collections::HashMap;

use tokio::sync::oneshot;

/// Member turns that may always start, cap or not.
pub(crate) const MEMBER_FLOOR: usize = 2;

/// What a start does about the cap.
pub(crate) enum Admit {
    /// A slot is held: start now.
    Now,
    /// No slot: start when the receiver fires. A dropped sender means the wait
    /// was cancelled.
    Queued(oneshot::Receiver<()>),
}

struct Waiter {
    chat_id: String,
    /// `Some(depth)` for a tree member; members go before everyone else.
    depth: Option<usize>,
    seq: u64,
    grant: oneshot::Sender<()>,
}

impl Waiter {
    /// Grant order: members first, deeper members first, then arrival.
    fn key(&self) -> (bool, std::cmp::Reverse<usize>, u64) {
        (
            self.depth.is_none(),
            std::cmp::Reverse(self.depth.unwrap_or(0)),
            self.seq,
        )
    }
}

#[derive(Default)]
pub(crate) struct Admission {
    /// Chats holding a slot, with their member depth.
    running: HashMap<String, Option<usize>>,
    /// Kept in grant order, so position = index + 1.
    waiters: Vec<Waiter>,
    seq: u64,
}

impl Admission {
    /// Whether `chat_id` holds a slot or waits for one.
    pub(crate) fn tracks(&self, chat_id: &str) -> bool {
        self.running.contains_key(chat_id) || self.waiters.iter().any(|w| w.chat_id == chat_id)
    }

    pub(crate) fn running(&self) -> usize {
        self.running.len()
    }

    fn member_running(&self) -> usize {
        self.running.values().filter(|d| d.is_some()).count()
    }

    /// 1-based place in the grant order; 0 = not queued.
    pub(crate) fn position(&self, chat_id: &str) -> u32 {
        self.waiters
            .iter()
            .position(|w| w.chat_id == chat_id)
            .map_or(0, |i| u32::try_from(i + 1).unwrap_or(u32::MAX))
    }

    fn fits(&self, cap: usize, member: bool) -> bool {
        cap == 0 || self.running.len() < cap || (member && self.member_running() < MEMBER_FLOOR)
    }

    /// Ask for a slot for a fresh run. Idempotent: a chat that already holds
    /// one is `Now`; one already queued keeps its place.
    pub(crate) fn admit(&mut self, chat_id: &str, depth: Option<usize>, cap: usize) -> Admit {
        // A raised cap (or a freed floor) may have room the queue has not used.
        self.grant(cap);
        if self.running.contains_key(chat_id) {
            return Admit::Now;
        }
        let (tx, rx) = oneshot::channel();
        if let Some(waiter) = self.waiters.iter_mut().find(|w| w.chat_id == chat_id) {
            waiter.grant = tx;
            return Admit::Queued(rx);
        }
        // After the grant above, only a member over the floor can still fit
        // while others wait.
        if self.fits(cap, depth.is_some()) {
            self.running.insert(chat_id.to_owned(), depth);
            return Admit::Now;
        }
        self.seq += 1;
        let waiter = Waiter {
            chat_id: chat_id.to_owned(),
            depth,
            seq: self.seq,
            grant: tx,
        };
        let at = self.waiters.partition_point(|w| w.key() < waiter.key());
        self.waiters.insert(at, waiter);
        Admit::Queued(rx)
    }

    /// A turn that must not wait (a steer into a live runtime, an un-park, an
    /// answered question) takes a slot even over the cap. A queued chat keeps
    /// waiting instead.
    pub(crate) fn mark_running(&mut self, chat_id: &str, depth: Option<usize>) {
        if self.waiters.iter().any(|w| w.chat_id == chat_id) {
            return;
        }
        self.running.entry(chat_id.to_owned()).or_insert(depth);
    }

    /// Free `chat_id`'s slot and hand freed slots to the queue. Returns whether
    /// the queue changed.
    pub(crate) fn release(&mut self, chat_id: &str, cap: usize) -> bool {
        self.running.remove(chat_id);
        self.grant(cap)
    }

    /// Drop `chat_id` from the queue. Returns whether it was waiting.
    pub(crate) fn cancel(&mut self, chat_id: &str) -> bool {
        let before = self.waiters.len();
        self.waiters.retain(|w| w.chat_id != chat_id);
        self.waiters.len() != before
    }

    /// Drop every trace of `chat_id`: slot and wait. Returns whether the queue
    /// changed.
    pub(crate) fn forget(&mut self, chat_id: &str, cap: usize) -> bool {
        let cancelled = self.cancel(chat_id);
        self.running.remove(chat_id);
        let granted = self.grant(cap);
        cancelled || granted
    }

    /// Wake waiters, best first, while they fit. A waiter whose run is gone
    /// (receiver dropped) is discarded without taking a slot.
    fn grant(&mut self, cap: usize) -> bool {
        let mut changed = false;
        while let Some(head) = self.waiters.first() {
            if !self.fits(cap, head.depth.is_some()) {
                break;
            }
            let waiter = self.waiters.remove(0);
            changed = true;
            if waiter.grant.send(()).is_ok() {
                self.running.insert(waiter.chat_id, waiter.depth);
            }
        }
        changed
    }

    /// Queue positions of every waiting chat, for publishing.
    pub(crate) fn positions(&self) -> HashMap<String, u32> {
        self.waiters
            .iter()
            .enumerate()
            .map(|(i, w)| (w.chat_id.clone(), u32::try_from(i + 1).unwrap_or(u32::MAX)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(admit: Admit) -> oneshot::Receiver<()> {
        match admit {
            Admit::Queued(rx) => rx,
            Admit::Now => panic!("expected the start to queue"),
        }
    }

    fn now(admit: Admit) {
        assert!(matches!(admit, Admit::Now), "expected the start to run");
    }

    fn granted(rx: &mut oneshot::Receiver<()>) -> bool {
        rx.try_recv().is_ok()
    }

    #[test]
    fn admission_cap_admits_then_queues_in_order() {
        let mut a = Admission::default();
        now(a.admit("a", None, 2));
        now(a.admit("b", None, 2));
        let mut c = queued(a.admit("c", None, 2));
        let mut d = queued(a.admit("d", None, 2));
        assert_eq!(
            (a.position("c"), a.position("d"), a.position("a")),
            (1, 2, 0)
        );
        assert!(a.release("a", 2));
        assert!(granted(&mut c));
        assert!(!granted(&mut d));
        assert_eq!((a.position("c"), a.position("d")), (0, 1));
        assert_eq!(a.running(), 2);
    }

    #[test]
    fn admission_grants_members_first_deepest_first_then_fifo() {
        let mut a = Admission::default();
        now(a.admit("r1", None, 1));
        let mut chat = queued(a.admit("chat", None, 1));
        let mut root = queued(a.admit("root", None, 1));
        // Two members are already running (floor), so these queue.
        now(a.admit("m1", Some(1), 1));
        now(a.admit("m2", Some(1), 1));
        let mut shallow = queued(a.admit("shallow", Some(1), 1));
        let mut deep = queued(a.admit("deep", Some(2), 1));
        let mut shallow2 = queued(a.admit("shallow2", Some(1), 1));
        let order: Vec<_> = ["deep", "shallow", "shallow2", "chat", "root"]
            .iter()
            .map(|c| a.position(c))
            .collect();
        assert_eq!(order, [1, 2, 3, 4, 5]);
        // Free everything: the cap of 1 then hands out one slot at a time.
        for done in ["r1", "m1", "m2"] {
            a.release(done, 1);
        }
        // Floor lets two members start; the third waits for the cap.
        assert!(granted(&mut deep));
        assert!(granted(&mut shallow));
        assert!(!granted(&mut shallow2));
        assert!(!granted(&mut chat) && !granted(&mut root));
        a.release("deep", 1);
        assert!(granted(&mut shallow2));
        assert!(!granted(&mut chat));
        a.release("shallow", 1);
        a.release("shallow2", 1);
        assert!(granted(&mut chat));
        assert!(!granted(&mut root));
        a.release("chat", 1);
        assert!(granted(&mut root));
    }

    #[test]
    fn admission_is_idempotent() {
        let mut a = Admission::default();
        now(a.admit("a", None, 1));
        now(a.admit("a", None, 1));
        assert_eq!(a.running(), 1);
        let _first = queued(a.admit("b", None, 1));
        let mut again = queued(a.admit("b", None, 1));
        assert_eq!(a.position("b"), 1);
        a.release("a", 1);
        assert!(granted(&mut again));
        // Releasing twice frees nothing more.
        a.release("a", 1);
        assert_eq!(a.running(), 1);
    }

    #[test]
    fn admission_cancel_renumbers_the_queue() {
        let mut a = Admission::default();
        now(a.admit("a", None, 1));
        let _b = queued(a.admit("b", None, 1));
        let mut c = queued(a.admit("c", None, 1));
        assert_eq!(a.position("c"), 2);
        assert!(a.cancel("b"));
        assert!(!a.cancel("b"));
        assert_eq!(a.position("c"), 1);
        a.release("a", 1);
        assert!(granted(&mut c));
    }

    #[test]
    fn admission_member_floor_admits_over_the_cap() {
        let mut a = Admission::default();
        now(a.admit("root", None, 1));
        now(a.admit("m1", Some(1), 1));
        now(a.admit("m2", Some(1), 1));
        assert_eq!(a.running(), 3);
        // Floor is spent: a third member waits like anyone else.
        let _m3 = queued(a.admit("m3", Some(1), 1));
        // A plain chat never gets the floor.
        let _chat = queued(a.admit("chat", None, 1));
    }

    #[test]
    fn admission_cap_zero_is_unlimited() {
        let mut a = Admission::default();
        for i in 0..50 {
            now(a.admit(&format!("c{i}"), None, 0));
        }
        assert_eq!(a.running(), 50);
    }

    #[test]
    fn admission_mark_running_never_blocks_but_respects_the_queue() {
        let mut a = Admission::default();
        now(a.admit("a", None, 1));
        a.mark_running("steered", None);
        assert_eq!(a.running(), 2);
        let _q = queued(a.admit("q", None, 1));
        a.mark_running("q", None);
        assert_eq!(a.position("q"), 1);
        assert_eq!(a.running(), 2);
    }

    #[test]
    fn admission_forget_clears_slot_and_wait() {
        let mut a = Admission::default();
        now(a.admit("a", None, 1));
        let mut b = queued(a.admit("b", None, 1));
        assert!(a.forget("a", 1));
        assert!(granted(&mut b));
        assert!(!a.forget("b", 1));
        assert_eq!(a.running(), 0);
        assert!(!a.tracks("a") && !a.tracks("b"));
    }

    #[test]
    fn admission_skips_waiters_whose_run_is_gone() {
        let mut a = Admission::default();
        now(a.admit("a", None, 1));
        drop(queued(a.admit("gone", None, 1)));
        let mut live = queued(a.admit("live", None, 1));
        a.release("a", 1);
        assert!(granted(&mut live));
        assert_eq!(a.running(), 1);
    }
}
