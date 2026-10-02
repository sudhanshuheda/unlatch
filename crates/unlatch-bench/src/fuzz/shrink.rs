//! Delta debugging (ddmin) over an op list: the smallest sub-sequence that still fails.

/// Minimise `ops` while `still_fails` holds, calling it at most `max_runs` times. The result is
/// 1-minimal when the budget suffices (removing any single op makes the failure go away).
pub fn ddmin<T: Clone>(
    ops: &[T],
    still_fails: &mut dyn FnMut(&[T]) -> bool,
    max_runs: usize,
) -> Vec<T> {
    let mut cur: Vec<T> = ops.to_vec();
    let mut n = 2usize;
    let mut runs = 0usize;
    while cur.len() >= 2 && runs < max_runs {
        let chunk = cur.len().div_ceil(n);
        let mut reduced = false;
        let mut start = 0;
        while start < cur.len() && runs < max_runs {
            let end = (start + chunk).min(cur.len());
            let complement: Vec<T> = cur[..start].iter().chain(&cur[end..]).cloned().collect();
            runs += 1;
            if !complement.is_empty() && still_fails(&complement) {
                cur = complement;
                n = (n - 1).max(2);
                reduced = true;
                break;
            }
            start = end;
        }
        if !reduced {
            if n >= cur.len() {
                break;
            }
            n = (n * 2).min(cur.len());
        }
    }
    cur
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_two_culprits() {
        let ops: Vec<u32> = (0..100).collect();
        let mut calls = 0;
        let min = ddmin(
            &ops,
            &mut |s: &[u32]| {
                calls += 1;
                s.contains(&17) && s.contains(&83)
            },
            10_000,
        );
        assert_eq!(min, vec![17, 83]);
        assert!(calls < 400, "{calls} runs");
    }

    #[test]
    fn respects_the_run_budget() {
        let ops: Vec<u32> = (0..64).collect();
        let mut calls = 0;
        let min = ddmin(
            &ops,
            &mut |s: &[u32]| {
                calls += 1;
                s.contains(&5)
            },
            3,
        );
        assert_eq!(calls, 3);
        assert!(min.contains(&5));
    }

    #[test]
    fn order_is_preserved() {
        let ops = vec!['a', 'b', 'c', 'd', 'e'];
        let min = ddmin(
            &ops,
            &mut |s: &[char]| s.windows(1).count() >= 2 && s.contains(&'d') && s.contains(&'b'),
            100,
        );
        assert_eq!(min, vec!['b', 'd']);
    }
}
