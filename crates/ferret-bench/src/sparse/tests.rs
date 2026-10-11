//! Soundness checks drive the production Cox fold, the real gram extractor,
//! and the benchmark's four-child candidate evaluation together.

use std::collections::BTreeSet;

use super::*;

fn lists(documents: &[&[u8]], variant: Variant, weights: &[(u64, u16)]) -> Lists {
    let mut lists = Lists::new();
    for (doc, bytes) in documents.iter().enumerate() {
        add_document(&mut lists, doc as u32, bytes, variant, weights, None);
    }
    lists
}

fn keys(need: &Need, out: &mut BTreeSet<Vec<u8>>) {
    match need {
        Need::Key(key) => {
            out.insert(key.clone());
        }
        Need::And(parts) | Need::Or(parts) => parts.iter().for_each(|p| keys(p, out)),
        Need::All | Need::None => {}
    }
}

#[test]
fn a_selective_gram_crossing_the_literal_edge_is_never_required() {
    // Requiring the document's exterior peak pair would lose the same literal
    // between different surrounding bytes. Pin both extraction and derivation.
    let variant = Variant::Sparse {
        frequency: false,
        cap: 32,
    };
    let mut weights = vec![(0, 0); 1 << 16];
    for (pair, rank) in [
        (b"xa", 100),
        (b"ab", 10),
        (b"bc", 1),
        (b"cd", 2),
        (b"de", 1),
        (b"ef", 9),
        (b"fy", 100),
    ] {
        weights[usize::from(u16::from_be_bytes(*pair))] = (rank, 0);
    }
    let flags = GrepFlags {
        case_insensitive: false,
    };
    let facts = flags.required_literals("abcdef").unwrap();
    let need = Need::literals(&facts, variant, &weights);
    let documents: [&[u8]; 3] = [b"xabcdefy", b"zabcdefw", b"abc bcd cde def"];
    let index = lists(&documents, variant, &weights);
    assert_eq!(index.get(b"xabcdefy".as_slice()), Some(&vec![0]));
    let mut required = BTreeSet::new();
    keys(&need, &mut required);
    assert!(required.contains(b"abcdef".as_slice()));
    assert!(!required.contains(b"xabcdefy".as_slice()));
    assert_eq!(need.candidates(&index, &[0, 1, 2]), [0, 1]);
}

#[test]
fn folded_literals_and_unicode_case_alternatives_never_lose_matches() {
    // ASCII folding must happen before weighting. Unicode equivalents remain
    // alternatives from HIR: the raw-byte index must not normalize them away.
    let documents: [&[u8]; 6] = [
        b"xxAbCdEfGhyy",
        b"abcdefgh",
        b"ABC bcd CDE def EFG fgh",
        b"Kilo",
        "Kilo".as_bytes(),
        "İstanbul".as_bytes(),
    ];
    check(&["aBcDeFgH", "kilo", "İstanbul"], &documents, true);
}

#[test]
fn disjoint_alternation_branches_and_unconstrained_branches_are_preserved() {
    // AND-ing unrelated branches loses either match; deleting a short branch
    // loses matches with no usable gram.
    let documents: [&[u8]; 4] = [b"abcdefgh", b"uvwxyz12", b"x", b"none"];
    check(
        &["abcdefgh|uvwxyz12", "abcdefgh|x", "abcdefgh|.*"],
        &documents,
        true,
    );
    let facts = GrepFlags::default()
        .required_literals("abcdefgh|uvwxyz12")
        .unwrap();
    assert!(matches!(facts, RequiredLiterals::Or(_)));
}

#[test]
fn real_derivation_covers_boundaries_repetition_classes_and_raw_bytes() {
    let documents: [&[u8]; 10] = [
        b"aaabbbccc",
        b"ababababcd",
        b"abcdezzz",
        b"ABCDdefg",
        b"abcdefghi",
        b"aaaaa",
        b"prefixZsuffix",
        b"prefixsuffix",
        b"\xffabcd",
        b"",
    ];
    check(
        &[
            "a{3}b{3}c+",
            "(?:ab){4}cd",
            "abc(?:de|fg)",
            "ABCD[de]+fg",
            "a{5,}",
            "prefix.?suffix",
            "prefix.*suffix",
            r"(?-u:\xFF)abcd",
            "(?:abc)?",
            "(?:ab)*",
            "(abcde|abcdefghi)",
        ],
        &documents,
        false,
    );
}

#[test]
fn generated_matches_survive_every_variant_and_frequency_skew() {
    let mut documents = Vec::new();
    for number in 0..4usize.pow(6) {
        let mut n = number;
        let mut bytes = b"prefix".to_vec();
        for _ in 0..6 {
            bytes.push(b'a' + (n % 4) as u8);
            n /= 4;
        }
        bytes.extend_from_slice(b"suffix");
        documents.push(bytes);
    }
    let borrowed: Vec<_> = documents.iter().map(Vec::as_slice).collect();
    check(
        &[
            "prefixa+",
            "ab(cd|aa)",
            "[ab]{2}cd",
            "(?:abc|dca)+suffix",
            "prefix.*suffix",
            "a?b?cd",
            "(prefixa|prefixb)",
        ],
        &borrowed,
        true,
    );
}

