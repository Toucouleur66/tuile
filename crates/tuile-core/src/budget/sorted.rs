// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Sorted vectors standing in for per-step `BTreeSet`s: refilled in place
//! each step, so they keep their capacity, and read by binary search.
//! Ordered like the sets they replace: no decision depends on a hash order.

/// A sorted set without duplicates, refilled in place.
#[derive(Clone, Debug)]
pub(super) struct SortedSet<T>(Vec<T>);

impl<T> Default for SortedSet<T> {
    fn default() -> Self {
        SortedSet(Vec::new())
    }
}

impl<T: Ord + Copy> SortedSet<T> {
    /// Whether `x` is in the set.
    pub fn contains(&self, x: &T) -> bool {
        self.0.binary_search(x).is_ok()
    }

    /// The members, in order.
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.0.iter()
    }

    /// The set becomes `items`, sorted and deduplicated, in the same
    /// buffer.
    pub fn refill(&mut self, items: impl IntoIterator<Item = T>) {
        self.0.clear();
        self.0.extend(items);
        // Equal members are identical: an unstable sort orders them as a
        // stable one would.
        self.0.sort_unstable();
        self.0.dedup();
    }
}

impl<'a, T> IntoIterator for &'a SortedSet<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// The distinct members of four sorted, deduplicated streams, in order:
/// their union, without building it.
pub(super) fn union4<T: Ord + Copy>(
    a: impl Iterator<Item = T>,
    b: impl Iterator<Item = T>,
    c: impl Iterator<Item = T>,
    d: impl Iterator<Item = T>,
) -> impl Iterator<Item = T> {
    let mut a = a.peekable();
    let mut b = b.peekable();
    let mut c = c.peekable();
    let mut d = d.peekable();
    std::iter::from_fn(move || {
        let next = [a.peek(), b.peek(), c.peek(), d.peek()]
            .into_iter()
            .flatten()
            .min()
            .copied()?;
        // Every stream at that member moves past it.
        a.next_if_eq(&next);
        b.next_if_eq(&next);
        c.next_if_eq(&next);
        d.next_if_eq(&next);
        Some(next)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refilled_set_is_sorted_and_distinct() {
        let mut s = SortedSet::default();
        s.refill([5, 1, 3, 1, 5]);
        assert_eq!(s.iter().copied().collect::<Vec<_>>(), [1, 3, 5]);
        assert!(s.contains(&3) && !s.contains(&2));
        s.refill([2]);
        assert_eq!(s.iter().copied().collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn the_union_of_four_streams_is_their_sorted_distinct_members() {
        let u: Vec<_> = union4(
            [1, 4, 9].into_iter(),
            [1, 2].into_iter(),
            std::iter::empty(),
            [2, 9, 10].into_iter(),
        )
        .collect();
        assert_eq!(u, [1, 2, 4, 9, 10]);
    }
}
