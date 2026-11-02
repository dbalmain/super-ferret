use std::ffi::OsString;

use ferret_verify::Trigrams;

use super::*;

// A refused command reaches rg without the arguments the parser took as
// `--rg`, and with every `--rg` it took as something else: a value, a
// positional after `--`, or the unsupported `--rg=x`. After a flag it
// refuses, the parse knows nothing: `-Pe --rg` is a pattern to rg, and
// `--pre-glob -e --rg` a flag rg rejects, so both reach rg as they are.
#[test]
fn own_flags_are_the_rg_flags_the_parser_took() {
    let own = |args: &[&str]| {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        own_flags(&args)
    };
    assert_eq!(own(&["--rg", "--sort", "path", "red"]), [0]);
    assert_eq!(own(&["-n", "--rg", "--sort", "path", "hit"]), [1]);
    assert_eq!(own(&["--rg", "red", "--rg"]), [0, 2]);
    for args in [
        &["-e", "--rg", "red"][..],
        &["--regexp", "--rg"],
        &["-g", "--rg", "red"],
        &["red", "--", "--rg"],
        &["--rg=x", "red"],
    ] {
        assert_eq!(own(args), [0usize; 0], "{args:?}");
    }
    assert_eq!(own(&["--rg", "--pcre2", "--rg", "red"]), [0]);
    for args in [
        &["-Pe", "--rg", "red"][..],
        &["-uuue", "--rg", "red"],
        &["--pre-glob", "-e", "--rg", "red"],
        &["--pre-glob", "--", "--rg", "red"],
    ] {
        assert_eq!(own(args), [0usize; 0], "{args:?}");
    }
}

// D128: hidden names show unless `--rg`; an explicit switch wins in
// either mode, in argv order.
#[test]
fn rg_mode_decides_hidden_unless_a_switch_does() {
    let hidden = |args: &[&str]| {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        match parse(&args).unwrap() {
            Command::Search(plan) => (plan.hidden, plan.traversal.rg),
            Command::Files(names) => (names.hidden, names.traversal.rg),
            _ => unreachable!(),
        }
    };
    assert_eq!(hidden(&["x"]), (true, false));
    assert_eq!(hidden(&["--files"]), (true, false));
    assert_eq!(hidden(&["--rg", "x"]), (false, true));
    assert_eq!(hidden(&["--rg", "--files"]), (false, true));
    assert_eq!(hidden(&["--no-hidden", "x"]), (false, false));
    assert_eq!(hidden(&["--rg", "--hidden", "x"]), (true, true));
    assert_eq!(hidden(&["--rg", "-uu", "x"]), (true, true));
    assert_eq!(hidden(&["--rg", "-uu", "--no-hidden", "x"]), (false, true));
    assert_eq!(hidden(&["--hidden", "--no-hidden", "x"]), (false, false));
}

#[test]
fn process_relative_directories_are_not_the_same_in_every_process() {
    let here = std::env::current_dir().unwrap();
    assert!(same_in_every_process(Path::new(".")));
    assert!(same_in_every_process(&here));
    let symlinked = std::env::temp_dir().join(format!("ferret-cwd-link-{}", std::process::id()));
    std::os::unix::fs::symlink(&here, &symlinked).unwrap();
    assert!(same_in_every_process(&symlinked));
    std::fs::remove_file(&symlinked).unwrap();
    let descriptor = std::fs::File::open(&here).unwrap();
    let fd = std::os::fd::AsRawFd::as_raw_fd(&descriptor);
    for path in [
        "/proc/self/cwd".to_owned(),
        format!("/proc/self/fd/{fd}"),
        format!("/dev/fd/{fd}"),
        "/proc/self".to_owned(),
        "/proc/self/fd".to_owned(),
    ] {
        assert!(Path::new(&path).is_dir(), "{path}");
        assert!(!same_in_every_process(Path::new(&path)), "{path}");
    }
    assert!(!same_in_every_process(Path::new("Cargo.toml")));
}

