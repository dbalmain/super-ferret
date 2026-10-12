//! The real detectors and [`tokenize_with`] on discriminating inputs: text
//! that looks like a target class but is not, beside text that is.

use super::*;
use crate::tokens;

fn kept(text: &str, policy: Policy) -> Vec<String> {
    let mut out = Vec::new();
    tokenize_with(text.as_bytes(), &mut Scratch::default(), policy, |token| {
        out.push(String::from_utf8(token.bytes.to_vec()).expect("tokens are UTF-8"));
    });
    out
}

fn runs(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    base64_runs(text.as_bytes(), |run| out.push(&text[run]));
    out
}

const T1: Policy = Policy {
    base64: true,
    timestamps: false,
};
const T2: Policy = Policy {
    base64: false,
    timestamps: true,
};

#[test]
fn no_policy_is_tokenize() {
    let text = "fooBar 2026-10-12T03:04:05Z iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB 1760238245123 Σx";
    let mut expected = Vec::new();
    tokens(text.as_bytes(), |t| {
        expected.push(String::from_utf8(t.to_vec()).expect("tokens are UTF-8"));
    });
    assert_eq!(kept(text, Policy::NONE), expected);
}

#[test]
fn base64_blobs_are_detected_with_padding() {
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
    let jwt_part = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
    let text = format!("before {png} middle \"{jwt_part}\" after");
    assert_eq!(runs(&text), [png, jwt_part]);
    assert_eq!(kept(&text, T1), ["before", "middle", "after"]);
}

#[test]
fn identifiers_and_hashes_are_not_base64() {
    for word in [
        "getHTTPResponseCodeForRequest2",
        "ThisIsAVeryLongPascalCaseTypeName123",
        "XMLHttpRequestUploadEventTarget",
        "MAX_BUFFER_SIZE_IN_BYTES_V2",
        "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0",
        "550e8400-e29b-41d4-a716-446655440000",
        "src/components/UserProfileCard/index",
        "_ZN4core3fmt5write17h0123456789abcdefE",
        "kSecAttrAccessibleWhenUnlockedThisDeviceOnly1",
        "oAuth2ClientCredentialsToJSON",
        "glTexImage2DMultisampleCoverageNV",
        "Cat_Video_1080_30s_2MB_final",
        "0x7a3F9b2C4d6E8f0A1b3C5d7E9f1A3b5C7d9E1f3A",
        // Bundler chunk names and CSS-module classes: a written name around
        // a short hash.
        "/DatePickerPanel-Xk3pQ9aZ",
        "js/settingsView-Qw7rT2xY",
        "ColorSwatchFns-B7kQx2Lm-Zp4TnR8w",
        "_cardHeader_aB3dE_cardHeader",
    ] {
        assert!(runs(word).is_empty(), "{word}");
        assert_eq!(kept(word, T1), kept(word, Policy::NONE), "{word}");
    }
}

#[test]
fn a_written_prefix_stays_a_term() {
    let text = "sha512-z4PhNX7vuL3xVChQ1m2AB9Yg5AT1dnDTkVbgMW8Rr3vJ6nQwFPsk3pvRdK8JlD2aTvMxH0Uq7cYbE5fZsG4Lhw==";
    let found = runs(text);
    assert_eq!(found.len(), 1);
    assert!(found[0].starts_with("z4Ph"));
    assert_eq!(kept(text, T1), ["sha512", "sha", "512"]);
}

#[test]
fn iso_forms_normalise_to_utc() {
    let cases = [
        ("2026-10-12T03:04:05.123Z", 1_791_774_245, 123_000_000),
        ("2026-10-12 13:04:05+10:00", 1_791_774_245, 0),
        ("2026/10/12 03:04:05", 1_791_774_245, 0),
        ("2026-10-11T22:04:05,5-0500", 1_791_774_245, 500_000_000),
        ("2026-10-12T03:04Z", 1_791_774_240, 0),
        ("2026-10-12", 1_791_763_200, 0),
    ];
    for (text, seconds, nanos) in cases {
        let mut found = Vec::new();
        timestamps(text.as_bytes(), |stamp| found.push(stamp));
        assert_eq!(found.len(), 1, "{text}");
        assert_eq!(
            (found[0].seconds, found[0].nanos, found[0].span.clone()),
            (seconds, nanos, 0..text.len()),
            "{text}"
        );
    }
}

#[test]
fn invalid_dates_and_longer_numbers_are_not_timestamps() {
    for text in [
        "2026-02-30",
        "2026-13-01",
        "2026-10-123",
        "12026-10-12",
        "v1.1760238245",
        "1760238245.5",
        "17602382451",
        "4102444800",
        "1760238245123x",
    ] {
        let mut found = 0;
        timestamps(text.as_bytes(), |_| found += 1);
        assert_eq!(found, 0, "{text}");
    }
}

#[test]
fn time_of_day_and_epochs_leave_but_dates_stay() {
    assert_eq!(
        kept("at 2026-10-12T03:04:05.123Z ok", T2),
        ["at", "2026", "10", "12", "ok"]
    );
    assert_eq!(kept("2026/10/12 03:04:05 x", T2), ["2026", "10", "12", "x"]);
    assert_eq!(kept("\"ts\":1760238245123,", T2), ["ts"]);
    assert_eq!(kept("2026-10-12", T2), kept("2026-10-12", Policy::NONE));
}

#[test]
fn an_in_range_identifier_stays_a_term() {
    // Every number is in EPOCH_RANGE; only the key says what it is.
    for text in [
        "\"id\": 1760238245",
        "userId=1760238245123",
        "msg_ids: 1760238245",
    ] {
        let mut found = 0;
        timestamps(text.as_bytes(), |_| found += 1);
        assert_eq!(found, 0, "{text}");
        assert_eq!(kept(text, T2), kept(text, Policy::NONE), "{text}");
    }
    let mut found = Vec::new();
    timestamps(b"\"time\": 1760238245", |stamp| found.push(stamp.form));
    assert_eq!(found, [Form::EpochSeconds]);
    // Without a key, a number in range is still not an epoch: as many such
    // 10-digit numbers fall just outside the window as in it.
    let mut found = 0;
    timestamps(b"order 1760238245 shipped", |_| found += 1);
    assert_eq!(found, 0);
}

#[test]
fn spans_merge_across_policies() {
    let text = "\"ts\":1760238245123 iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB";
    let mut spans = Vec::new();
    suppressed(
        text.as_bytes(),
        Policy {
            base64: true,
            timestamps: true,
        },
        &mut spans,
    );
    assert_eq!(spans, [5..18, 19..text.len()]);
}
