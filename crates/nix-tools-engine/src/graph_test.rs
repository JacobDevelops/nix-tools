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

#[test]
fn normalizes_store_relative_output_paths_from_nix_derivation_json_v4() {
    let graph = DependencyGraph::from_json(
        br#"{
          "version": 4,
          "derivations": {
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo.drv": {
              "outputs": {
                "out": { "path": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-demo" }
              },
              "inputs": { "drvs": {} }
            }
          }
        }"#,
        &BTreeSet::from(["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo.drv".to_owned()]),
        10,
    )
    .unwrap();

    assert_eq!(
        graph.nodes()["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demo.drv"].outputs["out"],
        Some("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-demo".to_owned())
    );
}

#[test]
fn required_outputs_excludes_unreachable_graph_nodes() {
    let graph = DependencyGraph::new(
        BTreeMap::from([
            (
                "dependency.drv".to_owned(),
                DerivationNode {
                    drv_path: "dependency.drv".to_owned(),
                    dependencies: BTreeMap::new(),
                    outputs: BTreeMap::from([("out".to_owned(), Some("dependency".to_owned()))]),
                },
            ),
            (
                "root.drv".to_owned(),
                DerivationNode {
                    drv_path: "root.drv".to_owned(),
                    dependencies: BTreeMap::from([(
                        "dependency.drv".to_owned(),
                        BTreeSet::from(["out".to_owned()]),
                    )]),
                    outputs: BTreeMap::from([("out".to_owned(), Some("root".to_owned()))]),
                },
            ),
            (
                "unrelated.drv".to_owned(),
                DerivationNode {
                    drv_path: "unrelated.drv".to_owned(),
                    dependencies: BTreeMap::new(),
                    outputs: BTreeMap::from([("out".to_owned(), Some("unrelated".to_owned()))]),
                },
            ),
        ]),
        &BTreeSet::from(["root.drv".to_owned()]),
        10,
    )
    .unwrap();

    assert_eq!(
        graph
            .required_outputs(&BTreeMap::from([(
                "root.drv".to_owned(),
                BTreeSet::from(["out".to_owned()]),
            )]))
            .unwrap(),
        BTreeMap::from([
            (
                "dependency.drv".to_owned(),
                BTreeSet::from(["out".to_owned()]),
            ),
            ("root.drv".to_owned(), BTreeSet::from(["out".to_owned()])),
        ])
    );
}