// Compare the actual scanner with its old path, including the source
// metadata consumed by plain rendering, replacement and JSON encoding.
fn compare_scanners(plan: &Plan, bytes: &[u8]) {
    let scan = |plan: &Plan| {
        let mut output = Vec::new();
        let mut result = scan_lines(
            plan,
            &mut &bytes[..],
            bytes.len() as u64,
            &mut Vec::new(),
            &|| false,
            |line| {
                output.push((
                    line.offset,
                    line.source_lines,
                    line.terminated,
                    line.number,
                    line.bytes.to_vec(),
                    line.kind,
                    line.fragment,
                    line.highlights.map(<[_]>::to_vec),
                ));
                Ok(())
            },
        )
        .unwrap();
        result.elapsed = std::time::Duration::ZERO;
        (output, result)
    };
    // The line-local paths count lines only when something prints them, so
    // an unnumbered plan compares everything else; a numbered copy compares
    // the numbers too. Context separators still read whether a line follows
    // the previous one, so that much is compared under context.
    let mut numbered = plan.clone();
    numbered.line_number = true;
    let context = plan.before > 0 || plan.after > 0;
    for (plan, masked) in [(plan, !plan.line_number && !plan.json), (&numbered, false)] {
        let mask = |(mut output, result): (Vec<_>, _)| {
            if masked {
                let mut previous = None;
                for line in &mut output {
                    let line: &mut (_, _, _, u64, _, _, _, _) = line;
                    let gap = previous.is_some_and(|previous: u64| line.3 > previous + 1);
                    previous = Some(line.3);
                    line.3 = u64::from(context && gap);
                }
            }
            (output, result)
        };
        let actual = mask(scan(plan));
        let mut slow = plan.clone();
        slow.force_per_line = true;
        let expected = mask(scan(&slow));
        assert_eq!(
            actual,
            expected,
            "pattern {:?}, {} input bytes",
            plan.grep.pattern(),
            bytes.len()
        );
    }
}

