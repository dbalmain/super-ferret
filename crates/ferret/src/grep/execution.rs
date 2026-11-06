//! One grep search over resolved inputs: preparing the pinned catalog,
//! index and holders, then searching each segment of operands.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use ferret_policy::GrepSelection;
use ferret_query::find::EntrySource;
use ferret_query::line_grep::{self, Plan};

use crate::cli::Exit;
use crate::engine;

use super::*;

// Both hosts supply the same resolved environment and pinned generation.
#[derive(Clone, Copy)]
pub(super) struct Execution<'a> {
    pub(super) plan: &'a Plan,
    pub(super) selection: &'a GrepSelection,
    pub(super) pin: Option<&'a engine::QuerySession>,
    pub(super) live: Option<&'a live::Anchors>,
    pub(super) cwd: &'a Path,
    pub(super) threads: usize,
    pub(super) runtime: &'a Runtime<'a>,
    pub(super) start: std::time::Instant,
}

impl<'a> Execution<'a> {
    pub(super) fn run<W: Write>(self, inputs: Vec<Input>, writer: W) -> Exit {
        self.search(inputs, writer).unwrap_or_else(|exit| exit)
    }

    /// The search. A failure ends it early as `Err`, carrying the exit it
    /// reported.
    fn search<W: Write>(self, inputs: Vec<Input>, writer: W) -> Result<Exit, Exit> {
        let Self {
            plan,
            runtime,
            start,
            ..
        } = self;
        let cancelled = runtime.cancelled;
        let stopped = || cancelled.load(Ordering::Acquire);
        let recursive = inputs
            .iter()
            .any(|input| matches!(input, Input::Directory(_)));
        let prepared = self.prepare(&inputs)?;
        // D99 asks whether the walk selected any file; pruning can skip
        // the only ones. Kept to ask again without it.
        let operands: Option<Vec<PathBuf>> = (prepared.holders.is_some() && plan.paths.is_empty())
            .then(|| {
                inputs
                    .iter()
                    .filter_map(|input| match input {
                        Input::Directory(path) => Some(path.clone()),
                        _ => None,
                    })
                    .collect()
            });
        let mut host = Host {
            plan,
            filename: plan
                .presentation
                .filename
                .unwrap_or(recursive || inputs.len() > 1),
            writer,
            runtime: Some(runtime),
            buffer: Vec::new(),
            matched: false,
            emitted_group: false,
            sent_output: false,
            json_stats: json::Stats::default(),
            errors: false,
            cancelled: Some(cancelled),
        };
        // rg diagnoses failed explicit operands before searching, even when an
        // earlier operand supplies a quiet match.
        let inputs = host.report_failed(inputs, runtime)?;
        // D99: with no path operand, a walk whose filters selected no file is
        // diagnosed at exit.
        let mut searched = false;
        if self.search_inputs(&mut host, inputs, &prepared, &mut searched)? && !plan.json {
            return Ok(Exit::Ok);
        }
        if let Some(operands) = operands.filter(|_| !searched && !stopped()) {
            searched = self.selects_a_file(operands);
        }
        if plan.json
            && let Err(error) = json::summary(&mut host.writer, &host.json_stats, start)
        {
            return Err(runtime.output_failure(&error));
        }
        host.writer
            .flush()
            .map_err(|error| runtime.output_failure(&error))?;
        Ok(host.exit(searched, runtime))
    }

