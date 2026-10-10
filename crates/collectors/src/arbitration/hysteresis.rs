//! Step 8: switching hysteresis between healthy sources.
//!
//! A healthy current source is replaced by a better challenger only after the
//! challenger won `switch_successes` consecutive rounds AND has been winning
//! for at least `switch_min_hold_ms` (measured from its first win in the
//! streak). An unhealthy (failed, stale, missing) current source is replaced
//! immediately. Hysteresis never delays a hard-failure fallback.

use dgxtop_core::canonical::SelectionMemo;
use dgxtop_core::identity::SourceId;

pub(super) struct Input<'a> {
    pub memo: Option<&'a SelectionMemo>,
    /// Best-ranked healthy representative of this round.
    pub best: &'a SourceId,
    /// The memo's current source is eligible this round.
    pub current_healthy: bool,
    pub now_boot_ns: u64,
    pub required_streak: u32,
    pub min_hold_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Verdict {
    pub selected: SourceId,
    pub memo: SelectionMemo,
    /// Better challenger kept waiting by hysteresis.
    pub held_challenger: Option<SourceId>,
}

pub(super) fn decide(input: &Input<'_>) -> Verdict {
    let current = input.memo.and_then(|memo| {
        memo.selected
            .as_ref()
            .filter(|_| input.current_healthy)
            .map(|selected| (memo, selected))
    });
    match current {
        Some((memo, selected)) if selected == input.best => Verdict {
            selected: selected.clone(),
            memo: settled(selected.clone(), memo.selected_since_boot_ns),
            held_challenger: None,
        },
        Some((memo, selected)) => challenge(input, memo, selected),
        None => switch_to(input.best, input.now_boot_ns),
    }
}

/// Memo of a source held without a pending challenger.
pub(super) fn settled(selected: SourceId, since_boot_ns: u64) -> SelectionMemo {
    SelectionMemo {
        selected: Some(selected),
        selected_since_boot_ns: since_boot_ns,
        challenger: None,
        challenger_streak: 0,
        challenger_since_boot_ns: 0,
    }
}

fn switch_to(best: &SourceId, now_boot_ns: u64) -> Verdict {
    Verdict {
        selected: best.clone(),
        memo: settled(best.clone(), now_boot_ns),
        held_challenger: None,
    }
}

fn challenge(input: &Input<'_>, memo: &SelectionMemo, selected: &SourceId) -> Verdict {
    let continuing = memo.challenger.as_ref() == Some(input.best);
    let streak = if continuing {
        memo.challenger_streak.saturating_add(1)
    } else {
        1
    };
    let since = if continuing {
        memo.challenger_since_boot_ns
    } else {
        input.now_boot_ns
    };
    let held_for = input.now_boot_ns.saturating_sub(since);
    if streak >= input.required_streak && held_for >= input.min_hold_ns {
        return switch_to(input.best, input.now_boot_ns);
    }
    Verdict {
        selected: selected.clone(),
        memo: SelectionMemo {
            selected: Some(selected.clone()),
            selected_since_boot_ns: memo.selected_since_boot_ns,
            challenger: Some(input.best.clone()),
            challenger_streak: streak,
            challenger_since_boot_ns: since,
        },
        held_challenger: Some(input.best.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::testing::source_id;

    const SECOND: u64 = 1_000_000_000;

    fn input<'a>(
        memo: Option<&'a SelectionMemo>,
        best: &'a SourceId,
        healthy: bool,
        now: u64,
    ) -> Input<'a> {
        Input {
            memo,
            best,
            current_healthy: healthy,
            now_boot_ns: now,
            required_streak: 3,
            min_hold_ns: 5 * SECOND,
        }
    }

    #[test]
    fn challenger_needs_three_wins_and_five_seconds() {
        let current = source_id("b");
        let challenger = source_id("a");
        let mut memo = settled(current.clone(), 0);
        let mut switched_at = None;
        for second in 1..=10u64 {
            let verdict = decide(&input(Some(&memo), &challenger, true, second * SECOND));
            if verdict.selected == challenger {
                switched_at = Some(second);
                break;
            }
            assert_eq!(verdict.held_challenger.as_ref(), Some(&challenger));
            memo = verdict.memo;
        }
        // Wins at 1..=6 s: the streak reaches 3 at 3 s but 5 s of winning
        // only elapse at 6 s.
        assert_eq!(switched_at, Some(6));
    }

    #[test]
    fn slow_rounds_still_need_three_wins() {
        let current = source_id("b");
        let challenger = source_id("a");
        let memo = settled(current.clone(), 0);
        let first = decide(&input(Some(&memo), &challenger, true, 10 * SECOND));
        let second = decide(&input(Some(&first.memo), &challenger, true, 20 * SECOND));
        assert_eq!(
            second.selected, current,
            "two wins over 10 s are not enough"
        );
        let third = decide(&input(Some(&second.memo), &challenger, true, 30 * SECOND));
        assert_eq!(third.selected, challenger);
    }

    #[test]
    fn interrupted_streak_restarts() {
        let current = source_id("b");
        let challenger = source_id("a");
        let other = source_id("c");
        let memo = settled(current.clone(), 0);
        let one = decide(&input(Some(&memo), &challenger, true, SECOND));
        let interrupted = decide(&input(Some(&one.memo), &other, true, 2 * SECOND));
        assert_eq!(interrupted.memo.challenger_streak, 1);
        assert_eq!(interrupted.memo.challenger.as_ref(), Some(&other));
        let back_to_current = decide(&input(Some(&interrupted.memo), &current, true, 3 * SECOND));
        assert_eq!(back_to_current.memo.challenger, None);
        assert_eq!(back_to_current.memo.challenger_streak, 0);
    }

    #[test]
    fn unhealthy_current_switches_immediately() {
        let current = source_id("b");
        let fallback = source_id("c");
        let memo = settled(current, 0);
        let verdict = decide(&input(Some(&memo), &fallback, false, SECOND));
        assert_eq!(verdict.selected, fallback);
        assert_eq!(verdict.memo.selected_since_boot_ns, SECOND);
    }
}
