use super::{gallop, partition_point, seek};

#[test]
fn gallop_finds_every_boundary_partition_point_does() {
    for n in 0..70 {
        for boundary in 0..=n {
            let pred = |i: usize| {
                assert!(i < n, "probed row {i} of {n}");
                i < boundary
            };
            assert_eq!(gallop(n, pred), boundary, "{n} rows");
            assert_eq!(partition_point(n, pred), boundary, "{n} rows");
        }
    }
}

#[test]
fn seek_finds_every_boundary_from_every_start() {
    for n in 0..40 {
        for boundary in 0..=n {
            let pred = |i: usize| {
                assert!(i < n, "probed row {i} of {n}");
                i < boundary
            };
            for at in 0..=n {
                assert_eq!(seek(at, n, pred), boundary, "{n} rows from {at}");
            }
        }
    }
}
