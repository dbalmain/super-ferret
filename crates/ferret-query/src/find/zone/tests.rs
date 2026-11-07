//! Real TZif inputs and adversarial headers exercise the production decoder.

use super::*;

#[test]
fn real_zone_uses_64_bit_table_and_footer() {
    let path = directory().join("Australia/Sydney");
    let bytes = std::fs::read(path).unwrap();
    let zone = Zone::parse(&bytes).unwrap();
    assert_eq!(zone.offset(1704067200), 39600);
    assert_eq!(zone.abbreviation(1704067200), b"AEDT");
    assert_eq!(zone.offset(1719792000), 36000);
    // 2100 is past the stored transitions: the POSIX footer must take over.
    assert_eq!(zone.offset(4102444800), 39600);
    assert_eq!(zone.offset(4118083200), 36000);
}

#[test]
fn malformed_files_fall_back_without_unbounded_counts() {
    let bytes = std::fs::read(directory().join("Australia/Sydney")).unwrap();
    for end in 0..bytes.len() {
        // A complete v1 block inside v2 is still not a complete file.
        assert!(Zone::parse(&bytes[..end]).is_none(), "prefix {end}");
    }
    let mut bad = bytes.clone();
    bad[0] = b'X';
    assert!(Zone::parse(&bad).is_none());
    for count in 0..6 {
        let mut bad = bytes.clone();
        bad[20 + count * 4..24 + count * 4].fill(255);
        assert!(Zone::parse(&bad).is_none(), "count {count}");
    }
    let path = std::env::temp_dir().join(format!("ferret-bad-zone-{}", std::process::id()));
    std::fs::write(&path, b"not TZif").unwrap();
    let fallback = Zone::file(&path).unwrap_or(Zone::utc(b"UTC".to_vec()));
    assert_eq!(fallback.offset(0), 0);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn v1_fixed_offset_and_invalid_transition_types() {
    let mut bytes = vec![0; 44];
    bytes[..4].copy_from_slice(b"TZif");
    bytes[39] = 1;
    bytes[43] = 4;
    bytes.extend_from_slice(&19800i32.to_be_bytes());
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(b"IST\0");
    let zone = Zone::parse(&bytes).unwrap();
    assert_eq!(zone.offset(-1), 19800);
    assert_eq!(zone.abbreviation(0), b"IST");
    bytes[48] = 2;
    assert!(Zone::parse(&bytes).is_none());
}

#[test]
fn posix_rules_handle_southern_dst_julian_days_and_signed_hours() {
    for rule in [
        b"EST5EDT,M3.2.0/2,M11.1.0/2".as_slice(),
        b"EST5EDT,J70/2,J308/2",
        b"EST5EDT,69/2,307/2",
    ] {
        let zone = Posix::parse(rule).unwrap();
        assert_eq!(zone.at(1704067200).seconds, -18000);
        assert_eq!(zone.at(1719792000).seconds, -14400);
    }
    let zone = Posix::parse(b"AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
    assert_eq!(zone.at(1704067200).seconds, 39600);
    assert_eq!(zone.at(1719792000).seconds, 36000);
    assert!(Posix::parse(b"<+0530>-5:30").is_some());
    assert!(Posix::parse(b"AAA0BBB,M1.1.0/-167,M12.5.0/167").is_some());
    for bad in [
        "",
        "A0",
        "AAA999999",
        "AAA0BBB,M0.1.0,M1.0.0",
        "AAA0BBB,J0,J366",
        "AAA0BBB,366,1",
        "AAA0BBB,M1.1.0/168,M2.1.0",
        "AAA0junk!",
    ] {
        assert!(Posix::parse(bad.as_bytes()).is_none(), "{bad}");
    }
}

#[test]
fn version_three_footer_can_keep_dst_all_year() {
    let original = std::fs::read(directory().join("Australia/Sydney")).unwrap();
    let first = Header::parse(&original).unwrap();
    let second_at = 44 + first.size(4).unwrap();
    let second = Header::parse(&original[second_at..]).unwrap();
    let end = second_at + 44 + second.size(8).unwrap();
    let mut bytes = original[..end].to_vec();
    bytes[4] = b'3';
    bytes[second_at + 4] = b'3';
    bytes.extend_from_slice(b"\nEST5EDT,0/0,J365/25\n");
    let zone = Zone::parse(&bytes).unwrap();
    for stamp in [4102444800, 4102466400, 4118083200, 4133980800] {
        assert_eq!(zone.offset(stamp), -14400);
        assert_eq!(zone.abbreviation(stamp), b"EDT");
    }
}

#[test]
fn adjoining_transitions_and_day_boundaries_preserve_extreme_instants() {
    let zone = Zone {
        transitions: Vec::new(),
        offsets: Vec::new(),
        future: Some(Posix::parse(b"EST5EDT,M3.2.0,M11.1.0").unwrap()),
    };
    // D139 round 2: adjoining-year transitions exceed i64 even when the
    // timestamp fits. They must remain real instants, never clamped events.
    assert_eq!(zone.offset(i64::MAX), -18000);
    assert_eq!(zone.offset(i64::MIN), -18000);
    let utc = Zone::utc(b"UTC".to_vec());
    assert_eq!(utc.day_end(i64::MAX), 9_223_372_036_854_806_400);
    assert_eq!(utc.day_end(i64::MIN), -9_223_372_036_854_720_000);
}
