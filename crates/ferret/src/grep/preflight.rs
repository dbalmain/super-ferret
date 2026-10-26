//! Host-independent preparation of a resolved grep plan. The local and protocol
//! hosts both call [`prepare`] once colour and line numbers are settled, so a
//! combination one host refuses cannot run through the other.

use ferret_policy::GrepSelection;
use ferret_query::line_grep::{Filter, Output, Plan};

use super::Refusal;

/// A resolved plan's outcome before any input is opened.
#[derive(Debug)]
pub(super) enum Prepared {
    Search(GrepSelection),
    /// `-m0` selects no line in any file, so no input is opened.
    NoMatch,
}

/// Refuses unsupported resolved combinations, then builds the path selection.
/// The message is the diagnostic without its `grep: ` prefix, which each host
/// adds.
pub(super) fn prepare(plan: &Plan) -> Result<Prepared, Refusal> {
    // In colour rg's notice counts a multiline group's matches, which the
    // rendered lines do not keep.
    if plan.max_columns.is_some()
        && plan.output == Output::Lines
        && !plan.quiet
        && plan.multiline
        && plan.presentation.color.enabled()
        && !plan.json
    {
        return Err(Refusal::Unsupported(
            "-M with -U in colour is not supported; use rg for this command".into(),
        ));
    }
    let selection = selection(plan.hidden, &plan.filters).map_err(Refusal::Failed)?;
    if plan.max_count == Some(0) {
        return Ok(Prepared::NoMatch);
    }
    Ok(Prepared::Search(selection))
}

/// The hidden switch and ordered `-g`, `-t` and `-T` filters, shared with
/// `--files`.
pub(super) fn selection(hidden: bool, filters: &[Filter]) -> Result<GrepSelection, String> {
    let mut selection = GrepSelection::default().with_hidden(hidden);
    for filter in filters {
        match filter {
            Filter::Glob(glob) => selection
                .push(glob)
                .map_err(|error| format!("glob {glob:?}: {error}"))?,
            Filter::Type { name, include } => selection
                .push_type(name, *include)
                .map_err(|error| error.to_string())?,
        }
    }
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferret_query::line_grep::{self, Color};

    // Each row resolves colour as a host would; only the refused row and
    // the invalid glob fail, and `-m0` opens nothing.
    #[test]
    fn prepare_refuses_only_coloured_multiline_line_omission() {
        let refused = "-M with -U in colour is not supported; use rg for this command";
        let cases: &[(&[&str], Color, &str)] = &[
            (&["-U", "-M", "3", "x"], Color::Always, refused),
            (&["-U", "-M", "3", "x"], Color::Ansi, refused),
            (&["-U", "-M", "3", "x"], Color::Never, "search"),
            (&["-M", "3", "x"], Color::Always, "search"),
            (&["-U", "x"], Color::Always, "search"),
            (&["-U", "-M", "3", "--json", "x"], Color::Always, "search"),
            (&["-U", "-M", "3", "-q", "x"], Color::Always, "search"),
            (&["-U", "-M", "3", "-c", "x"], Color::Always, "search"),
            (&["-U", "-M", "3", "-l", "x"], Color::Always, "search"),
            (&["-m0", "x"], Color::Never, "no match"),
            (&["-U", "-M", "3", "-m0", "x"], Color::Always, refused),
            (&["-g", "[", "x"], Color::Never, "glob"),
            (&["-g", "[", "-m0", "x"], Color::Never, "glob"),
            (
                &["-g", "*.rs", "-g", "!a.rs", "--hidden", "x"],
                Color::Never,
                "search",
            ),
        ];
        for (args, color, expected) in cases {
            let args: Vec<_> = args.iter().map(Into::into).collect();
            let line_grep::Command::Search(mut plan) = line_grep::parse(&args).unwrap() else {
                panic!("{args:?} is not a search");
            };
            plan.presentation.color = *color;
            let actual = match prepare(&plan).map_err(Refusal::into_message) {
                Ok(Prepared::Search(_)) => "search".to_owned(),
                Ok(Prepared::NoMatch) => "no match".to_owned(),
                Err(error) if error.starts_with("glob \"[\": ") => "glob".to_owned(),
                Err(error) => error,
            };
            assert_eq!(actual, *expected, "{args:?} {color:?}");
        }
    }
}
