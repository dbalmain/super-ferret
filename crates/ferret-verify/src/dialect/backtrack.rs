//! Bounded byte backtracking for the canonical syntax emitted by dialect.rs.
//! Capture-aware alternatives stay on an explicit stack, so matching does not
//! recurse on path length. Bracket membership comes from the existing regex
//! engine, keeping its C-locale class semantics shared with ordinary patterns.

use std::fmt;

use super::RegexError;

const STEPS: usize = 1_000_000;
const STACK_BYTES: usize = 8 * 1024 * 1024;

/// A backreference search could not finish within its resource budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchLimit {
    /// The search used its instruction, byte comparison or state copy
    /// allowance.
    Steps,
    /// Pending alternatives would exceed the stack memory allowance.
    PendingStates,
}
impl fmt::Display for MatchLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "backreference regex exceeded its matching budget ({self:?})"
        )
    }
}
impl std::error::Error for MatchLimit {}

#[derive(Clone, Copy, Debug)]
struct ByteSet([u64; 4]);
impl ByteSet {
    fn contains(self, byte: u8) -> bool {
        self.0[usize::from(byte / 64)] & (1 << (byte % 64)) != 0
    }
}

#[derive(Clone, Copy, Debug)]
enum Assertion {
    Start,
    End,
    Boundary(bool),
}
#[derive(Debug)]
enum Node {
    Bytes(ByteSet),
    Assert(Assertion),
    Reference(usize),
    Sequence(Vec<Node>),
    Alternate(Vec<Node>),
    Capture(usize, Box<Node>),
    Repeat(Box<Node>, usize, Option<usize>),
}
#[derive(Clone, Debug)]
enum Instruction {
    Bytes(ByteSet),
    Assert(Assertion),
    Reference(usize),
    Begin(usize),
    End(usize),
    Split(usize),
    Jump(usize),
    Reset(usize),
    Repeat {
        slot: usize,
        min: usize,
        max: Option<usize>,
        end: usize,
    },
    Accept,
}

#[derive(Clone, Debug)]
pub(super) struct Program {
    code: Vec<Instruction>,
    repetitions: usize,
    fold: bool,
}

impl Program {
    pub(super) fn new(translated: &str) -> Result<Self, RegexError> {
        let (_, body) = translated
            .split_once("\\A(?:")
            .ok_or_else(|| RegexError("missing canonical regex prefix".into()))?;
        let body = body
            .strip_suffix(")\\z")
            .ok_or_else(|| RegexError("missing canonical regex suffix".into()))?;
        let flags = translated.split_once(')').map_or("", |(flags, _)| flags);
        let mut parser = Parser {
            bytes: body.as_bytes(),
            at: 0,
            groups: 0,
            flags,
        };
        let node = parser.expression(0)?;
        if parser.at != body.len() {
            return Err(RegexError("unmatched group closer".into()));
        }
        let mut program = Self {
            code: Vec::new(),
            repetitions: 0,
            fold: flags.contains('i'),
        };
        program.compile(&node);
        program.code.push(Instruction::Accept);
        Ok(program)
    }

    fn compile(&mut self, node: &Node) {
        match node {
            Node::Bytes(set) => self.code.push(Instruction::Bytes(*set)),
            Node::Assert(assertion) => self.code.push(Instruction::Assert(*assertion)),
            Node::Reference(group) => self.code.push(Instruction::Reference(*group)),
            Node::Sequence(nodes) => {
                for node in nodes {
                    self.compile(node);
                }
            }
            Node::Alternate(nodes) => {
                let mut jumps = Vec::new();
                for (index, node) in nodes.iter().enumerate() {
                    if index + 1 == nodes.len() {
                        self.compile(node);
                        break;
                    }
                    let split = self.code.len();
                    self.code.push(Instruction::Split(0));
                    self.compile(node);
                    jumps.push(self.code.len());
                    self.code.push(Instruction::Jump(0));
                    self.code[split] = Instruction::Split(self.code.len());
                }
                let end = self.code.len();
                for jump in jumps {
                    self.code[jump] = Instruction::Jump(end);
                }
            }
            Node::Capture(group, node) => {
                if *group < 9 {
                    self.code.push(Instruction::Begin(*group));
                }
                self.compile(node);
                if *group < 9 {
                    self.code.push(Instruction::End(*group));
                }
            }
            Node::Repeat(node, min, max) => {
                let slot = self.repetitions;
                self.repetitions += 1;
                self.code.push(Instruction::Reset(slot));
                let head = self.code.len();
                self.code.push(Instruction::Repeat {
                    slot,
                    min: *min,
                    max: *max,
                    end: 0,
                });
                self.compile(node);
                self.code.push(Instruction::Jump(head));
                let end = self.code.len();
                self.code[head] = Instruction::Repeat {
                    slot,
                    min: *min,
                    max: *max,
                    end,
                };
            }
        }
    }

    pub(super) fn is_match(&self, bytes: &[u8]) -> Result<bool, MatchLimit> {
        self.search(bytes, STEPS, STACK_BYTES)
    }

