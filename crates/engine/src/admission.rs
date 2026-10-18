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
//!
//! An orchestrator's user may also limit how many of its tree's writing
//! sessions run a turn at once ([`TreeLimit`]). A writer over the limit
//! queues like one over the cap, but it waits only for its own tree: a tree at
//! its limit never holds up another tree's start behind it.

use std::collections::HashMap;

use tokio::sync::oneshot;

/// Member turns that may always start, cap or not.
pub(crate) const MEMBER_FLOOR: usize = 2;

/// A turn of a writing session in an orchestrator's tree, counted against the
/// tree's limit on concurrent writers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeLimit {
    pub(crate) root: String,
    pub(crate) limit: usize,
}

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
    /// The tree limit it was admitted under, if it writes in a tree.
    tree: Option<TreeLimit>,
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
    /// The orchestrator root of each running writer that counts against a
    /// tree's limit.
    trees: HashMap<String, String>,
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

    fn tree_running(&self, root: &str) -> usize {
        self.trees.values().filter(|r| r.as_str() == root).count()
    }

    fn fits_global(&self, cap: usize, member: bool) -> bool {
        cap == 0 || self.running.len() < cap || (member && self.member_running() < MEMBER_FLOOR)
    }

    fn fits_tree(&self, tree: Option<&TreeLimit>) -> bool {
        tree.is_none_or(|tree| self.tree_running(&tree.root) < tree.limit)
    }

    fn fits(&self, cap: usize, member: bool, tree: Option<&TreeLimit>) -> bool {
        self.fits_global(cap, member) && self.fits_tree(tree)
    }

    /// [`Self::admit_in`] for a chat that is in no tree's limit.
    #[cfg(test)]
    pub(crate) fn admit(&mut self, chat_id: &str, depth: Option<usize>, cap: usize) -> Admit {
        self.admit_in(chat_id, depth, cap, None)
    }

    /// Ask for a slot for a fresh run. Idempotent: a chat that already holds
    /// one is `Now`; one already queued keeps its place. `tree` is the limit
    /// of the orchestrator tree a writer belongs to.
    pub(crate) fn admit_in(
        &mut self,
        chat_id: &str,
        depth: Option<usize>,
        cap: usize,
        tree: Option<TreeLimit>,
    ) -> Admit {
        // A raised cap (or a freed floor) may have room the queue has not used.
        self.grant(cap);
        if self.running.contains_key(chat_id) {
            return Admit::Now;
        }
        let (tx, rx) = oneshot::channel();
        if let Some(waiter) = self.waiters.iter_mut().find(|w| w.chat_id == chat_id) {
            waiter.grant = tx;
            // The limit as it stands now.
            waiter.tree = tree;
            return Admit::Queued(rx);
        }
        // After the grant above, only a member over the floor can still fit
        // while others wait.
        if self.fits(cap, depth.is_some(), tree.as_ref()) {
            self.running.insert(chat_id.to_owned(), depth);
            if let Some(tree) = tree {
                self.trees.insert(chat_id.to_owned(), tree.root);
            }
            return Admit::Now;
        }
        self.seq += 1;
        let waiter = Waiter {
            chat_id: chat_id.to_owned(),
            depth,
            tree,
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
    #[cfg(test)]
    pub(crate) fn mark_running(&mut self, chat_id: &str, depth: Option<usize>) {
        self.mark_running_in(chat_id, depth, None);
    }

    /// [`Self::mark_running`] for a writer of the tree rooted at `tree_root`,
    /// so it counts against the tree's limit.
    pub(crate) fn mark_running_in(
        &mut self,
        chat_id: &str,
        depth: Option<usize>,
        tree_root: Option<String>,
    ) {
        if self.waiters.iter().any(|w| w.chat_id == chat_id) {
            return;
        }
        self.running.entry(chat_id.to_owned()).or_insert(depth);
        if let Some(root) = tree_root {
            self.trees.entry(chat_id.to_owned()).or_insert(root);
        }
    }

    /// Free `chat_id`'s slot and hand freed slots to the queue. Returns whether
    /// the queue changed.
    pub(crate) fn release(&mut self, chat_id: &str, cap: usize) -> bool {
        self.running.remove(chat_id);
        self.trees.remove(chat_id);
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
        self.trees.remove(chat_id);
        let granted = self.grant(cap);
        cancelled || granted
    }

    /// Wake waiters, best first, while they fit. A waiter whose run is gone
    /// (receiver dropped) is discarded without taking a slot. One held back
    /// only by its own tree's limit is passed over, so a tree at its limit
    /// never holds up the starts behind it; one the cap holds back stops the
    /// scan, as the cap holds back everyone after it.
    fn grant(&mut self, cap: usize) -> bool {
        let mut changed = false;
        let mut at = 0;
        while let Some(next) = self.waiters.get(at) {
            if !self.fits_global(cap, next.depth.is_some()) {
                break;
            }
            if !self.fits_tree(next.tree.as_ref()) {
                at += 1;
                continue;
            }
            let waiter = self.waiters.remove(at);
            changed = true;
            if waiter.grant.send(()).is_ok() {
                if let Some(tree) = waiter.tree {
                    self.trees.insert(waiter.chat_id.clone(), tree.root);
                }
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

    fn writer(root: &str, limit: usize) -> Option<TreeLimit> {
        Some(TreeLimit {
            root: root.into(),
            limit,
        })
    }

    #[test]
    fn a_trees_writer_limit_queues_the_extra_writers_and_spares_everyone_else() {
        let mut a = Admission::default();
        // No global cap: only the tree's limit of 2 writers holds anyone back.
        now(a.admit_in("w1", Some(1), 0, writer("orchestrator-a", 2)));
        now(a.admit_in("w2", Some(1), 0, writer("orchestrator-a", 2)));
        let mut w3 = queued(a.admit_in("w3", Some(1), 0, writer("orchestrator-a", 2)));
        let mut w4 = queued(a.admit_in("w4", Some(2), 0, writer("orchestrator-a", 2)));
        assert_eq!((a.position("w4"), a.position("w3")), (1, 2), "deeper first");
        // A reader of the same tree, another tree's writer and a plain chat
        // start at once.
        now(a.admit_in("reader", Some(1), 0, None));
        now(a.admit_in("other", Some(1), 0, writer("orchestrator-b", 1)));
        now(a.admit_in("chat", None, 0, None));
        // A freed writer slot goes to the next in line, one at a time.
        assert!(a.release("w1", 0));
        assert!(granted(&mut w4) && !granted(&mut w3));
        assert!(a.release("w2", 0));
        assert!(granted(&mut w3));
        assert_eq!(a.position("w3") + a.position("w4"), 0);
    }

    #[test]
    fn a_tree_at_its_limit_does_not_hold_up_another_trees_waiter_behind_it() {
        let mut a = Admission::default();
        now(a.admit_in("a1", Some(1), 0, writer("orchestrator-a", 1)));
        now(a.admit_in("b1", Some(1), 0, writer("orchestrator-b", 1)));
        let mut a2 = queued(a.admit_in("a2", Some(1), 0, writer("orchestrator-a", 1)));
        let mut b2 = queued(a.admit_in("b2", Some(1), 0, writer("orchestrator-b", 1)));
        // b's writer frees up while a's waiter is ahead of b2 in the queue.
        assert!(a.release("b1", 0));
        assert!(granted(&mut b2), "b2 is not stuck behind a2");
        assert!(!granted(&mut a2));
        assert_eq!(a.position("a2"), 1);
        assert!(a.release("a1", 0));
        assert!(granted(&mut a2));
    }

    #[test]
    fn a_steered_writer_counts_against_its_tree_without_waiting() {
        let mut a = Admission::default();
        a.mark_running_in("steered", Some(1), Some("orchestrator-a".into()));
        let mut next = queued(a.admit_in("next", Some(1), 0, writer("orchestrator-a", 1)));
        assert!(a.release("steered", 0));
        assert!(granted(&mut next));
        // Finished writers hold nothing.
        assert!(a.release("next", 0));
        now(a.admit_in("again", Some(1), 0, writer("orchestrator-a", 1)));
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
