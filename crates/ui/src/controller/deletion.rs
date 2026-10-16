//! A deletion of Process History runs (D-108), sent a command at a time:
//! each command holds the runs that fit the smallest command any Manager
//! takes, so no configuration refuses it, and the next leaves once the
//! last is answered, so a deletion of many runs never fills the queue.

use std::collections::VecDeque;

use dgxtop_core::identity::ProcessKey;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RunDeletion {
    /// The commands' runs still to send.
    waiting: VecDeque<Vec<ProcessKey>>,
    /// The command sent and not answered yet, and its runs.
    sent: Option<(u128, Vec<ProcessKey>)>,
    /// Runs of the commands the Manager completed.
    deleted: usize,
}

impl RunDeletion {
    pub(super) fn new(batches: Vec<Vec<ProcessKey>>) -> Self {
        Self {
            waiting: batches.into(),
            sent: None,
            deleted: 0,
        }
    }

    /// The next command's runs; none once every command left.
    pub(super) fn next_batch(&mut self) -> Option<Vec<ProcessKey>> {
        self.waiting.pop_front()
    }

    /// The command of `runs` left as request `request_id`.
    pub(super) fn sent(&mut self, request_id: u128, runs: Vec<ProcessKey>) {
        self.sent = Some((request_id, runs));
    }

    /// The request waiting for its answer.
    pub(super) fn awaited(&self) -> Option<u128> {
        self.sent.as_ref().map(|(request_id, _)| *request_id)
    }

    /// The runs of request `request_id` when it is the one awaited.
    pub(super) fn answered(&mut self, request_id: u128) -> Option<Vec<ProcessKey>> {
        if self.awaited() != Some(request_id) {
            return None;
        }
        self.sent.take().map(|(_, runs)| runs)
    }

    /// `runs` more were deleted.
    pub(super) fn count_deleted(&mut self, runs: usize) {
        self.deleted += runs;
    }

    pub(super) fn deleted(&self) -> usize {
        self.deleted
    }

    /// Runs not deleted yet: waiting or awaiting their answer.
    pub(super) fn left(&self) -> usize {
        let awaiting = self.sent.as_ref().map_or(0, |(_, runs)| runs.len());
        self.waiting.iter().map(Vec::len).sum::<usize>() + awaiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::test_process_key;

    #[test]
    fn a_deletion_sends_its_batches_one_answer_at_a_time() {
        let runs = |pids: &[i64]| -> Vec<ProcessKey> {
            pids.iter().map(|pid| test_process_key(*pid)).collect()
        };
        let mut deletion = RunDeletion::new(vec![runs(&[1, 2]), runs(&[3])]);
        assert_eq!(deletion.left(), 3);
        let first = deletion.next_batch().unwrap();
        deletion.sent(7, first.clone());
        assert_eq!(deletion.awaited(), Some(7));
        assert_eq!(deletion.answered(8), None, "another request");
        assert_eq!(deletion.answered(7), Some(first));
        assert_eq!(deletion.answered(7), None, "answered once");
        deletion.count_deleted(2);
        assert_eq!((deletion.deleted(), deletion.left()), (2, 1));
        assert_eq!(deletion.next_batch(), Some(runs(&[3])));
        assert_eq!(deletion.next_batch(), None);
    }
}