#[test]
fn run_search_and_per_line_search_emit_identical_records_and_results() {
    let documents: &[&[u8]] = &[
        // Pins the bounded-context skip path at a distant match.
        b"x\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nx\nred\n",
        b"red\ngap\nred\n",
        b"red\ngap\nred",
        b"gap\nred\n",
        b"gap\nred",
        b"red red\r\ngap\r\nred\r\n",
        b"\nred\n\n",
        b"\xef\xbb\xbfred\ngap\nred\n",
        b"red\0red\n",
        b"gap\0red\n",
        b"red\n\0red\n",
        b"\0red\n",
        b"\n\0",
        b"sing\nwalking\nred\nsinging\n",
        b"a b z\nb\nred gap red\n",
        b"\x90\x80\x81\xf0\x90\x80\x81X\n",
        "singing界no\nwalking\nx singing𐀀 end\nsinging🙂\n".as_bytes(),
        b"gap\n",
        b"gap",
        b"",
        b"\n",
        b"\n\n",
        b"red\nred\nred\n",
        b"\xffred\n\xff\nred\xff\n",
        "Kilo\nred\nſea\n".as_bytes(),
        // Gaps longer and shorter than the context windows.
        b"a\nb\nc\nd\nred\ne\nf\ng\nh\ni\nred\nj\nred\nk\nl\nm\nn\n",
        b"a\nb\nc\nd\ne\nf\n\n\nred\n\n\n\n\ng",
        b"a\nb\nc\nd\ne\nf\ng\n",
        b"a\nb\nc\nd\ne\nf\n\0g\nred\n",
    ];
    let patterns = [
        "red",
        "absent",
        "",
        "^",
        "$",
        "^$",
        "^red$",
        r"\bred\b",
        r"\b",
        r"\B",
        r"\b{start-half}",
        r"\b{end-half}",
        r"\w+ing\b",
        "red|$",
        "red.*red|gap",
        "a.*z|b",
        r"a*(?-u:\x90\x80\x81)$",
        r"(?-u:[\x00-\x09\x0B-\xEF]*\x90\x80\x81)$",
        r"[a-z]+ing\b",
        "red?",
        "red*",
        "(?i:red)",
        "[a-z]+red",
        "[a-z]*red",
        // These must retain per-line assertion/consumption bounds (D79).
        r"\Ared",
        r"red\z",
        r"\A|\z",
        "(?-m:^red$)",
        "(?R:^red$)",
        r"\s",
        r"[^x]",
        "(?s:.)",
        r"[\s\S]",
        r"(?-u:\xFF)",
    ];
    let profiles: &[&[&str]] = &[
        &[],
        &["-n"],
        &["-m1"],
        &["-m2"],
        &["-m0"],
        &["-c"],
        &["-co"],
        &["--count-matches"],
        &["-l"],
        &["-q"],
        &["-o"],
        &["-om1"],
        &["-cm1"],
        &["--json"],
        &["--json", "-m1"],
        &["--json", "-q"],
        &["--json", "-N"],
        &["-r", "<$0>"],
        &["-or", "<$0>"],
        &["--files-without-match"],
        &["-v"],
        &["-vn"],
        &["-vc"],
        &["-vl"],
        &["-vq"],
        &["-vm2"],
        &["-v", "--files-without-match"],
        &["-v", "--json"],
        &["-v", "-C1"],
        &["-v", "-A1", "-B2"],
        &["-v", "-C1", "-m1"],
        &["-v", "-r", "<$0>"],
        // Unchanged fallbacks share the same switch and public entry point.
        &["-C1"],
        &["-B2"],
        &["-A2"],
        &["-C3"],
        &["-A1", "-B3"],
        &["-B2", "-m1"],
        &["-C2", "-m2"],
        &["-C1", "-o"],
        &["-c", "-C2"],
        &["-B2", "-r", "<$0>"],
        &["--json", "-A1"],
        &["--json", "-B2"],
        &["-U"],
    ];
    for pattern in patterns {
        for &profile in profiles {
            for text in [false, true] {
                let mut args: Vec<_> = profile.iter().map(OsString::from).collect();
                if text {
                    args.push("-a".into());
                }
                args.push(pattern.into());
                let Command::Search(plan) = parse(&args).unwrap() else {
                    panic!("search")
                };
                for &bytes in documents {
                    compare_scanners(&plan, bytes);
                }
            }
        }
    }
    // Context windows that straddle 64 KiB read runs, around lines that
    // are long, short and empty.
    let mut runs = Vec::new();
    for line in 0..12_000 {
        match line % 4_001 {
            0 | 4_000 => runs.extend_from_slice(b"red\n"),
            7 => runs.extend(std::iter::repeat_n(b'x', 70_000)),
            _ if line % 3 == 0 => {}
            _ => runs.extend_from_slice(format!("gap {line}").as_bytes()),
        }
        runs.push(b'\n');
    }
    for profile in [
        &["-C3"][..],
        &["-B5"],
        &["-A2"],
        &["-B2", "-m2"],
        &["-c", "-C1"],
        // Inversion keeps the next hit per run; one carried into the next
        // run would select or hide the wrong lines.
        &["-v"],
        &["-v", "-C1"],
    ] {
        let mut args: Vec<_> = profile.iter().map(OsString::from).collect();
        args.push("red".into());
        let Command::Search(plan) = parse(&args).unwrap() else {
            panic!("search")
        };
        compare_scanners(&plan, &runs);
        compare_scanners(&plan, &runs[..runs.len() - 1]);
    }
    for flags in [
        vec!["-F"],
        vec!["-Fi"],
        vec!["-w"],
        vec!["-x"],
        vec!["-e", "", "-e"],
    ] {
        let mut args: Vec<_> = flags.into_iter().map(OsString::from).collect();
        args.push("red".into());
        let Command::Search(plan) = parse(&args).unwrap() else {
            panic!("search")
        };
        for &bytes in documents {
            compare_scanners(&plan, bytes);
        }
    }
}

#[test]
fn before_context_boundary_work_is_linear() {
    let count = 256;
    let mut bytes = b"x\n".repeat(count);
    bytes.extend_from_slice(b"red\n");
    let args = ["-n", "-B256", "red"];
    let Command::Search(plan) = parse(&args.map(OsString::from)).unwrap() else {
        panic!("search")
    };
    compare_scanners(&plan, &bytes);
    context::reset_tail_start_scanned();
    scan_lines(
        &plan,
        &mut &bytes[..],
        bytes.len() as u64,
        &mut Vec::new(),
        &|| false,
        |_| Ok(()),
    )
    .unwrap();
    assert!(
        context::tail_start_scanned() <= bytes.len() * 2,
        "tail_start scanned {} bytes for {} input bytes",
        context::tail_start_scanned(),
        bytes.len()
    );
}

