//! Resolve grep line qualifiers and smart case before compiling and deriving.
//! Syntax-aware case analysis uses public regex-syntax AST traversal; the
//! executable profile and its necessary query are compiled from the same HIR.

use regex_syntax::ast::{self, Ast, ClassSetItem};

use super::{Grep, GrepError, GrepFlags};

/// Line-profile settings, resolved in argv order by the query parser.
#[derive(Clone, Copy, Debug, Default)]
pub struct LineFlags {
    pub fixed: bool,
    pub smart_case: bool,
    pub word: bool,
    pub line: bool,
}

#[derive(Default)]
struct Literals {
    any: bool,
    upper: bool,
}
impl Literals {
    fn literal(&mut self, c: char) {
        self.any = true;
        self.upper |= c.is_uppercase();
    }
}
impl ast::Visitor for Literals {
    type Output = Self;
    type Err = std::convert::Infallible;
    fn finish(self) -> Result<Self, Self::Err> {
        Ok(self)
    }
    fn visit_pre(&mut self, ast: &Ast) -> Result<(), Self::Err> {
        if let Ast::Literal(literal) = ast {
            self.literal(literal.c);
        }
        Ok(())
    }
    fn visit_class_set_item_pre(&mut self, item: &ClassSetItem) -> Result<(), Self::Err> {
        match item {
            ClassSetItem::Literal(literal) => self.literal(literal.c),
            ClassSetItem::Range(range) => {
                self.literal(range.start.c);
                self.literal(range.end.c);
            }
            _ => {}
        }
        Ok(())
    }
}

impl GrepFlags {
    /// Resolve fixed strings, smart case and half/whole-line boundaries before
    /// deriving necessary trigrams. Inline flags remain scoped to arguments.
    pub fn compile_line_profile(
        &self,
        patterns: &[String],
        flags: LineFlags,
    ) -> Result<Grep, GrepError> {
        let patterns: Vec<_> = patterns
            .iter()
            .map(|pattern| {
                if flags.fixed {
                    regex::escape(pattern)
                } else {
                    pattern.clone()
                }
            })
            .collect();
        let mut resolved = *self;
        if flags.smart_case {
            let mut any = false;
            let mut upper = false;
            for pattern in &patterns {
                let ast = ast::parse::Parser::new()
                    .parse(pattern)
                    .map_err(|error| GrepError::Pattern(super::RegexError(error.to_string())))?;
                let literals = match ast::visit(&ast, Literals::default()) {
                    Ok(literals) => literals,
                    Err(never) => match never {},
                };
                any |= literals.any;
                upper |= literals.upper;
            }
            resolved.case_insensitive = any && !upper;
        }
        if !flags.word
            && !flags.line
            && let [pattern] = patterns.as_slice()
        {
            return resolved.compile_lines(pattern);
        }
        let mut alternatives = Vec::with_capacity(patterns.len());
        for pattern in &patterns {
            // Resolving first prevents extended-mode comments swallowing a
            // wrapper, and prevents one argument's inline flags leaking.
            resolved.compile_lines(pattern)?;
            let hir = resolved
                .parser()
                .build()
                .parse(pattern)
                .map_err(|error| GrepError::Derivation(error.to_string()))?;
            alternatives.push(format!("(?:{hir})"));
        }
        let body = if alternatives.is_empty() {
            // No pattern-file lines is the empty language, not an empty regex.
            String::from("[a&&b]")
        } else {
            alternatives.join("|")
        };
        let body = if flags.line {
            format!("^(?:{body})$")
        } else if flags.word {
            format!(r"\b{{start-half}}(?:{body})\b{{end-half}}")
        } else {
            body
        };
        // Each HIR has resolved its own inline folding, including (?-i).
        GrepFlags {
            case_insensitive: false,
        }
        .compile_lines(&body)
    }
}
