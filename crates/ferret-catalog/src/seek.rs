//! Searches by index over a sorted range read through a decoder, where a
//! slice's own `partition_point` cannot reach.

#[cfg(test)]
mod tests;

/// The first index in `0..n` for which `pred` is false, given that `pred` is
/// true then false over the range.
pub(crate) fn partition_point(n: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// As [`partition_point`], for a boundary near the start: probes 1, 2, 4…
/// rows ahead, then searches the last span.
pub(crate) fn gallop(n: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut step) = (0, 1);
    while lo + step <= n && pred(lo + step - 1) {
        lo += step;
        step *= 2;
    }
    // `pred(lo + step - 1)` failed or lies past the end.
    let hi = (lo + step - 1).min(n);
    lo + partition_point(hi - lo, |i| pred(lo + i))
}

/// As [`partition_point`], from `at`, the answer to a nearby earlier search:
/// a cursor over mostly ascending keys moves a few rows per call, and pays
/// for a search over the whole range only when it moves back.
pub(crate) fn seek(at: usize, n: usize, pred: impl Fn(usize) -> bool) -> usize {
    match at > 0 && !pred(at - 1) {
        true => partition_point(at, pred),
        false => at + gallop(n - at, |i| pred(at + i)),
    }
}
