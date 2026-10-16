//! The binary search for the first red merge: over the first-parent commits
//! after a green baseline, oldest first, the last of which is known red.

/// Where the search stands: every index up to `good` passed (none when
/// `None`: only the baseline did) and `bad` failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bisect {
    good: Option<usize>,
    bad: usize,
}

impl Bisect {
    /// A search over `len` merges, of which `known_bad` (else the last) failed.
    pub fn new(len: usize, known_bad: Option<usize>) -> Self {
        assert!(len > 0, "a red head is at least one merge past green");
        let bad = known_bad.unwrap_or(len - 1);
        assert!(bad < len, "the known red merge is one of them");
        Self { good: None, bad }
    }

    /// The merge to verify next; `None` once the first red one is found.
    pub fn next(&self) -> Option<usize> {
        let low = self.good.map_or(0, |good| good + 1);
        assert!(low <= self.bad, "a good merge never follows a bad one");
        (low < self.bad).then(|| low + (self.bad - low) / 2)
    }

    /// How the merge at `index`, which [`Self::next`] named, fared.
    pub fn record(&mut self, index: usize, red: bool) {
        assert_eq!(Some(index), self.next(), "only the asked merge is recorded");
        if red {
            self.bad = index;
        } else {
            self.good = Some(index);
        }
    }

    /// The first red merge, once the search is done.
    pub fn first_bad(&self) -> Option<usize> {
        self.next().is_none().then_some(self.bad)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first red merge of `len` when it is `culprit`, and how many
    /// merges were verified to find it.
    fn search(len: usize, culprit: usize, known_bad: Option<usize>) -> (usize, usize) {
        let mut bisect = Bisect::new(len, known_bad);
        let mut runs = 0;
        while let Some(index) = bisect.next() {
            bisect.record(index, index >= culprit);
            runs += 1;
        }
        (bisect.first_bad().expect("the search ended"), runs)
    }

    #[test]
    fn every_culprit_is_found_in_logarithmic_runs() {
        for len in 1..=17 {
            for culprit in 0..len {
                let (found, runs) = search(len, culprit, None);
                assert_eq!(found, culprit, "{culprit} of {len}");
                assert!(runs <= len.ilog2() as usize + 1, "{runs} runs for {len}");
            }
        }
    }

    #[test]
    fn a_known_red_merge_bounds_the_search() {
        assert_eq!(
            search(5, 2, Some(2)),
            (2, 1),
            "only merge 1 is left to verify"
        );
        assert_eq!(search(5, 1, Some(2)), (1, 2));
        assert_eq!(search(5, 0, Some(0)), (0, 0), "nothing left to verify");
        assert_eq!(Bisect::new(1, None).first_bad(), Some(0));
        assert_eq!(Bisect::new(3, None).first_bad(), None, "not searched yet");
    }
}