    fn search(
        &self,
        bytes: &[u8],
        mut budget: usize,
        stack_bytes: usize,
    ) -> Result<bool, MatchLimit> {
        let state_bytes = std::mem::size_of::<State>()
            + self.repetitions * std::mem::size_of::<(usize, Option<usize>)>();
        let stack_limit = stack_bytes / state_bytes;
        let mut pending = Vec::new();
        let mut state = State {
            pc: 0,
            at: 0,
            starts: [0; 9],
            captures: [None; 9],
            repetitions: vec![(0, None); self.repetitions],
        };
        loop {
            spend(&mut budget, 1)?;
            let mut failed = false;
            match self.code[state.pc] {
                Instruction::Bytes(set) => {
                    if bytes.get(state.at).is_some_and(|byte| set.contains(*byte)) {
                        state.at += 1;
                    } else {
                        failed = true;
                    }
                    state.pc += 1;
                }
                Instruction::Assert(assertion) => {
                    let word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
                    let left = state
                        .at
                        .checked_sub(1)
                        .and_then(|at| bytes.get(at))
                        .is_some_and(|byte| word(*byte));
                    let right = bytes.get(state.at).is_some_and(|byte| word(*byte));
                    failed = !match assertion {
                        Assertion::Start => state.at == 0,
                        Assertion::End => state.at == bytes.len(),
                        Assertion::Boundary(boundary) => (left != right) == boundary,
                    };
                    state.pc += 1;
                }
                Instruction::Reference(group) => {
                    if let Some((start, end)) = state.captures[group] {
                        let captured = &bytes[start..end];
                        spend(&mut budget, captured.len())?;
                        if let Some(candidate) =
                            bytes.get(state.at..state.at.saturating_add(captured.len()))
                        {
                            failed = if self.fold {
                                !captured.eq_ignore_ascii_case(candidate)
                            } else {
                                captured != candidate
                            };
                            state.at += captured.len();
                        } else {
                            failed = true;
                        }
                    } else {
                        failed = true;
                    }
                    state.pc += 1;
                }
                Instruction::Begin(group) => {
                    state.starts[group] = state.at;
                    state.pc += 1;
                }
                Instruction::End(group) => {
                    state.captures[group] = Some((state.starts[group], state.at));
                    state.pc += 1;
                }
                Instruction::Split(other) => {
                    push(&mut pending, &state, other, stack_limit, &mut budget)?;
                    state.pc += 1;
                }
                Instruction::Jump(to) => state.pc = to,
                Instruction::Reset(slot) => {
                    state.repetitions[slot] = (0, None);
                    state.pc += 1;
                }
                Instruction::Repeat {
                    slot,
                    min,
                    max,
                    end,
                } => {
                    let (count, last) = state.repetitions[slot];
                    let stop = count >= min;
                    let enter =
                        max.is_none_or(|max| count < max) && !(stop && last == Some(state.at));
                    if enter {
                        if stop {
                            push(&mut pending, &state, end, stack_limit, &mut budget)?;
                        }
                        state.repetitions[slot] = (count + 1, Some(state.at));
                        state.pc += 1;
                    } else if stop {
                        state.pc = end;
                    } else {
                        failed = true;
                    }
                }
                Instruction::Accept => {
                    if state.at == bytes.len() {
                        return Ok(true);
                    }
                    failed = true;
                }
            }
            if failed {
                let Some(alternative) = pending.pop() else {
                    return Ok(false);
                };
                state = alternative;
            }
        }
    }
}

