//! Decides once, before any output, where a request's operands are searched:
//! ferretd, the local snapshot, a live walk, or plain reads. Content search
//! and `--files` share it, so a refusal and the resources it implies cannot
//! disagree between the two modes.

use ferret_query::line_grep::Traversal;

use super::*;

/// A request whose operands have been resolved and routed.
pub(super) struct Routed {
    pub inputs: Vec<Input>,
    /// Policy for walked directories: every directory under `--live`, the
    /// uncovered ones otherwise.
    pub live: Option<live::Anchors>,
    /// Some directory operand is read from the snapshot.
    pub catalog: bool,
    /// ferretd may answer the whole request (D111).
    pub daemon: bool,
}

pub(super) fn route(
    inputs: Vec<Input>,
    traversal: Traversal,
    index: Option<&Path>,
) -> Result<Routed, Refusal> {
    let recursive = inputs
        .iter()
        .any(|input| matches!(input, Input::Directory(_)));
    let indexed = recursive && !traversal.unrestricted && !traversal.live;
    let anchors = indexed
        .then(|| live::Anchors::routing(index, &inputs))
        .transpose()
        .map_err(Refusal::Failed)?;
    let catalog = indexed
        && anchors
            .as_ref()
            .is_none_or(|anchors| anchors.any_covered(&inputs));
    if recursive && traversal.follow && !traversal.unrestricted && (traversal.live || catalog) {
        return Err(Refusal::Unsupported(
            "-L is supported for uncovered directories and -u; use -u to follow links in an indexed tree"
                .into(),
        ));
    }
    let daemon = anchors.as_ref().is_none_or(live::Anchors::all_covered)
        && !traversal.unrestricted
        && !traversal.live
        && !test_injections()
        && daemon_operands(&inputs);
    let live = if recursive && traversal.live && !traversal.unrestricted {
        Some(live::Anchors::open(index).map_err(Refusal::Failed)?)
    } else {
        anchors.map(live::Anchors::with_git_global)
    };
    Ok(Routed {
        inputs,
        live,
        catalog,
        daemon,
    })
}
