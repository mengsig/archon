//! Independent work in parallel: the work of one lane runs in order, and up
//! to [`MAX_LANES`] lanes run at once. A lane is what must not overlap: a
//! run's spawns, or one repository's git work (git locks its shared config).

use std::collections::BTreeMap;
use std::future::Future;

use futures_util::StreamExt as _;

/// Most lanes worked at once; the rest wait for one to finish.
pub(crate) const MAX_LANES: usize = 8;

/// `keyed` grouped into lanes by key, each lane in the order given.
pub(crate) fn group<K: Ord, T>(keyed: impl IntoIterator<Item = (K, T)>) -> BTreeMap<K, Vec<T>> {
    let mut lanes: BTreeMap<K, Vec<T>> = BTreeMap::new();
    for (key, item) in keyed {
        lanes.entry(key).or_default().push(item);
    }
    debug_assert!(lanes.values().all(|lane| !lane.is_empty()));
    lanes
}

/// `work` for every item of `lanes`: one lane's items in order, lanes in
/// parallel. Returns every result, lane by lane, each lane's in order.
pub(crate) async fn run<K, T, R, F, Fut>(lanes: BTreeMap<K, Vec<T>>, work: F) -> Vec<R>
where
    K: Ord,
    F: Fn(T) -> Fut,
    Fut: Future<Output = R>,
{
    let total: usize = lanes.values().map(Vec::len).sum();
    let work = &work;
    let mut by_lane: Vec<(usize, Vec<R>)> =
        futures_util::stream::iter(lanes.into_values().enumerate())
            .map(|(at, lane)| async move {
                let mut done = Vec::with_capacity(lane.len());
                for item in lane {
                    done.push(work(item).await);
                }
                (at, done)
            })
            .buffer_unordered(MAX_LANES)
            .collect()
            .await;
    by_lane.sort_unstable_by_key(|(at, _)| *at);
    let results: Vec<R> = by_lane.into_iter().flat_map(|(_, done)| done).collect();
    assert_eq!(results.len(), total, "every item is worked exactly once");
    results
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::time::Duration;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn lanes_overlap_and_each_keeps_its_order() {
        let log = RefCell::new(Vec::new());
        let lanes = group([("a", ("a1", 30)), ("b", ("b1", 10)), ("a", ("a2", 0))]);
        let started = tokio::time::Instant::now();
        let results = run(lanes, |(name, millis)| {
            let log = &log;
            async move {
                tokio::time::sleep(Duration::from_millis(millis)).await;
                log.borrow_mut().push(name);
                name
            }
        })
        .await;
        assert_eq!(results, ["a1", "a2", "b1"], "lane by lane, in order");
        assert_eq!(
            *log.borrow(),
            ["b1", "a1", "a2"],
            "b finished while a waited"
        );
        assert_eq!(started.elapsed(), Duration::from_millis(30), "not 40");
    }

    #[tokio::test(start_paused = true)]
    async fn no_more_than_the_bound_run_at_once() {
        let (running, most) = (RefCell::new(0usize), RefCell::new(0usize));
        let lanes = group((0..MAX_LANES * 2 + 1).map(|n| (n, n)));
        let results = run(lanes, |n| {
            let (running, most) = (&running, &most);
            async move {
                *running.borrow_mut() += 1;
                let now = *running.borrow();
                most.replace_with(|most| (*most).max(now));
                tokio::time::sleep(Duration::from_millis(5)).await;
                *running.borrow_mut() -= 1;
                n
            }
        })
        .await;
        assert_eq!(results.len(), MAX_LANES * 2 + 1);
        assert_eq!(*most.borrow(), MAX_LANES);
    }
}
