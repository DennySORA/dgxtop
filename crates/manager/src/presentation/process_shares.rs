//! What each tracked GPU process holds of a memory bar now (D-64). Their
//! parts of the history's memory charts come with the history's bars
//! (D-99); the runs are the Process History's (D-76).

use dgxtop_core::identity::EntityId;
use dgxtop_core::process_model::TrackedProcess;
use dgxtop_core::view::ProcessShare;

use super::processes::display_name;

/// What each tracked process holds now on the GPUs `include` accepts, in
/// slot order; a process holding nothing there is left out.
pub(crate) fn process_shares(
    tracked: &[TrackedProcess],
    include: impl Fn(&EntityId) -> bool,
) -> Vec<ProcessShare> {
    tracked
        .iter()
        .filter_map(|process| {
            let bytes = process.bytes_on(&include);
            (bytes > 0).then(|| ProcessShare {
                slot: process.slot,
                pid: process.pid,
                name: display_name(&process.name),
                bytes,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::test_process_key;

    const UNIFIED: &str = "gpu:GPU-aaaa1111";
    const DISCRETE: &str = "gpu:GPU-cccc3333";
    const GIB: u64 = 1 << 30;
    const TRAIN: u8 = 1;
    const SERVE: u8 = 3;

    fn gpu(id: &str) -> EntityId {
        EntityId::new(id).unwrap()
    }

    /// `train` holds 6 GiB of the unified GPU, `serve` 2 GiB of it and
    /// 1 GiB of the discrete one, in the order of their colors.
    fn tracked() -> Vec<TrackedProcess> {
        vec![
            TrackedProcess {
                slot: TRAIN,
                key: test_process_key(1),
                pid: 1,
                name: "train".to_owned(),
                current: vec![(gpu(UNIFIED), 6 * GIB)],
            },
            TrackedProcess {
                slot: SERVE,
                key: test_process_key(2),
                pid: 2,
                name: "serve".to_owned(),
                current: vec![(gpu(UNIFIED), 2 * GIB), (gpu(DISCRETE), GIB)],
            },
        ]
    }

    /// Each process's share of a bar is what it holds on the bar's GPUs,
    /// in the order of its color.
    #[test]
    fn shares_are_what_each_process_holds_on_the_gpus_of_a_bar() {
        let tracked = tracked();
        let unified = process_shares(&tracked, |gpu| gpu.as_str() == UNIFIED);
        let shown: Vec<(u8, &str, u64)> = unified
            .iter()
            .map(|share| (share.slot, share.name.as_str(), share.bytes))
            .collect();
        assert_eq!(
            shown,
            vec![(TRAIN, "train", 6 * GIB), (SERVE, "serve", 2 * GIB)],
            "in color order"
        );
        let discrete = process_shares(&tracked, |gpu| gpu.as_str() == DISCRETE);
        assert_eq!(discrete.len(), 1);
        assert_eq!((discrete[0].slot, discrete[0].bytes), (SERVE, GIB));
    }
}
