//! Resolve grep execution flags before shared compilation and derivation.
//! Syntax-aware case analysis uses public regex-syntax AST traversal; the
//! executable profile and its necessary query are compiled from the same HIR.

use regex_syntax::ast::{self, Ast, ClassSetItem};

use super::{Grep, GrepError, GrepFlags};

/// Grep execution settings, resolved in argv order by the query parser.
#[derive(Clone, Copy, Debug, Default)]
pub struct LineFlags {
    pub fixed: bool,
    pub smart_case: bool,
    pub word: bool,
    pub line: bool,
    pub multiline: bool,
    pub dotall: bool,
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
    /// Compile the shared execution profile and resolve replacement capture
    /// references against that same regex. Replacement never affects the
    /// necessary query or document truth.
    pub fn compile_line_replacement(
        &self,
        patterns: &[String],
        flags: LineFlags,
        template: &str,
    ) -> Result<(Grep, crate::Replacement), GrepError> {
        let grep = self.compile_line_profile(patterns, flags)?;
        let replacement = grep.regex().replacement(template);
        Ok((grep, replacement))
    }


    /// Resolve fixed strings, smart case and half/whole-line boundaries before
    /// deriving necessary trigrams. Inline flags remain scoped to arguments.
    pub fn compile_line_profile(
        &self,
        patterns: &[String],
        flags: LineFlags,
    ) -> Result<Grep, GrepError> {
        let compile = |settings: &GrepFlags, pattern: &str| {
            if flags.multiline {
                settings.compile(pattern)
            } else {
                settings.compile_lines(pattern)
            }
        };
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
        // Resolve dotall before each argument's inline flags. Lowering the
        // resulting HIR keeps matching and derivation on identical settings.
        let patterns: Vec<_> = patterns
            .into_iter()
            .map(|pattern| {
                if flags.multiline && flags.dotall {
                    format!("(?s){pattern}")
                } else {
                    pattern
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
            return compile(&resolved, pattern);
        }
        let mut alternatives = Vec::with_capacity(patterns.len());
        for pattern in &patterns {
            // Resolving first prevents extended-mode comments swallowing a
            // wrapper, and prevents one argument's inline flags leaking.
            compile(&resolved, pattern)?;
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
        compile(
            &GrepFlags {
                case_insensitive: false,
            },
            &body,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conservative_lf_classification_uses_resolved_hir_not_reachability() {
        let flags = GrepFlags {
            case_insensitive: false,
        };
        for (pattern, excludes) in [
            ("foo", true),
            (r"foo|\A\z\n", false),
            (r"foo|\A\z", false),
            (r"foo|[a&&b]\n", false),
            (r"foo|[^\s\S]", true),
            (r"foo|\s", false),
            (r"foo|(?s:.)", false),
            (r"foo|(?-s:.)", true),
            (r"foo|\n{0}", true),
            (r"foo\b", true),
            (r"\Afoo", false),
            (r"\A", false),
            ("^", false),
            (r"\b", true),
            ("", true),
            (r"foo|^a$", false),
        ] {
            let grep = flags
                .compile_line_profile(
                    &[pattern.to_owned()],
                    LineFlags {
                        multiline: true,
                        ..LineFlags::default()
                    },
                )
                .unwrap();
            assert_eq!(grep.excludes_lf(), excludes, "{pattern}");
        }
    }
}