#[test]
fn run_search_preserves_partial_lines_across_reads_and_run_offsets() {
    let mut bytes = b"red\ngap\n".to_vec();
    bytes.extend(std::iter::repeat_n(b'x', crate::scan::BUFFER * 2 + 17));
    bytes.extend_from_slice(b"red\n\ngap\nred\n");
    // The first read contains a complete run followed by a partial line;
    // that line survives multiple reads before the next callback.
    for prefix in [b"".as_slice(), b"\xef\xbb\xbf"] {
        let mut input = prefix.to_vec();
        input.extend_from_slice(&bytes);
        for suffix in [b"".as_slice(), b"red", b"\0red\n"] {
            let mut input = input.clone();
            input.extend_from_slice(suffix);
            for flags in ["-n", "-o", "-c", "-l", "-q", "-m1", "--json"] {
                for text in [false, true] {
                    let mut args = vec![flags.into()];
                    if text {
                        args.push("-a".into());
                    }
                    args.push("red".into());
                    let Command::Search(plan) = parse(&args).unwrap() else {
                        panic!("search")
                    };
                    compare_scanners(&plan, &input);
                }
            }
        }
    }
}

// Globs and types share one ordered list, so `-g` and `-t` interleave
// as written, in every spelling.
#[test]
fn globs_and_types_keep_argv_order() {
    let args: Vec<OsString> = [
        "-tpy",
        "-g",
        "*.rs",
        "--type",
        "rust",
        "-T",
        "md",
        "--type-not=c",
        "--type=sh",
        "-g!x",
        "red",
    ]
    .iter()
    .map(Into::into)
    .collect();
    let Command::Search(plan) = parse(&args).unwrap() else {
        panic!("search")
    };
    let ty = |name: &str, include| Filter::Type {
        name: name.into(),
        include,
    };
    assert_eq!(
        plan.filters,
        [
            ty("py", true),
            Filter::Glob("*.rs".into()),
            ty("rust", true),
            ty("md", false),
            ty("c", false),
            ty("sh", true),
            Filter::Glob("!x".into()),
        ]
    );
}

#[test]
fn offset_presentation_flags_remain_refused() {
    // These are outside the current grep contract, including this slice.
    for flag in ["--column", "-b"] {
        assert!(matches!(parse(&[flag.into(), "red".into()]),
            Err(ParseError::Unsupported(value)) if value == flag));
    }
}

#[test]
fn an_unknown_colour_choice_is_an_error_not_a_missing_feature() {
    let error = parse(&["--color=bogus".into(), "red".into()]).unwrap_err();
    assert!(matches!(&error, ParseError::Choice(flag, value)
        if flag == "--color" && value == "bogus"));
    assert_eq!(
        error.to_string(),
        "error parsing flag --color: choice 'bogus' is unrecognized"
    );
}

#[test]
fn only_a_names_listing_sorts_by_modification_time() {
    let names = |args: &[&str]| {
        let args: Vec<OsString> = args.iter().map(Into::into).collect();
        match parse(&args) {
            Ok(Command::Files(names)) => Ok(names.sort),
            Ok(_) => panic!("{args:?} is not a listing"),
            Err(error) => Err(error.to_string()),
        }
    };
    assert_eq!(
        names(&["--files", "--sort=modified"]),
        Ok(Some(Sort::Oldest))
    );
    assert_eq!(
        names(&["--files", "--sortr", "modified"]),
        Ok(Some(Sort::Newest))
    );
    // One setting, the last given; `none` clears it.
    assert_eq!(
        names(&["--files", "--sortr=modified", "--sort=modified"]),
        Ok(Some(Sort::Oldest))
    );
    assert_eq!(
        names(&["--files", "--sort=modified", "--sort=none"]),
        Ok(None)
    );
    let refused = |args: &[&str]| {
        let args: Vec<OsString> = args.iter().map(Into::into).collect();
        parse(&args).unwrap_err()
    };
    for args in [
        &["--files", "--sort=path"][..],
        &["--files", "--sort=modified", "--sortr=created"],
        &["--sort=modified", "red"],
    ] {
        assert!(
            matches!(refused(args), ParseError::Unsupported(_)),
            "{args:?}"
        );
    }
    assert!(matches!(
        refused(&["--files", "--sort=size"]),
        ParseError::Choice(..)
    ));
}

fn admits(query: &Trigrams, original: &[u8]) -> bool {
    match query {
        Trigrams::All => true,
        Trigrams::None => false,
        Trigrams::Key(key) => original
            .windows(3)
            .any(|w| w.iter().map(u8::to_ascii_lowercase).eq(key.iter().copied())),
        Trigrams::And(parts) => parts.iter().all(|p| admits(p, original)),
        Trigrams::Or(parts) => parts.iter().any(|p| admits(p, original)),
    }
}