#[derive(Clone)]
struct State {
    pc: usize,
    at: usize,
    starts: [usize; 9],
    captures: [Option<(usize, usize)>; 9],
    repetitions: Vec<(usize, Option<usize>)>,
}
fn spend(budget: &mut usize, steps: usize) -> Result<(), MatchLimit> {
    *budget = budget.checked_sub(steps).ok_or(MatchLimit::Steps)?;
    Ok(())
}
fn push(
    pending: &mut Vec<State>,
    state: &State,
    pc: usize,
    limit: usize,
    budget: &mut usize,
) -> Result<(), MatchLimit> {
    if pending.len() >= limit {
        return Err(MatchLimit::PendingStates);
    }
    // A large pattern must not make each counted step copy thousands of
    // repetition slots for free. Charge for every copied machine word.
    spend(
        budget,
        std::mem::size_of::<State>() / std::mem::size_of::<usize>() + state.repetitions.len() * 3,
    )?;
    let mut alternative = state.clone();
    alternative.pc = pc;
    pending.push(alternative);
    Ok(())
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    groups: usize,
    flags: &'a str,
}
impl Parser<'_> {
    fn expression(&mut self, depth: usize) -> Result<Node, RegexError> {
        if depth > 256 {
            return Err(RegexError("regex nesting limit exceeded".into()));
        }
        let mut branches = Vec::new();
        loop {
            let mut nodes = Vec::new();
            while self.at < self.bytes.len() && !b"|)".contains(&self.bytes[self.at]) {
                let mut node = self.atom(depth)?;
                while let Some(&byte) = self.bytes.get(self.at) {
                    let bounds = match byte {
                        b'*' => (0, None),
                        b'+' => (1, None),
                        b'?' => (0, Some(1)),
                        b'{' => {
                            let start = self.at + 1;
                            let end = self.bytes[start..]
                                .iter()
                                .position(|byte| *byte == b'}')
                                .ok_or_else(|| RegexError("unclosed canonical interval".into()))?
                                + start;
                            let content = std::str::from_utf8(&self.bytes[start..end])
                                .map_err(|e| RegexError(e.to_string()))?;
                            self.at = end;
                            let (min, max) = content
                                .split_once(',')
                                .map_or((content, Some(content)), |(min, max)| {
                                    (min, if max.is_empty() { None } else { Some(max) })
                                });
                            let number = |value: &str| {
                                value
                                    .parse::<usize>()
                                    .map_err(|e| RegexError(e.to_string()))
                            };
                            (number(min)?, max.map(number).transpose()?)
                        }
                        _ => break,
                    };
                    self.at += 1;
                    node = Node::Repeat(Box::new(node), bounds.0, bounds.1);
                }
                nodes.push(node);
            }
            branches.push(Node::Sequence(nodes));
            if self.bytes.get(self.at) != Some(&b'|') {
                break;
            }
            self.at += 1;
        }
        Ok(Node::Alternate(branches))
    }

    fn atom(&mut self, depth: usize) -> Result<Node, RegexError> {
        let start = self.at;
        let byte = self.bytes[self.at];
        self.at += 1;
        match byte {
            b'(' => {
                let capture = if self.bytes[self.at..].starts_with(b"?:") {
                    self.at += 2;
                    None
                } else {
                    let group = self.groups;
                    self.groups += 1;
                    Some(group)
                };
                let node = self.expression(depth + 1)?;
                if self.bytes.get(self.at) != Some(&b')') {
                    return Err(RegexError("unclosed canonical group".into()));
                }
                self.at += 1;
                Ok(match capture {
                    Some(group) => Node::Capture(group, Box::new(node)),
                    None => node,
                })
            }
            b'^' => Ok(Node::Assert(Assertion::Start)),
            b'$' => Ok(Node::Assert(Assertion::End)),
            b'\\' => {
                let escaped = *self
                    .bytes
                    .get(self.at)
                    .ok_or_else(|| RegexError("canonical trailing backslash".into()))?;
                self.at += 1;
                match escaped {
                    b'1'..=b'9' => return Ok(Node::Reference(usize::from(escaped - b'1'))),
                    b'b' | b'B' => return Ok(Node::Assert(Assertion::Boundary(escaped == b'b'))),
                    b'x' => self.at += 2,
                    b'w' | b'W' => {}
                    _ => return Err(RegexError("invalid canonical escape".into())),
                }
                self.byte_set(start)
            }
            b'[' => {
                while let Some(&byte) = self.bytes.get(self.at) {
                    if byte == b'[' && self.bytes.get(self.at + 1) == Some(&b':') {
                        self.at += 2;
                        while self.at < self.bytes.len()
                            && !self.bytes[self.at..].starts_with(b":]")
                        {
                            self.at += 1;
                        }
                        self.at += 2;
                    } else {
                        self.at += 1;
                        if byte == b']' {
                            return self.byte_set(start);
                        }
                    }
                }
                Err(RegexError("unclosed canonical bracket".into()))
            }
            b'.' => self.byte_set(start),
            _ => Err(RegexError("invalid canonical atom".into())),
        }
    }

    fn byte_set(&self, start: usize) -> Result<Node, RegexError> {
        let fragment = std::str::from_utf8(&self.bytes[start..self.at])
            .map_err(|e| RegexError(e.to_string()))?;
        let pattern = format!("{})\\A(?:{fragment})\\z", self.flags);
        let regex = regex::bytes::Regex::new(&pattern).map_err(|e| RegexError(e.to_string()))?;
        let mut set = ByteSet([0; 4]);
        for byte in 0..=u8::MAX {
            if regex.is_match(&[byte]) {
                set.0[usize::from(byte / 64)] |= 1 << (byte % 64);
            }
        }
        Ok(Node::Bytes(set))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_exhaustion_is_an_error_and_empty_repetitions_terminate() -> Result<(), RegexError> {
        let program = Program::new(r"(?-u)\A(?:(\x61*)\1)\z")?;
        assert_eq!(
            program.search(b"aaaa", 1, STACK_BYTES),
            Err(MatchLimit::Steps)
        );
        assert_eq!(
            program.search(b"aaaa", STEPS, 0),
            Err(MatchLimit::PendingStates)
        );
        let empty = Program::new(r"(?-u)\A(?:(\x61*)*\1)\z")?;
        assert_eq!(empty.is_match(b""), Ok(true));
        assert_eq!(empty.is_match(b"b"), Ok(false));
        Ok(())
    }
}
