"""Classify normalized grep audit differences using each row's own evidence.

The scratch adapter supplies approved ignored paths from its policy fixture;
this module contains no private corpus paths, content or command arguments.
"""


def accepted_visibility(row, ignored_paths):
    """Accept only additional records for approved ignored paths, with no loss.

    Records must be normalized names or path-prefixed line/count records. Paths
    containing colons are conservatively refused: the prefix is ambiguous.
    Missing evidence is a refusal, not a default to an accepted difference.
    """
    extras = row.get("target_only_records")
    return (
        row.get("status") == "differ"
        and row.get("categories") == ["stdout"]
        and row.get("oracle_exit") == row.get("target_exit") == 0
        and row.get("oracle_stderr") == row.get("target_stderr") == ""
        and row.get("oracle_only_records") == []
        and isinstance(extras, list)
        and bool(extras)
        and all(
            isinstance(record, str) and record.partition(":")[0] in ignored_paths
            for record in extras
        )
    )


def accepted_pattern_syntax(row):
    """Recognize the documented trailing-comment exception from row evidence.

    rg rejects its inserted wrapper while ferret compiles the valid argument.
    Requiring the argument, diagnostic, exits, channels and record deltas keeps
    this separate from visibility and from unexplained successful searches.
    """
    extras = row.get("target_only_records")
    arguments = row.get("argv")
    diagnostic = row.get("oracle_stderr")
    if not isinstance(extras, list) or not all(isinstance(x, str) for x in extras):
        return False
    if not isinstance(arguments, list) or not all(
        isinstance(x, str) for x in arguments
    ):
        return False
    channels = ["exit", "stderr", "stdout"] if extras else ["exit", "stderr"]
    return (
        row.get("status") == "differ"
        and row.get("categories") == channels
        and row.get("oracle_exit") == 2
        and row.get("target_exit") in (0, 1)
        and (row.get("target_exit") == 0 or not extras)
        and row.get("oracle_only_records") == []
        and row.get("target_stderr") == ""
        and isinstance(diagnostic, str)
        and diagnostic.startswith("rg: regex parse error:\n")
        and "(?:" in diagnostic
        and diagnostic.endswith("error: unclosed group\n")
        and any(
            "(?x)" in argument and "#" in argument and f"(?:{argument})" in diagnostic
            for argument in arguments
        )
    )
