use super::*;

// ── table tests ──

fn ok(line: &[u8]) -> Request {
    parse_request(line).unwrap_or_else(|e| panic!("expected Ok, got {e:?} for {line:?}"))
}

fn err_kind(line: &[u8]) -> RequestErrorKind {
    parse_request(line).unwrap_err().kind
}

#[test]
fn minimal_request_parses() {
    let r = ok(br#"{"id":"a","op":"search","args":[]}"#);
    assert_eq!(
        r,
        Request {
            id: "a".into(),
            op: Op::Search,
            grep: None,
            args: vec![],
            cwd: None,
            limit: None,
            capabilities: Vec::new(),
            child_stdin: None,
            start_unix_ns: None,
        }
    );
}

#[test]
fn capabilities_and_child_stdin_are_typed_and_unknown_options_remain_ignored() {
    let request = ok(br#"{"id":"a","op":"find","args":[],"capabilities":["local-effects","interactive","future"],"child_stdin":"inherit","future":{"nested":[true]}}"#);
    assert_eq!(
        request.capabilities,
        ["local-effects", "interactive", "future"]
    );
    assert_eq!(request.child_stdin, Some(ChildStdin::Inherit));
    assert_eq!(
        ok(br#"{"id":"a","op":"find","args":[],"child_stdin":"null"}"#).child_stdin,
        Some(ChildStdin::Null)
    );
    for (extra, field) in [
        (r#""capabilities":"local-effects""#, "capabilities"),
        (r#""capabilities":[true]"#, "capabilities"),
        (r#""child_stdin":"pipe""#, "child_stdin"),
        (r#""child_stdin":false"#, "child_stdin"),
    ] {
        let line = format!(r#"{{"id":"a","op":"find","args":[],{extra}}}"#);
        assert_eq!(
            err_kind(line.as_bytes()),
            RequestErrorKind::InvalidField(field)
        );
    }
}

#[test]
fn every_escape_decodes() {
    let line = br#"{"id":"a","op":"search","args":["\"\\\/\b\f\n\r\t\u0041"]}"#;
    let r = ok(line);
    assert_eq!(r.args, vec![b"\"\\/\x08\x0c\n\r\tA".to_vec()]);
}

#[test]
fn surrogate_pair_combines_to_one_codepoint() {
    // U+1F600 GRINNING FACE, as a UTF-16 surrogate pair.
    let line = br#"{"id":"a","op":"search","args":["\ud83d\ude00"]}"#;
    let r = ok(line);
    assert_eq!(r.args, vec!["\u{1F600}".as_bytes().to_vec()]);
}

#[test]
fn lone_high_surrogate_is_rejected_not_replaced_with_u_fffd() {
    // The discriminating case: a plausible-but-wrong reader accepts this
    // and emits U+FFFD. Ours must reject it outright.
    let line = br#"{"id":"a","op":"search","args":["\ud800"]}"#;
    assert_eq!(err_kind(line), RequestErrorKind::InvalidJson);
}

#[test]
fn lone_low_surrogate_is_rejected() {
    let line = br#"{"id":"a","op":"search","args":["\udc00"]}"#;
    assert_eq!(err_kind(line), RequestErrorKind::InvalidJson);
}

#[test]
fn invalid_utf8_inside_a_string_is_rejected() {
    let mut line = br#"{"id":"a","op":"search","args":["#.to_vec();
    line.push(b'"');
    line.push(0xFF);
    line.extend_from_slice(br#""]}"#);
    assert_eq!(err_kind(&line), RequestErrorKind::InvalidJson);
}

#[test]
fn base64_with_bad_padding_is_rejected() {
    let line = br#"{"id":"a","op":"search","args":[{"base64":"A==="}]}"#;
    assert_eq!(err_kind(line), RequestErrorKind::InvalidField("args"));
}

#[test]
fn base64_decodes_exact_bytes() {
    let line = br#"{"id":"a","op":"search","args":[{"base64":"/w=="}]}"#;
    let r = ok(line);
    assert_eq!(r.args, vec![vec![0xffu8]]);
}

#[test]
fn duplicate_key_is_rejected() {
    let line = br#"{"id":"a","id":"b","op":"search","args":[]}"#;
    assert_eq!(err_kind(line), RequestErrorKind::DuplicateKey);
}

#[test]
fn unknown_op_is_rejected() {
    let line = br#"{"id":"a","op":"frobnicate","args":[]}"#;
    let e = parse_request(line).unwrap_err();
    assert_eq!(e.id, Some("a".into()));
    assert_eq!(e.kind, RequestErrorKind::UnknownOp);
}

#[test]
fn control_requests_need_no_args_but_queries_do() {
    let status = parse_request(br#"{"id":"s","op":"status"}"#).unwrap();
    assert!(status.args.is_empty());
    let e = parse_request(br#"{"id":"q","op":"search"}"#).unwrap_err();
    assert_eq!(e.kind, RequestErrorKind::MissingField("args"));
}

#[test]
fn missing_id_has_no_tagged_id() {
    let line = br#"{"op":"search","args":[]}"#;
    let e = parse_request(line).unwrap_err();
    assert_eq!(e.id, None);
    assert_eq!(e.kind, RequestErrorKind::MissingField("id"));
}

#[test]
fn id_is_recovered_when_a_later_field_is_bad() {
    let line = br#"{"id":"keep-me","op":"search","args":"not-an-array"}"#;
    let e = parse_request(line).unwrap_err();
    assert_eq!(e.id, Some("keep-me".into()));
    assert_eq!(e.kind, RequestErrorKind::InvalidField("args"));
}

#[test]
fn line_exactly_at_the_size_limit_parses() {
    // Pad with a long id so the whole line is exactly MAX_LINE_BYTES.
    let prefix = br#"{"id":""#;
    let suffix = br#"","op":"search","args":[]}"#;
    let pad_len = MAX_LINE_BYTES - prefix.len() - suffix.len();
    let mut line = prefix.to_vec();
    line.extend(std::iter::repeat_n(b'a', pad_len));
    line.extend_from_slice(suffix);
    assert_eq!(line.len(), MAX_LINE_BYTES);
    ok(&line);
}

#[test]
fn line_one_byte_over_the_size_limit_is_rejected() {
    let prefix = br#"{"id":""#;
    let suffix = br#"","op":"search","args":[]}"#;
    let pad_len = MAX_LINE_BYTES - prefix.len() - suffix.len() + 1;
    let mut line = prefix.to_vec();
    line.extend(std::iter::repeat_n(b'a', pad_len));
    line.extend_from_slice(suffix);
    assert_eq!(line.len(), MAX_LINE_BYTES + 1);
    assert_eq!(err_kind(&line), RequestErrorKind::LineTooLong);
}

#[test]
fn argv_at_the_bound_parses() {
    let args = "\"x\",".repeat(MAX_ARGV_ELEMENTS - 1) + "\"x\"";
    let line = format!(r#"{{"id":"a","op":"search","args":[{args}]}}"#);
    let r = ok(line.as_bytes());
    assert_eq!(r.args.len(), MAX_ARGV_ELEMENTS);
}

#[test]
fn argv_one_over_the_bound_is_rejected() {
    let args = "\"x\",".repeat(MAX_ARGV_ELEMENTS) + "\"x\"";
    let line = format!(r#"{{"id":"a","op":"search","args":[{args}]}}"#);
    assert_eq!(
        err_kind(line.as_bytes()),
        RequestErrorKind::InvalidField("args")
    );
}

/// Builds `{"id":"a","op":"search","args":[],"extra":<nested arrays
/// `depth` deep>}`. `extra` is unknown and ignored, but its value must
/// still parse as valid, depth-bounded JSON — exercising the bound
/// independently of the schema's own shape.
fn nested_extra(depth: u32) -> Vec<u8> {
    let mut extra = String::from("0");
    // `depth` here counts the levels of array nesting on top of the
    // request object itself (depth 1), matching parse_value's depth
    // argument: the object is depth 1, so `extra`'s outermost array is
    // depth 2, and each further nesting level adds one.
    for _ in 0..depth {
        extra = format!("[{extra}]");
    }
    format!(r#"{{"id":"a","op":"search","args":[],"extra":{extra}}}"#).into_bytes()
}

#[test]
fn nesting_at_the_bound_parses() {
    ok(&nested_extra(MAX_NESTING_DEPTH - 1));
}

#[test]
fn nesting_one_level_deeper_is_rejected() {
    assert_eq!(
        err_kind(&nested_extra(MAX_NESTING_DEPTH)),
        RequestErrorKind::NestingTooDeep
    );
}

#[test]
fn trailing_garbage_after_the_object_is_rejected() {
    let line = br#"{"id":"a","op":"search","args":[]} garbage"#;
    assert_eq!(err_kind(line), RequestErrorKind::InvalidJson);
}

#[test]
fn leading_byte_order_mark_is_rejected() {
    let mut line = vec![0xEF, 0xBB, 0xBF];
    line.extend_from_slice(br#"{"id":"a","op":"search","args":[]}"#);
    assert_eq!(err_kind(&line), RequestErrorKind::InvalidJson);
}

#[test]
fn whitespace_everywhere_legal_is_accepted() {
    let line =
        b"  \t\n{ \"id\" : \"a\" ,\n\"op\"\t:\"search\",\"args\"  :[  \"x\" ,  \"y\"  ]  }  \n";
    let r = ok(line);
    assert_eq!(r.args, vec![b"x".to_vec(), b"y".to_vec()]);
}

#[test]
fn cwd_and_limit_round_trip_as_options() {
    let r = ok(br#"{"id":"a","op":"find","args":[],"cwd":"/work","limit":20}"#);
    assert_eq!(r.cwd, Some(b"/work".to_vec()));
    assert_eq!(r.limit, Some(20));
}

// The server spills context into the client's directory, so it must not
// resolve a missing or relative one against its own environment or cwd.
#[test]
fn grep_temp_dir_is_required_and_absolute() {
    let grep = |temp: &str| {
        format!(
            r#"{{"id":"g","op":"grep","args":[],"grep":{{"color":false,"heading":false,"numbers":false,"tty":false,"threads":1,"pattern_files":[]{temp}}}}}"#
        )
    };
    let r = ok(grep(r#","temp_dir":"/client/tmp""#).as_bytes());
    assert_eq!(r.grep.unwrap().temp_dir, b"/client/tmp".to_vec());
    for temp in ["", r#","temp_dir":"tmp""#, r#","temp_dir":"""#] {
        assert_eq!(
            err_kind(grep(temp).as_bytes()),
            RequestErrorKind::InvalidField("grep")
        );
    }
}

#[test]
fn negative_limit_is_rejected() {
    let line = br#"{"id":"a","op":"search","args":[],"limit":-1}"#;
    assert_eq!(err_kind(line), RequestErrorKind::InvalidField("limit"));
}

#[test]
fn fractional_limit_is_rejected() {
    let line = br#"{"id":"a","op":"search","args":[],"limit":1.5}"#;
    assert_eq!(err_kind(line), RequestErrorKind::InvalidField("limit"));
}

// ── round-trip property, driving the real json.rs writer ──

/// Seeded xorshift64*, matching the idiom in
/// `crates/ferret-verify/src/tests.rs`.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn bool(&mut self) -> bool {
        self.next() & 1 == 1
    }

    /// Random bytes, including sequences that are not valid UTF-8.
    fn bytes(&mut self, max_len: usize) -> Vec<u8> {
        let len = self.below(max_len + 1);
        (0..len).map(|_| (self.next() & 0xff) as u8).collect()
    }

    fn id(&mut self) -> String {
        let len = 1 + self.below(12);
        (0..len)
            .map(|_| (b'a' + (self.below(26) as u8)) as char)
            .collect()
    }

    fn op(&mut self) -> Op {
        match self.below(5) {
            0 => Op::Search,
            1 => Op::Find,
            2 => Op::Status,
            3 => Op::Grep,
            _ => Op::Reload,
        }
    }

    fn request(&mut self) -> Request {
        let args_len = self.below(6);
        let args = (0..args_len).map(|_| self.bytes(24)).collect();
        Request {
            id: self.id(),
            op: self.op(),
            grep: self.bool().then(|| GrepOptions {
                color: self.bool(),
                heading: self.bool(),
                numbers: self.bool(),
                tty: self.bool(),
                threads: self.below(32) + 1,
                pattern_files: vec![self.bytes(24)],
                temp_dir: [b"/".as_slice(), &self.bytes(23)].concat(),
            }),
            args,
            cwd: self.bool().then(|| self.bytes(24)),
            limit: self.bool().then(|| self.next() % 1_000_000),
            capabilities: if self.bool() {
                vec!["local-effects".into(), "interactive".into()]
            } else {
                vec![]
            },
            start_unix_ns: self.bool().then(|| self.next()),
            child_stdin: self.bool().then(|| {
                if self.bool() {
                    ChildStdin::Null
                } else {
                    ChildStdin::Inherit
                }
            }),
        }
    }
}

/// Encodes a [`Request`] with the real production writer
/// ([`json::Object`]), not a copy of the parser's logic — this is the
/// writer side of the same schema the parser reads.
fn encode(req: &Request) -> Vec<u8> {
    let mut out = Vec::new();
    let mut obj = crate::json::Object::new(&mut out);
    obj.str("id", &req.id);
    obj.str(
        "op",
        match req.op {
            Op::Search => "search",
            Op::Find => "find",
            Op::Grep => "grep",
            Op::Status => "status",
            Op::Reload => "reload",
            Op::Index => "index",
            Op::RootsRemove => "roots-remove",
        },
    );
    if let Some(grep) = &req.grep {
        grep.encode(&mut obj);
    }
    obj.byte_strings("args", req.args.iter().map(|a| a.as_slice()));
    obj.opt_byte_value("cwd", req.cwd.as_deref());
    obj.opt_int("limit", req.limit.map(|l| l as i128));
    obj.opt_int("start_unix_ns", req.start_unix_ns);
    obj.byte_strings(
        "capabilities",
        req.capabilities.iter().map(String::as_bytes),
    );
    if let Some(policy) = req.child_stdin {
        obj.str(
            "child_stdin",
            match policy {
                ChildStdin::Null => "null",
                ChildStdin::Inherit => "inherit",
            },
        );
    }
    obj.end();
    out
}

#[test]
fn round_trip_through_the_real_writer() {
    let mut rng = Rng(0xfeed_face_dead_beef);
    for _ in 0..4000 {
        let req = rng.request();
        let encoded = encode(&req);
        let parsed = parse_request(&encoded)
            .unwrap_or_else(|e| panic!("{e:?} on {:?}", String::from_utf8_lossy(&encoded)));
        assert_eq!(parsed, req, "line: {:?}", String::from_utf8_lossy(&encoded));
    }
}

// ── mutation fuzz ──

/// Flips, inserts, deletes or truncates bytes in `line` and returns the
/// result. May produce anything, including invalid UTF-8 and empty
/// input.
fn mutate(rng: &mut Rng, line: &[u8]) -> Vec<u8> {
    let mut out = line.to_vec();
    if out.is_empty() {
        out.push(rng.next() as u8);
        return out;
    }
    match rng.below(4) {
        0 => {
            let i = rng.below(out.len());
            out[i] = (rng.next() & 0xff) as u8;
        }
        1 => {
            let i = rng.below(out.len() + 1);
            out.insert(i, (rng.next() & 0xff) as u8);
        }
        2 => {
            let i = rng.below(out.len());
            out.remove(i);
        }
        _ => {
            let i = rng.below(out.len() + 1);
            out.truncate(i);
        }
    }
    out
}

fn mutation_fuzz(iterations: u64) {
    let mut rng = Rng(0x0dba_11ca_fef0_0d42);
    for i in 0..iterations {
        let mut req_rng = Rng(0x5eed ^ i);
        let base = encode(&req_rng.request());
        let mutated = mutate(&mut rng, &base);
        // Rejection is always fine; only a panic is a bug. An accepted
        // mutation must re-encode to a request that parses equal: no
        // mutation may be accepted into something the writer itself
        // couldn't produce.
        if let Ok(req) = parse_request(&mutated) {
            let re_encoded = encode(&req);
            let reparsed =
                parse_request(&re_encoded).expect("re-encoding an accepted request must parse");
            assert_eq!(reparsed, req);
        }
    }
}

#[test]
fn mutation_fuzz_never_panics() {
    mutation_fuzz(2_000);
}

/// Longer mutation fuzz run, for ad hoc use:
/// `FERRET_PROTOCOL_FUZZ_ITERS=200000 cargo test --workspace -- --ignored
/// mutation_fuzz_long`.
#[test]
#[ignore]
fn mutation_fuzz_long() {
    let iterations = std::env::var("FERRET_PROTOCOL_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    mutation_fuzz(iterations);
}