#[test]
fn replacement_cancellation_after_one_record_stops_expansion() {
    use std::cell::Cell;
    for flags in ["-or", "-Uor"] {
        let Command::Search(plan) = parse(&[flags.into(), "<$0>".into(), "red".into()]).unwrap()
        else {
            panic!("search")
        };
        let stopped = Cell::new(false);
        let mut emitted = 0;
        let bytes = b"red red\n";
        let result = scan_lines(
            &plan,
            &mut &bytes[..],
            bytes.len() as u64,
            &mut Vec::new(),
            &|| stopped.get(),
            |_| {
                emitted += 1;
                stopped.set(true);
                Ok(())
            },
        );
        assert!(result.is_err(), "{flags}: cancelled expansion succeeded");
        assert_eq!(emitted, 1);
    }
}

#[test]
fn multiline_replacement_splitting_stops_after_the_first_emitted_line() {
    use std::cell::Cell;
    let Command::Search(plan) =
        parse(&["-Ur".into(), "$2\n$1\nlast".into(), r"(red)\n(blue)".into()]).unwrap()
    else {
        panic!("search")
    };
    assert!(
        !plan.grep.excludes_lf(),
        "must exercise multiline splitting"
    );
    let bytes = b"red\nblue\nred\nblue\n";
    let expected = vec![
        (1, b"blue".to_vec()),
        (2, b"red".to_vec()),
        (3, b"last".to_vec()),
        (4, b"blue".to_vec()),
        (5, b"red".to_vec()),
        (6, b"last".to_vec()),
    ];
    for cancel in [false, true] {
        let stopped = Cell::new(false);
        let mut records = Vec::new();
        let result = scan_lines(
            &plan,
            &mut &bytes[..],
            bytes.len() as u64,
            &mut Vec::new(),
            &|| stopped.get(),
            |line| {
                records.push((line.number, line.bytes.to_vec()));
                if cancel {
                    stopped.set(true);
                }
                Ok(())
            },
        );
        if cancel {
            assert!(matches!(result, Err(ScanError::Output(ref error))
                if error.kind() == io::ErrorKind::ConnectionAborted));
            assert_eq!(records, expected[..1]);
        } else {
            assert!(result.is_ok());
            assert_eq!(records, expected);
        }
    }
}

#[test]
fn resolved_names_only_never_reads_pattern_files_or_stdin() {
    for args in [
        vec!["--files", "-f", "missing"],
        vec!["--files", "-f", "-"],
        vec!["--help", "-f", "missing"],
    ] {
        let args: Vec<_> = args.into_iter().map(OsString::from).collect();
        parse_with(&args, &mut |_| panic!("names-only attempted pattern I/O")).unwrap();
    }
}

