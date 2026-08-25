use std::collections::{BTreeMap, BTreeSet};

use super::{DependencyGraph, DerivationNode};

#[test]
fn rejects_dependency_cycles_with_stable_node_list() {
    let graph = DependencyGraph::new(
        BTreeMap::from([
            (
                "a.drv".to_owned(),
                DerivationNode {
                    drv_path: "a.drv".to_owned(),
                    dependencies: BTreeMap::from([(
                        "b.drv".to_owned(),
                        BTreeSet::from(["out".to_owned()]),
                    )]),
                    outputs: BTreeMap::from([("out".to_owned(), Some("a".to_owned()))]),
                },
            ),
            (
                "b.drv".to_owned(),
                DerivationNode {
                    drv_path: "b.drv".to_owned(),
                    dependencies: BTreeMap::from([(
                        "a.drv".to_owned(),
                        BTreeSet::from(["out".to_owned()]),
                    )]),
                    outputs: BTreeMap::from([("out".to_owned(), Some("b".to_owned()))]),
                },
            ),
        ]),
        &BTreeSet::from(["a.drv".to_owned()]),
        10,
    )
    .expect_err("cycle");

    assert_eq!(graph.code(), "derivation_cycle");
    assert_eq!(
        graph.message(),
        "derivation graph contains a cycle: a.drv, b.drv"
    );
}

#[test]
fn rejects_missing_dependency_outputs() {
    let error = DependencyGraph::new(
        BTreeMap::from([
            (
                "a.drv".to_owned(),
                DerivationNode {
                    drv_path: "a.drv".to_owned(),
                    dependencies: BTreeMap::new(),
                    outputs: BTreeMap::from([("dev".to_owned(), Some("a-dev".to_owned()))]),
                },
            ),
            (
                "b.drv".to_owned(),
                DerivationNode {
                    drv_path: "b.drv".to_owned(),
                    dependencies: BTreeMap::from([(
                        "a.drv".to_owned(),
                        BTreeSet::from(["out".to_owned()]),
                    )]),
                    outputs: BTreeMap::from([("out".to_owned(), Some("b".to_owned()))]),
                },
            ),
        ]),
        &BTreeSet::from(["b.drv".to_owned()]),
        10,
    )
    .expect_err("missing output");

    assert_eq!(error.code(), "missing_dependency_output");
}