fn check(patterns: &[&str], documents: &[&[u8]], insensitive: bool) {
    let flags = GrepFlags {
        case_insensitive: insensitive,
    };
    let mut frequencies = vec![0; 1 << 16];
    for bytes in documents {
        for pair in fold(bytes).windows(2) {
            frequencies[usize::from(u16::from_be_bytes([pair[0], pair[1]]))] += 1;
        }
    }
    let live: Vec<_> = (0..documents.len() as u32).collect();
    for variant in Variant::ALL {
        let weights = weights(
            &frequencies,
            matches!(
                variant,
                Variant::Sparse {
                    frequency: true,
                    ..
                }
            ),
        );
        let index = lists(documents, variant, &weights);
        for pattern in patterns {
            let grep = flags.compile(pattern).unwrap();
            let facts = flags.required_literals(pattern).unwrap();
            let candidates = Need::literals(&facts, variant, &weights).candidates(&index, &live);
            for (doc, bytes) in documents.iter().enumerate() {
                if grep.regex().is_match(bytes) {
                    assert!(
                        candidates.binary_search(&(doc as u32)).is_ok(),
                        "{variant:?} pattern {pattern:?} lost fixture {doc}"
                    );
                }
            }
        }
    }
}

#[test]
fn byte_accounting_uses_real_segments_with_inlined_singletons() {
    let index = lists(&[b"abcdefgh", b"abcdefxx"], Variant::Fixed(4), &[]);
    let mut sorted: Vec<_> = index.iter().collect();
    sorted.sort_unstable_by_key(|(key, _)| *key);
    let mut writer = Writer::keyed(0, 1, 1).unwrap();
    for (key, docs) in sorted {
        writer.push(key, docs).unwrap();
    }
    let mut bytes = Vec::new();
    let sizes = writer.finish(&mut bytes).unwrap();
    assert_eq!(sizes.total(), bytes.len() as u64);
    let encoded = encode(&index, 2).unwrap();
    assert_eq!(encoded.total(), sizes.total());
    assert!(sizes.singleton_bytes > 0);
    assert!(sizes.postings > 0);
    let segment = Segment::open(bytes).unwrap();
    for (key, expected) in index {
        let mut docs = Vec::new();
        match segment.lookup(&key).unwrap().unwrap() {
            ferret_index::segment::Hit::Single(doc) => docs.push(doc),
            ferret_index::segment::Hit::Postings(list) => list.decode(&mut docs),
        }
        assert_eq!(&docs, &expected);
    }
}

#[test]
fn quantiles_and_size_bands_use_candidate_documents_not_gram_occurrences() {
    let counts = counts(&[0, 1, 2], &[1, 1 << 20, 16 << 20], &[true, true, false]);
    assert_eq!(counts[0].docs, 3);
    assert_eq!(counts[0].bytes, 1 + (1 << 20) + (16 << 20));
    assert_eq!(counts[0].readable, 1 + (1 << 20));
    for count in &counts[1..] {
        assert_eq!(count.docs, 1);
    }
    assert_eq!(stats((1..=50).collect()), (25, 45, 1275));
}

#[test]
fn partitioned_maps_produce_byte_identical_segments_and_query_lists() {
    // Partitioning must not turn global front-coding or postings into an
    // estimate, or drop query keys when the full map is released.
    let documents: [&[u8]; 3] = [b"abcdefghijk", b"zzabcdefghijkl", b"123456789abcd"];
    for variant in Variant::ALL {
        let weights = weights(
            &vec![0; 1 << 16],
            matches!(
                variant,
                Variant::Sparse {
                    frequency: true,
                    ..
                }
            ),
        );
        let index = lists(&documents, variant, &weights);
        let mut required = BTreeSet::new();
        required.insert(b"abcdefgh".to_vec());
        required.insert(b"123".to_vec());
        let mut ordinary = Writer::keyed(0, 2, 1).unwrap();
        let mut sorted: Vec<_> = index.iter().collect();
        sorted.sort_unstable_by_key(|(key, _)| *key);
        for (key, docs) in sorted {
            ordinary.push(key, docs).unwrap();
        }
        let mut expected = Vec::new();
        ordinary.finish(&mut expected).unwrap();
        let mut partitioned = Writer::keyed(0, 2, 1).unwrap();
        let mut query_lists = Lists::new();
        for part in 0..32 {
            let mut partition = Lists::new();
            for (doc, bytes) in documents.iter().enumerate() {
                add_document(
                    &mut partition,
                    doc as u32,
                    bytes,
                    variant,
                    &weights,
                    Some(part),
                );
            }
            append_partition(&mut partitioned, partition, &required, &mut query_lists).unwrap();
        }
        let mut actual = Vec::new();
        partitioned.finish(&mut actual).unwrap();
        assert_eq!(actual, expected);
        for key in required {
            assert_eq!(query_lists.get(key.as_slice()), index.get(key.as_slice()));
        }
    }
}

#[test]
fn word_four_keeps_case_alternatives_and_falls_back_across_punctuation() {
    // Dropping long non-word or partial UTF-8 keys must weaken the query too;
    // probing those missing keys would silently lose punctuation literals.
    let bytes = fold("abcd abc_ Ki aſb abc def ab-cd".as_bytes());
    let mut keys = BTreeSet::new();
    extract(&bytes, Variant::Word4, &[], |gram| { keys.insert(gram.to_vec()); });
    assert!(keys.contains(b"abcd".as_slice()));
    assert!(keys.contains(b"abc_".as_slice()));
    assert!(keys.contains("Ki".as_bytes()));
    assert!(keys.contains("aſb".as_bytes()));
    assert!(!keys.contains(b"abc ".as_slice()));
    assert!(!keys.contains(b"ab-c".as_slice()));
    assert!(bytes.windows(3).all(|gram| keys.contains(gram)));
    check(&["abc def", "ab-cd", "kilo", "aſb"], &[b"abc def", b"ab-cd", "Kilo".as_bytes(), "aſb".as_bytes()], true);
}
