//! Explaining a content query from the dictionaries alone: each node's
//! possible and maybe bounds ([`Bounds`]), gathered into a [`ContentPlan`].
//! No postings are read.

use super::*;
use crate::query::NameTest;

/// Dictionary-only upper bounds in the document domain. Neither bound may
/// be subtracted: dead documents and aliases can inflate them independently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub possible: u64,
    pub maybe: u64,
}

/// A content plan's bounds as they are estimated, with a report for each
/// node.
struct Estimate<'a> {
    catalog: &'a Catalog,
    index: &'a NameIndex,
    atoms: &'a [(u64, Certainty, u64)],
    live: u64,
    reports: Vec<NodeReport>,
}

impl Estimate<'_> {
    fn report(&mut self, expression: &str, bounds: Bounds) {
        self.reports.push(NodeReport {
            expression: expression.into(),
            bounds,
        });
    }

    /// The names the name index estimates `test` matches.
    fn name(&self, test: &NameTest) -> Result<Bounds, RunError> {
        Ok(Bounds {
            possible: self
                .index
                .select(self.catalog, None, &[], |name| test.matches(name))
                .map_err(RunError::Stale)?
                .estimate
                .hits
                .min(self.live),
            maybe: 0,
        })
    }

    /// The bounds of `tree`, reporting each node after its children.
    fn node(&mut self, tree: &Node<Test>) -> Result<Bounds, RunError> {
        let live = self.live;
        let bounds = match tree {
            Node::Leaf(Test::Content(i)) => {
                let possible = self.atoms[*i].0.min(live);
                Bounds {
                    possible,
                    maybe: self.atoms[*i].2.min(possible),
                }
            }
            Node::Leaf(Test::Name(test)) => self.name(test)?,
            Node::Leaf(_) => Bounds::all(live),
            Node::Not(inner) => Bounds {
                possible: live,
                maybe: self.node(inner)?.maybe,
            },
            Node::And(all) => {
                let mut bounds = Bounds::all(live);
                for child in all {
                    bounds = bounds.and(self.node(child)?);
                }
                bounds
            }
            Node::Or(any) => {
                let mut bounds = Bounds {
                    possible: 0,
                    maybe: 0,
                };
                for child in any {
                    let b = self.node(child)?;
                    bounds.possible = bounds.possible.saturating_add(b.possible).min(live);
                    bounds.maybe = bounds.maybe.saturating_add(b.maybe).min(live);
                }
                bounds
            }
        };
        let expression = tree.describe(&|test| match test {
            Test::Content(i) => format!("content[{i}]"),
            Test::Name(_) => "name".into(),
            Test::Path(_) => "path".into(),
            Test::Meta(_) => "metadata".into(),
        });
        self.reports.push(NodeReport { bounds, expression });
        Ok(bounds)
    }
}

impl Bounds {
    /// Every one of `live` documents possible, none read yet.
    fn all(live: u64) -> Self {
        Self {
            possible: live,
            maybe: 0,
        }
    }

    fn and(self, other: Self) -> Self {
        let possible = self.possible.min(other.possible);
        Self {
            possible,
            maybe: possible.min(self.maybe.saturating_add(other.maybe)),
        }
    }
}

impl Query {
    /// Explains the dictionary-only plan, including refused expressions.
    /// No postings are read. Execution records candidate counts separately.
    pub fn explain_content(
        &self,
        catalog: &Catalog,
        index: &NameIndex,
        pinned: &Pinned<'_>,
    ) -> Result<ContentPlan, RunError> {
        let atoms = self
            .atoms
            .iter()
            .map(|atom| atom.estimate_until(pinned, &|| false))
            .collect::<Result<Vec<_>, _>>()
            .map_err(RunError::Index)?;
        self.plan_from_estimates(catalog, index, pinned, &atoms, &|| false)
    }

    pub(super) fn plan_from_estimates(
        &self,
        catalog: &Catalog,
        index: &NameIndex,
        pinned: &Pinned<'_>,
        atoms: &[(u64, Certainty, u64)],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ContentPlan, RunError> {
        let live = u64::from(pinned.live().len());
        let mut estimate = Estimate {
            catalog,
            index,
            atoms,
            live,
            reports: Vec::new(),
        };
        for test in &self.names {
            let bounds = estimate.name(test)?;
            estimate.report("name", bounds);
        }
        for _ in &self.paths {
            estimate.report("path", Bounds::all(live));
        }
        for _ in &self.meta {
            estimate.report("metadata", Bounds::all(live));
        }
        let mut bounds = Bounds {
            possible: self.name_estimate(catalog, index)?.min(live),
            maybe: 0,
        };
        for tree in &self.residual {
            bounds = bounds.and(estimate.node(tree)?);
        }
        let mut greps = Vec::new();
        for atom in &self.atoms {
            if let crate::ContentAtom::Grep(grep) = atom {
                greps.push(crate::grep::explain(grep, pinned, cancelled).map_err(RunError::Index)?);
            }
        }
        let reports = estimate.reports;
        Ok(ContentPlan {
            root: bounds,
            nodes: reports,
            greps,
        })
    }
}

/// One expression node's P/M bounds, in postorder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeReport {
    pub expression: String,
    pub bounds: Bounds,
}

/// The pre-read content plan, available even when execution refuses a scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentPlan {
    pub root: Bounds,
    pub nodes: Vec<NodeReport>,
    pub greps: Vec<crate::grep::GrepReport>,
}

impl ContentPlan {
    /// Includes every node, derived tree, selected keys and dictionary df.
    pub fn describe(&self) -> String {
        format!(
            "P={} M={}; nodes {:?}; regex plans {:?}",
            self.root.possible, self.root.maybe, self.nodes, self.greps
        )
    }
}
