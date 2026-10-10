//! Ordering helpers shared by list builders. Missing values always sort
//! after present ones so an unreadable device never looks idle or busy.

use std::cmp::Ordering;

/// Present values first, larger first; missing values last.
pub(crate) fn descending_present(left: Option<f64>, right: Option<f64>) -> Ordering {
    match (finite(left), finite(right)) {
        (Some(left), Some(right)) => right.total_cmp(&left),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Sum of the present values; `None` when every value is missing (a missing
/// rate is never counted as 0).
pub(crate) fn sum_present(values: impl IntoIterator<Item = Option<f64>>) -> Option<f64> {
    values
        .into_iter()
        .filter_map(finite)
        .fold(None, |total, value| Some(total.unwrap_or(0.0) + value))
}

fn finite(value: Option<f64>) -> Option<f64> {
    value.filter(|number| number.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_sorts_last_and_larger_first() {
        let mut values = vec![None, Some(1.0), Some(5.0), None, Some(0.0)];
        values.sort_by(|left, right| descending_present(*left, *right));
        assert_eq!(values, vec![Some(5.0), Some(1.0), Some(0.0), None, None]);
    }

    #[test]
    fn non_finite_counts_as_missing() {
        assert_eq!(
            descending_present(Some(f64::NAN), Some(1.0)),
            Ordering::Greater
        );
    }

    #[test]
    fn sum_keeps_missing_distinct_from_zero() {
        assert_eq!(sum_present([None, None]), None);
        assert_eq!(sum_present([Some(0.0), None]), Some(0.0));
        assert_eq!(sum_present([Some(2.0), Some(3.0)]), Some(5.0));
    }
}