// M2: compile, matching and necessary keys see the same resolved profile.
#[test]
fn fixed_folded_and_repeated_profiles_are_sound_over_original_bytes() {
    let profiles: &[(&[&str], &[u8])] = &[
        (&["--json", "-i", "KILO"], "Kilo\n".as_bytes()),
        (&["--json", "-U", "red\\nviolet"], b"red\nviolet\n"),
        (&["-r", "<$1>", "(red)"], b"red\nviolet\n"),
        (
            &["-Uor", "<$1>", "(?-u:(\\xFF))red\\nviolet"],
            b"\xffred\nviolet\n",
        ),
        (&["-r", "<$name>", "(?P<name>red)"], b"red.*\n"),
        (&["-U", "red\\nviolet"], b"red\nviolet\n"),
        (&["-U", "(?s:red.*violet)"], b"red\nviolet\n"),
        (&["-UF", "red\nviolet"], b"red\nviolet\n"),
        (&["-U", "red|\\A\\z\\n"], b"red.*\n"),
        (
            &["-U", "-e", "red", "-e", "violet\\nblue"],
            b"red\nviolet\n",
        ),
        (&["-Uiw", "KILO"], "Kilo\n".as_bytes()),
        (&["-Ux", "red\\nviolet"], b"red\nviolet\n"),
        (
            &["-U", "--multiline-dotall", "red.*violet"],
            b"red\nviolet\n",
        ),
        (&["-U", "(?-u:\\xFF)red\\nviolet"], b"\xffred\nviolet\n"),
        (&["-i", "KILO"], "Kilo\n".as_bytes()),
        (&["-Fi", "SEA"], "ſea\n".as_bytes()),
        (&["-F", "red.*"], b"red.*\n"),
        (&["-w", "red"], b"red\nviolet\n"),
        (&["-x", "red"], b"\xef\xbb\xbfred\n"),
        (&["-Sw", "red"], b"red\nviolet\n"),
        (&["-SFx", "red.*"], b"red.*\n"),
        (&["-S", "-e", "red", "-e", "VIOLET"], b"VIOLET\n"),
        (&["-wo", "@red@"], b"@red@\n"),
        (&["-wo", ""], b"\n"),
        (&["-e", "red", "-e", "(?i:violet)"], b"VIOLET\n"),
        (&["-o", "-e", "", "-e", "red"], b"\n"),
    ];
    let documents: &[&[u8]] = &[
        b"red.*\n",
        b"red\nviolet\n",
        b"\xef\xbb\xbfred\nviolet\n",
        b"\xffred\nviolet\n",
        b"red\r\nviolet\r\n",
        b"\xef\xbb\xbfred\n",
        b"VIOLET\n",
        b"@red@\n",
        b"x@red@x\n",
        b"RED\n",
        b"bred\n",
        "Kilo\n".as_bytes(),
        "ſea\n".as_bytes(),
        b"\xffred\n",
        b"\n",
        b"",
    ];
    let mut positives = 0;
    for &(args, expected_positive) in profiles {
        let mut checked_positive = false;
        let Command::Search(plan) =
            parse(&args.iter().map(OsString::from).collect::<Vec<_>>()).unwrap()
        else {
            panic!("search")
        };
        for &bytes in documents {
            let result = scan(
                &plan,
                &mut &bytes[..],
                bytes.len() as u64,
                &mut Vec::new(),
                &|| false,
                |_, _| Ok(()),
            )
            .unwrap();
            if bytes == expected_positive {
                checked_positive = true;
                assert!(
                    result.matched,
                    "missing required positive for {args:?}: {bytes:?}"
                );
            }
            if result.matched {
                positives += 1;
                assert!(admits(plan.grep.trigrams(), bytes));
            }
        }
        assert!(checked_positive, "missing positive fixture for {args:?}");
    }
    assert!(positives >= 12);
}

#[test]
fn output_universe_follows_resolved_inversion_and_count_overrides() {
    for (args, universe, output) in [
        (vec!["-c", "red"], Universe::Candidates, Output::Count),
        (
            vec!["-c", "--include-zero", "red"],
            Universe::Searchable,
            Output::Count,
        ),
        (
            vec!["--files-without-match", "red"],
            Universe::Searchable,
            Output::WithoutMatch,
        ),
        (
            vec!["-q", "--files-without-match", "red"],
            Universe::Searchable,
            Output::WithoutMatch,
        ),
        (
            vec!["-qc", "--include-zero", "red"],
            Universe::Candidates,
            Output::Count,
        ),
        (vec!["-vc", "red"], Universe::Searchable, Output::Count),
        (vec!["-cl", "red"], Universe::Candidates, Output::Names),
        (
            vec!["-v", "--no-invert-match", "red"],
            Universe::Candidates,
            Output::Lines,
        ),
    ] {
        let Command::Search(plan) =
            parse(&args.into_iter().map(OsString::from).collect::<Vec<_>>()).unwrap()
        else {
            panic!("search")
        };
        assert_eq!(plan.universe(), universe);
        assert_eq!(plan.output, output);
    }
}

#[test]
fn executed_line_profile_is_sound_over_original_bom_and_invalid_bytes() {
    let patterns = [
        "red",
        "(?i)red",
        "(?i:KELVIN)",
        r"\Ared\z",
        "^red$",
        "red|violet",
        "re[d-f]",
        "re.*",
        "",
        r"\s",
        r"[\s\S]",
        r"(?-u:\xFF)red",
        r"[a-z&&[^aeiou]]+",
        "(?s:.)",
        "(?i:[a-z_]+)red",
    ];
    let documents: &[&[u8]] = &[
        b"\xef\xbb\xbfred\n",
        b"blue\nred\n",
        b"\xffred\n",
        "Kelvin\n".as_bytes(),
        b"red\r\n",
        b"",
        b"\n",
        b"red",
        b"violet\n",
    ];
    let mut positives = 0;
    for pattern in patterns {
        let Command::Search(plan) = parse(&[pattern.into()]).unwrap() else {
            panic!("search")
        };
        for &original in documents {
            let mut selected = Vec::new();
            let result = scan(
                &plan,
                &mut &original[..],
                original.len() as u64,
                &mut Vec::new(),
                &|| false,
                |number, line| {
                    selected.push((number, line.to_vec()));
                    Ok(())
                },
            )
            .unwrap();
            if result.matched {
                positives += 1;
                assert!(
                    admits(plan.grep.trigrams(), original),
                    "{pattern:?}, {original:?}: {:?}",
                    plan.grep.trigrams()
                );
            }
            assert_eq!(result.matched, !selected.is_empty());
        }
    }
    assert!(positives > 40);
}