    /// Searches `inputs` in order, each run of walkable operands as one
    /// segment and each stream alone. Returns whether a quiet search
    /// stopped at its first match.
    fn search_inputs<W: Write>(
        &self,
        host: &mut Host<'_, W>,
        inputs: Vec<Input>,
        prepared: &Prepared<'_>,
        searched: &mut bool,
    ) -> Result<bool, Exit> {
        let Self {
            plan,
            live,
            runtime,
            ..
        } = *self;
        let mut inputs = inputs.into_iter().peekable();
        while inputs.peek().is_some() {
            let mut segment = Vec::new();
            while inputs.peek().is_some_and(|input| {
                !matches!(input, Input::Stream(_))
                    && (segment.is_empty()
                        || !buffered_path_output(plan, runtime.tty) && covered_inputs(live))
            }) {
                if let Some(input) = inputs.next() {
                    segment.push(input);
                }
            }
            if !segment.is_empty() {
                self.segment(host, segment, prepared, searched)?;
                if plan.quiet && host.matched {
                    return Ok(true);
                }
                if buffered_path_output(plan, runtime.tty)
                    && let Err(error) = host.writer.flush()
                {
                    return Err(runtime.output_failure(&error));
                }
            }
            if let Some(Input::Stream(path)) =
                inputs.next_if(|input| matches!(input, Input::Stream(_)))
            {
                *searched = true;
                self.stream(host, &path)?;
                if plan.quiet && host.matched {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// The catalog's part of a search, before any walk: the index's
    /// candidates, probed against the documents the walks can reach, the
    /// documents' reader, and the directories a pruned walk lists.
    fn prepare(&self, inputs: &[Input]) -> Result<Prepared<'a>, Exit> {
        let Self {
            plan, pin, runtime, ..
        } = *self;
        let stopped = || runtime.cancelled.load(Ordering::Acquire);
        let mut candidates = if !plan.invert {
            pin.map(|pin| pin.grep_candidates(&plan.grep, &stopped))
                .transpose()
                .map_err(|error| runtime.failure(&error.to_string()))?
        } else {
            None
        };
        let documents = pin
            .map(|pin| {
                ferret_crawl::Documents::by_name(pin.catalog()).map(|documents| {
                    documents.with_config(ferret_policy::Config { size_cap: u64::MAX })
                })
            })
            .transpose()
            .map_err(|error| runtime.failure(&error.to_string()))?;
        // The catalog documents the walks below can reach: candidates are
        // decoded for these alone (S5). Listing a name costs about what
        // decoding a candidate does, so past half the candidates' bound,
        // decoding them all is the cheaper answer.
        let mut reachable = candidates.as_ref().zip(pin).map(|(candidates, pin)| {
            (
                ferret_index::DocSet::empty(pin.catalog().doc_count()),
                candidates.bound() / 2,
            )
        });
        self.check_directories(inputs, &mut reachable)?;
        // A walk too large to probe lists only the directories holding a
        // candidate. Absent files, followed links and live walks need every
        // directory.
        let prune = reachable.is_none()
            && plan.universe() == line_grep::Universe::Candidates
            && !plan.traversal.follow
            && !plan.traversal.unrestricted;
        if let (Some(candidates), Some((reachable, _))) = (&mut candidates, reachable)
            && let Err(error) = candidates.probe(reachable, &stopped)
        {
            return Err(runtime.failure(&error.to_string()));
        }
        let holders = self.holders(candidates.as_ref(), prune, &stopped)?;
        Ok(Prepared {
            candidates,
            documents,
            holders,
        })
    }

    /// The directories a pruned walk lists. Past one candidate a document,
    /// every directory may hold one, unless the globs and types admit files
    /// by name.
    fn holders(
        &self,
        candidates: Option<&engine::GrepCandidates<'a>>,
        prune: bool,
        stopped: &dyn Fn() -> bool,
    ) -> Result<Option<holders::Holders>, Exit> {
        let Self {
            plan,
            selection,
            pin,
            runtime,
            ..
        } = *self;
        match candidates.zip(pin).filter(|(candidates, pin)| {
            prune
                && (candidates.bound() < u64::from(pin.catalog().doc_count())
                    || selection.whitelists_names())
        }) {
            Some((candidates, pin)) => {
                holders::Holders::new(pin.catalog(), candidates, selection, plan.text, stopped)
                    .map_err(|error| runtime.failure(&error.to_string()))
            }
            None => Ok(None),
        }
    }

    /// Refuses before any output when a live operand's anchors fail to
    /// validate or a catalogued operand's walk cannot start, and gathers
    /// the documents `reachable` probes for.
    fn check_directories(
        &self,
        inputs: &[Input],
        reachable: &mut Option<(ferret_index::DocSet, u64)>,
    ) -> Result<(), Exit> {
        let Self {
            pin,
            live,
            cwd,
            runtime,
            ..
        } = *self;
        let stopped = || runtime.cancelled.load(Ordering::Acquire);
        for input in inputs {
            if let Input::Directory(path) = input {
                if let Some(anchors) = live.filter(|anchors| anchors.walks(path)) {
                    if let Err(error) = anchors.validate(path) {
                        return Err(runtime.failure(&format!("{}: {error}", path.display())));
                    }
                } else if let Some(pin) = pin {
                    let mut source =
                        sources::Directory::new(path, Some(pin.catalog()), cwd, false, false)
                            .map_err(|error| runtime.failure(&error.to_string()))?;
                    if let Some(Err(error)) = source.next(true) {
                        return Err(runtime.failure(&format!(
                        "{}: {}; refresh the index with ferret index DIR, or use -u to search current files",
                        error.path.display(),
                        error.error
                    )));
                    }
                    if let Some((docs, budget)) = reachable {
                        match documents_below(
                            pin.catalog(),
                            &cwd.join(path),
                            docs,
                            budget,
                            &stopped,
                        ) {
                            Ok(true) => {}
                            Ok(false) => *reachable = None,
                            Err(error) => return Err(runtime.failure(&error.to_string())),
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The source that lists `segment`'s inputs and their candidates.
    fn source<'s>(&self, segment: Vec<Input>, prepared: &'s Prepared<'a>) -> groups::Source<'s> {
        let plan = self.plan;
        groups::Source::new(
            segment,
            self.pin.map(|p| p.catalog()),
            prepared
                .candidates
                .as_ref()
                .map(|candidates| candidates as &dyn groups::Candidates),
            self.selection,
            self.cwd.to_owned(),
        )
        .with_traversal(plan.traversal, plan.text)
        .with_live(self.live)
        .with_holders(prepared.holders.as_ref())
        .with_absence(plan.universe() == line_grep::Universe::Searchable)
    }

    /// Searches one segment of operands, on several workers when it walks
    /// live.
    fn segment<W: Write>(
        &self,
        host: &mut Host<'_, W>,
        segment: Vec<Input>,
        prepared: &Prepared<'_>,
        searched: &mut bool,
    ) -> Result<(), Exit> {
        let Self {
            pin,
            cwd,
            threads,
            runtime,
            ..
        } = *self;
        let cancelled = runtime.cancelled;
        let fork = || {
            pin.zip(prepared.documents.as_ref())
                .map(|(pin, documents)| engine::reader::checked(pin.catalog(), documents))
        };
        let threads = if segment
            .iter()
            .any(|input| matches!(input, Input::Directory(_)))
        {
            threads
        } else {
            threads.min(segment.len())
        };
        let mut source = self.source(segment, prepared);
        let walked = (threads > 1 && source.walks_live())
            .then(|| groups::walk(host, &mut source, threads, cancelled, searched));
        let serial = match walked {
            None => Some(threads),
            Some(Ok(groups::Walk::Done)) => None,
            // D108 B: the calling thread searches the operands alone.
            Some(Ok(groups::Walk::Refused(cause))) => {
                if let Err(error) = host.report_shortfall(&ferret_query::Shortfall {
                    running: 1,
                    asked: threads,
                    cause,
                }) {
                    return Err(runtime.output_failure(&error));
                }
                Some(1)
            }
            Some(Err(error)) => return Err(runtime.output_failure(&error)),
        };
        if let Some(threads) = serial {
            let result = groups::run(
                host,
                |stopped| source.next(stopped),
                cwd,
                &fork,
                self.pin.map(|pin| pin.catalog()),
                threads,
                cancelled,
            );
            *searched |= source.searched();
            result.map_err(|error| runtime.output_failure(&error))?;
        }
        Ok(())
    }

    /// Searches a stream operand, `-` being stdin, front to back.
    fn stream<W: Write>(&self, host: &mut Host<'_, W>, path: &Path) -> Result<(), Exit> {
        let result = if path == Path::new("-") {
            host.direct(path, &mut io::stdin().lock(), u64::MAX)
        } else {
            match fs::File::open(self.cwd.join(path)) {
                Ok(mut file) => host.direct(path, &mut file, u64::MAX),
                Err(error) => host.error(path, &error),
            }
        };
        result.map_err(|error| self.runtime.output_failure(&error))
    }

    /// Whether an unpruned walk of `operands` selects any file (D99).
    fn selects_a_file(&self, operands: Vec<PathBuf>) -> bool {
        let Self {
            plan,
            selection,
            pin,
            live,
            cwd,
            runtime,
            ..
        } = *self;
        let stopped = || runtime.cancelled.load(Ordering::Acquire);
        let mut source = groups::Source::new(
            operands.into_iter().map(Input::Directory).collect(),
            pin.map(|p| p.catalog()),
            None,
            selection,
            cwd.to_owned(),
        )
        .with_traversal(plan.traversal, plan.text)
        .with_live(live)
        .until_searched();
        while source.next(&stopped).is_some() {}
        source.searched()
    }
}

/// What [`Execution::prepare`] takes from the pinned catalog and index.
struct Prepared<'a> {
    candidates: Option<engine::GrepCandidates<'a>>,
    documents: Option<ferret_crawl::Documents>,
    holders: Option<holders::Holders>,
}
