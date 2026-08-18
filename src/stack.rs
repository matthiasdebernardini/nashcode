//! Stack inference.
//!
//! dgit has no pull-request model, so a "stack" is derived from the commit graph
//! alone. For each branch B other than the default branch, its parent is the branch P
//! whose tip is an ancestor of B and whose merge base with B is closest to B — that is,
//! the ancestor branch that shares the most history with B. When nothing qualifies, the
//! parent is the default branch.
//!
//! Because P's tip is an ancestor of B's tip and the two tips differ, the parent
//! relation is a strict partial order, so the result is always a forest and never a
//! cycle.

use std::collections::BTreeMap;

use crate::git::{Commit, GitResult, Repo};

/// One branch and its place in the stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchNode {
    pub branch: String,
    pub tip: String,
    /// The inferred parent branch. The default branch has none.
    pub parent: Option<String>,
    /// Commits in `parent..branch`.
    pub ahead: usize,
    /// Branches whose parent is this one, sorted.
    pub children: Vec<String>,
    pub last_commit: Option<Commit>,
}

/// Every branch in a repo, arranged into stacks.
#[derive(Debug, Clone, Default)]
pub struct StackGraph {
    pub default_branch: String,
    pub nodes: BTreeMap<String, BranchNode>,
}

impl StackGraph {
    /// Infer the whole graph by asking git about every branch pair.
    pub async fn infer(repo: &Repo) -> GitResult<Self> {
        let default_branch = repo.default_branch().await?;
        let branches = repo.branches().await?;

        let mut tips = BTreeMap::new();
        for branch in &branches {
            tips.insert(branch.clone(), repo.tip(branch).await?);
        }

        let mut nodes: BTreeMap<String, BranchNode> = BTreeMap::new();
        for branch in &branches {
            let tip = tips[branch].clone();
            let parent = if branch == &default_branch {
                None
            } else {
                Some(
                    infer_parent(repo, branch, &tip, &tips, &default_branch)
                        .await?,
                )
            };

            let ahead = match &parent {
                Some(parent) => repo.count_ahead(&tips[parent], &tip).await.unwrap_or(0),
                None => 0,
            };

            nodes.insert(
                branch.clone(),
                BranchNode {
                    branch: branch.clone(),
                    tip: tip.clone(),
                    parent,
                    ahead,
                    children: Vec::new(),
                    last_commit: repo.last_commit(&tip).await.unwrap_or(None),
                },
            );
        }

        // Link children now that every parent is known.
        let links: Vec<(String, String)> = nodes
            .values()
            .filter_map(|node| node.parent.clone().map(|parent| (parent, node.branch.clone())))
            .collect();
        for (parent, child) in links {
            if let Some(node) = nodes.get_mut(&parent) {
                node.children.push(child);
            }
        }
        for node in nodes.values_mut() {
            node.children.sort();
        }

        Ok(Self { default_branch, nodes })
    }

    pub fn get(&self, branch: &str) -> Option<&BranchNode> {
        self.nodes.get(branch)
    }

    /// The branch names between the default branch and `branch`, inclusive of both.
    pub fn path_to(&self, branch: &str) -> Vec<String> {
        let mut chain = Vec::new();
        let mut current = Some(branch.to_owned());
        // The parent relation is acyclic, but bound the walk anyway: a corrupt graph
        // must not hang a page render.
        let mut guard = 0;
        while let Some(name) = current {
            if guard > self.nodes.len() + 1 {
                break;
            }
            guard += 1;
            let Some(node) = self.nodes.get(&name) else { break };
            chain.push(name.clone());
            current = node.parent.clone();
        }
        chain.reverse();
        chain
    }

    /// Every root-to-leaf chain, each rendered as one Graphite-style column.
    /// A chain always starts at the default branch.
    pub fn chains(&self) -> Vec<Vec<String>> {
        let Some(root) = self.nodes.get(&self.default_branch) else {
            return Vec::new();
        };
        let mut chains = Vec::new();
        walk(self, root, &mut vec![], &mut chains);
        chains
    }

    /// Branches whose parent is the default branch.
    pub fn roots(&self) -> Vec<&BranchNode> {
        self.nodes
            .get(&self.default_branch)
            .map(|root| root.children.iter().filter_map(|c| self.nodes.get(c)).collect())
            .unwrap_or_default()
    }

    /// Every descendant of a branch, parents before children, in restack order.
    pub fn descendants(&self, branch: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut queue: Vec<String> = self
            .nodes
            .get(branch)
            .map(|node| node.children.clone())
            .unwrap_or_default();
        // Breadth-first keeps a parent ahead of its own children, which is the order a
        // restack has to rebase them in.
        while let Some(name) = queue.first().cloned() {
            queue.remove(0);
            if let Some(node) = self.nodes.get(&name) {
                out.push(name);
                queue.extend(node.children.clone());
            }
        }
        out
    }
}

fn walk(
    graph: &StackGraph,
    node: &BranchNode,
    prefix: &mut Vec<String>,
    chains: &mut Vec<Vec<String>>,
) {
    prefix.push(node.branch.clone());
    if node.children.is_empty() {
        chains.push(prefix.clone());
    } else {
        for child in &node.children {
            if let Some(child) = graph.nodes.get(child) {
                walk(graph, child, prefix, chains);
            }
        }
    }
    prefix.pop();
}

/// Pick the parent branch for one branch.
async fn infer_parent(
    repo: &Repo,
    branch: &str,
    tip: &str,
    tips: &BTreeMap<String, String>,
    default_branch: &str,
) -> GitResult<String> {
    let mut best: Option<(usize, String)> = None;

    for (candidate, candidate_tip) in tips {
        // A branch is not its own parent, and two branches sitting on the same commit
        // cannot be each other's parent.
        if candidate == branch || candidate_tip == tip {
            continue;
        }
        if !repo.is_ancestor(candidate_tip, tip).await? {
            continue;
        }
        // The candidate's tip is an ancestor, so it *is* the merge base. The closest
        // merge base is therefore the one with the fewest commits left to reach B.
        let distance = repo.count_ahead(candidate_tip, tip).await?;
        let better = match &best {
            None => true,
            // Ties break by name so the graph is stable between renders.
            Some((best_distance, best_name)) => {
                distance < *best_distance
                    || (distance == *best_distance && candidate < best_name)
            }
        };
        if better {
            best = Some((distance, candidate.clone()));
        }
    }

    Ok(best.map(|(_, name)| name).unwrap_or_else(|| default_branch.to_owned()))
}
