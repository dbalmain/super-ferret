//! The real detector and [`tokenize_terms`] on discriminating inputs: text
//! that looks like a blob but is written, beside blobs. Every blob here is
//! synthetic: seeded random bytes, base64-encoded, or the public 1×1 PNG.

use super::*;
use crate::tokens;

fn kept(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    tokenize_terms(text.as_bytes(), &mut Scratch::default(), |token| {
        out.push(String::from_utf8(token.bytes.to_vec()).expect("tokens are UTF-8"));
    });
    out
}

fn all(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    tokens(text.as_bytes(), |t| {
        out.push(String::from_utf8(t.to_vec()).expect("tokens are UTF-8"));
    });
    out
}

fn runs(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    base64_runs(text.as_bytes(), |run| out.push(&text[run]));
    out
}

/// Standard alphabet, two `=` of padding.
const PADDED: &str = "1JOTcOPq4q3fcyTvh2TyF4M5UUU/RrO8u3Z0y+bzGU4Kq4CjigRY3CWFBF1S3Q==";
/// base64url: `-` and `_`, no padding.
const URL: &str = "3EPWpXJ5mLVjvQKbIYmT6FJDQy6N3Mu_KdBNU_v4lvSp";
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

#[test]
fn text_without_a_blob_tokenizes_as_before() {
    let text = "fooBar parseHTTPRequest2 snake_case_name 2026-10-12 Σx";
    assert!(!has_base64_run(text.as_bytes()));
    assert_eq!(kept(text), all(text));
}

#[test]
fn base64_blobs_are_detected_with_padding() {
    let jwt_part = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
    let text = format!("before {PNG} middle \"{jwt_part}\" and {PADDED} after");
    assert_eq!(runs(&text), [PNG, jwt_part, PADDED]);
    assert_eq!(kept(&text), ["before", "middle", "and", "after"]);
}

#[test]
fn base64url_and_data_uris_are_detected() {
    let text = format!("token={URL}; src=\"data:image/png;base64,{PADDED}\"");
    assert_eq!(runs(&text), [URL, PADDED]);
    assert_eq!(
        kept(&text),
        [
            "token", "src", "data", "image", "png", "base64", "base", "64"
        ]
    );
    assert!(has_base64_run(URL.as_bytes()));
}

#[test]
fn identifiers_hashes_and_uuids_are_not_blobs() {
    for word in [
        "getHTTPResponseCodeForRequest2",
        "ThisIsAVeryLongPascalCaseTypeName123",
        "XMLHttpRequestUploadEventTarget",
        "MAX_BUFFER_SIZE_IN_BYTES_V2",
        "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
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
        assert!(!has_base64_run(word.as_bytes()), "{word}");
        assert_eq!(kept(word), all(word), "{word}");
    }
}

#[test]
fn a_written_prefix_stays_a_term() {
    let text = "sha512-z4PhNX7vuL3xVChQ1m2AB9Yg5AT1dnDTkVbgMW8Rr3vJ6nQwFPsk3pvRdK8JlD2aTvMxH0Uq7cYbE5fZsG4Lhw==";
    let found = runs(text);
    assert_eq!(found.len(), 1);
    assert!(found[0].starts_with("z4Ph"));
    assert_eq!(kept(text), ["sha512", "sha", "512"]);
}

#[test]
fn a_run_reaching_into_a_blob_keeps_its_parts_outside_it() {
    // `_` joins the key to the blob in one tokenizer run: the whole run
    // overlaps the blob and leaves, its written part stays.
    let text = format!("key_{URL}");
    assert_eq!(runs(&text), [URL]);
    assert_eq!(kept(&text), ["key"]);
}

#[test]
fn a_short_word_inside_a_blob_leaves_with_it() {
    let blob = "nkzzCi5csBBZh21Dh5Z7n8pP1UaPL+fox+KKcXB0xYeDUnGtBJe3j1LIyw==";
    let text = format!("x {blob} y");
    assert_eq!(runs(&text), [blob]);
    assert!(all(&text).contains(&"fox".to_owned()));
    assert_eq!(kept(&text), ["x", "y"]);
}

#[test]
fn short_random_runs_are_kept() {
    let short = &PADDED[..BASE64_MIN - 1];
    assert!(runs(short).is_empty());
    assert_eq!(kept(short), all(short));
}