#[test]
fn long_lines_tiny_reads_and_cancelled_reads_use_the_shared_stream() {
    struct Tiny<'a>(&'a [u8]);
    impl Read for Tiny<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.0.read(&mut bytes[..1])
        }
    }
    let Command::Search(plan) = parse(&["-n".into(), "red".into()]).unwrap() else {
        panic!("search")
    };
    let mut bytes = b"\xef\xbb\xbf".to_vec();
    bytes.extend(std::iter::repeat_n(b'x', 100_000));
    bytes.extend_from_slice(b"red\nred");
    let mut lines = Vec::new();
    scan(
        &plan,
        &mut Tiny(&bytes),
        u64::MAX,
        &mut Vec::new(),
        &|| false,
        |n, line| {
            lines.push((n, line.len()));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(lines, [(1, 100_003), (2, 3)]);
    let result = scan(
        &plan,
        &mut &b"red\n"[..],
        4,
        &mut Vec::new(),
        &|| true,
        |_, _| panic!("cancelled"),
    );
    assert!(result.is_err());
}
// R1/P2 and M4 R1: an already-read NUL survives the selected-line limit,
// including a NUL after a complete line; no mode requests another read.
#[test]
fn binary_sentinel_stops_before_another_read_or_buffer_growth() {
    struct Source<'a> {
        bytes: &'a [u8],
        reads: usize,
    }
    impl Read for Source<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            if self.reads != 1 {
                return Err(io::Error::other("read past NUL"));
            }
            bytes[..self.bytes.len()].copy_from_slice(self.bytes);
            Ok(self.bytes.len())
        }
    }
    for bytes in [b"red\0x".as_slice(), b"red\nred\0x"] {
        for flags in [
            vec![],
            vec!["-m1"],
            vec!["-c"],
            vec!["-m1", "-c"],
            vec!["--count-matches"],
            vec!["-m1", "--count-matches"],
            vec!["-co"],
            vec!["-m1", "-co"],
        ] {
            let mut args: Vec<_> = flags.iter().map(OsString::from).collect();
            args.push("red".into());
            let Command::Search(plan) = parse(&args).unwrap() else {
                panic!("search")
            };
            let mut source = Source { bytes, reads: 0 };
            let mut buffer = Vec::new();
            let result = scan(
                &plan,
                &mut source,
                u64::MAX,
                &mut buffer,
                &|| false,
                |_, _| Ok(()),
            )
            .unwrap();
            assert!(
                result.matched && result.binary,
                "{flags:?}: {bytes:?}: {result:?}"
            );
            let expected = if flags.contains(&"-m1") {
                1
            } else {
                bytes.iter().filter(|&&b| b == b'\n').count() as u64 + 1
            };
            assert_eq!(result.count, expected, "{flags:?}: {bytes:?}");
            assert_eq!(source.reads, 1);
            assert_eq!(buffer.len(), crate::scan::BUFFER);
        }
    }
}

#[test]
fn count_lf_counts_across_its_byte_sized_chunks() {
    // A run of 255 64-byte blocks of LFs fills every lane exactly; one more
    // byte crosses into the next run. Lengths off a block boundary leave a
    // bytewise tail.
    for n in [
        0,
        1,
        63,
        64,
        65,
        1000,
        255 * 64 - 1,
        255 * 64,
        255 * 64 + 1,
        3 * 255 * 64 + 7,
    ] {
        assert_eq!(super::count_lf(&vec![b'\n'; n]), n as u64);
    }
    let mixed: Vec<u8> = (0..1000)
        .map(|i| if i % 3 == 0 { b'\n' } else { b'x' })
        .collect();
    assert_eq!(super::count_lf(&mixed), 334);
}
