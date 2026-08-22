//! A torn stack graph must not hang a page render or a restack.

use std::collections::BTreeMap;

use nashcode::stack::{BranchNode, StackGraph};

fn node(branch: &str, parent: Option<&str>, children: &[&str]) -> BranchNode {
    BranchNode {
        branch: branch.to_owned(),
        tip: format!("{branch}-tip"),
        parent: parent.map(str::to_owned),
        ahead: 0,
        children: children.iter().map(|c| (*c).to_owned()).collect(),
        last_commit: None,
    }
}

#[test]
fn a_cyclic_stack_graph_terminates() {
    // main -> a -> b -> a. Inference cannot produce this from one consistent ref
    // snapshot, but a stale in-memory graph or a torn read can, and neither the
    // stacks page nor a restack may spin on it.
    let nodes: BTreeMap<String, BranchNode> = [
        node("main", None, &["a"]),
        node("a", Some("main"), &["b"]),
        node("b", Some("a"), &["a"]),
    ]
    .into_iter()
    .map(|n| (n.branch.clone(), n))
    .collect();
    let graph = StackGraph { default_branch: "main".to_owned(), nodes };

    // `chains` recursed until the stack overflowed before the walk carried a visited set.
    let chains = graph.chains();
    assert_eq!(chains, vec![vec!["main".to_string(), "a".into(), "b".into()]]);

    // `descendants` queued `a` again on every lap before the same fix.
    assert_eq!(graph.descendants("main"), vec!["a".to_string(), "b".into()]);

    // `path_to` was already bounded; it stays bounded.
    assert!(graph.path_to("b").len() <= 3);
}
