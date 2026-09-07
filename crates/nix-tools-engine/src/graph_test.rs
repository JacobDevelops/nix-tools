use std::collections::{BTreeMap, BTreeSet};

use nix_tools_core::process::StreamConsumer;

use super::{DependencyGraph, DerivationNode};
use crate::graph::GraphStream;

const ROOT: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-root.drv";
const DEPENDENCY: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dependency.drv";

fn roots(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|path| (*path).to_owned()).collect()
}

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

#[test]
fn keeps_only_the_consumed_fields_of_a_v4_document() {
    let document = format!(
        r#"{{
          "version": 4,
          "derivations": {{
            "{DEPENDENCY}": {{
              "name": "dependency",
              "system": "x86_64-linux",
              "builder": "/nix/store/zzzz-bash/bin/bash",
              "args": ["-c", "true"],
              "env": {{"out": "/nix/store/cccc", "buildInputs": "a b c"}},
              "outputs": {{"out": {{"path": "/nix/store/cccccccccccccccccccccccccccccccc-dependency"}}}},
              "inputs": {{"srcs": ["/nix/store/dddd-source"], "drvs": {{}}}}
            }},
            "{ROOT}": {{
              "env": {{"noise": "discarded"}},
              "outputs": {{"out": {{"path": "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-root"}}}},
              "inputs": {{
                "drvs": {{"{DEPENDENCY}": {{"outputs": ["out"], "dynamicOutputs": {{}}}}}}
              }}
            }}
          }}
        }}"#
    );

    let graph =
        DependencyGraph::from_reader(document.as_bytes(), &roots(&[ROOT]), 10).expect("graph");

    assert_eq!(graph.topological_order(), [DEPENDENCY, ROOT]);
    assert_eq!(
        graph.get(ROOT).expect("root").dependencies,
        BTreeMap::from([(DEPENDENCY.to_owned(), BTreeSet::from(["out".to_owned()]))])
    );
    assert_eq!(
        graph.get(ROOT).expect("root").outputs["out"],
        Some("/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-root".to_owned())
    );
}

#[test]
fn accepts_the_v3_input_derivation_field_and_a_top_level_derivation_map() {
    let document = format!(
        r#"{{
          "{DEPENDENCY}": {{
            "outputs": {{"out": "/nix/store/cccccccccccccccccccccccccccccccc-dependency"}},
            "inputDrvs": {{}}
          }},
          "{ROOT}": {{
            "outputs": {{"out": null}},
            "inputDrvs": {{"{DEPENDENCY}": ["out"]}}
          }}
        }}"#
    );

    let graph =
        DependencyGraph::from_reader(document.as_bytes(), &roots(&[ROOT]), 10).expect("graph");

    assert_eq!(
        graph.get(ROOT).expect("root").dependencies,
        BTreeMap::from([(DEPENDENCY.to_owned(), BTreeSet::from(["out".to_owned()]))])
    );
    assert_eq!(graph.get(ROOT).expect("root").outputs["out"], None);
}

#[test]
fn an_empty_document_yields_an_empty_graph() {
    let graph = DependencyGraph::from_reader(
        br#"{"version": 4, "derivations": {}}"#.as_slice(),
        &BTreeSet::new(),
        10,
    )
    .expect("graph");

    assert!(graph.is_empty());
    assert!(graph.topological_order().is_empty());
}

#[test]
fn a_stream_that_ends_mid_document_is_a_parse_failure() {
    let truncated = format!(
        r#"{{"version": 4, "derivations": {{"{ROOT}": {{"outputs": {{"out": {{"path": "/nix/st"#
    );

    let error = DependencyGraph::from_reader(truncated.as_bytes(), &roots(&[ROOT]), 10)
        .expect_err("truncated stream");

    assert_eq!(error.code(), "invalid_graph_json");
}

#[test]
fn the_node_limit_stops_the_parse_before_the_rest_of_the_stream() {
    // Everything after the second derivation is unparseable, so only an early bail can succeed
    // in reporting the limit rather than a syntax error.
    let document = format!(
        r#"{{"version": 4, "derivations": {{
          "{DEPENDENCY}": {{"outputs": {{"out": null}}, "inputs": {{"drvs": {{}}}}}},
          "{ROOT}": {{"outputs": {{"out": null}}, "inputs": {{"drvs": {{}}}}}},
          not json at all"#
    );

    let error = DependencyGraph::from_reader(document.as_bytes(), &BTreeSet::new(), 1)
        .expect_err("node limit");

    assert_eq!(error.code(), "graph_node_limit_exceeded");
    assert_eq!(
        error.message(),
        "derivation graph exceeds the configured limit of 1"
    );
}

#[test]
fn malformed_nodes_keep_their_own_protocol_codes() {
    let cases = [
        (r#"{"derivations": 4}"#.to_owned(), "invalid_graph_schema"),
        (r"[]".to_owned(), "invalid_graph_schema"),
        (
            format!(r#"{{"derivations": {{"{ROOT}": 4}}}}"#),
            "invalid_graph_node",
        ),
        (
            format!(r#"{{"derivations": {{"{ROOT}": {{"inputs": {{"drvs": {{}}}}}}}}}}"#),
            "invalid_graph_outputs",
        ),
        (
            format!(r#"{{"derivations": {{"{ROOT}": {{"outputs": {{"": null}}}}}}}}"#),
            "invalid_output_name",
        ),
        (
            format!(r#"{{"derivations": {{"{ROOT}": {{"outputs": {{"out": {{"path": 4}}}}}}}}}}"#),
            "invalid_graph_output_path",
        ),
        (
            format!(r#"{{"derivations": {{"{ROOT}": {{"outputs": {{"out": true}}}}}}}}"#),
            "invalid_graph_output",
        ),
        (
            format!(
                r#"{{"derivations": {{"{ROOT}": {{"outputs": {{"out": null}}, "inputs": {{"drvs": 4}}}}}}}}"#
            ),
            "invalid_input_derivations",
        ),
        (
            format!(
                r#"{{"derivations": {{"{ROOT}": {{"outputs": {{"out": null}}, "inputDrvs": {{"{DEPENDENCY}": 4}}}}}}}}"#
            ),
            "invalid_input_outputs",
        ),
        (
            format!(
                r#"{{"derivations": {{"{ROOT}": {{"outputs": {{"out": null}}, "inputDrvs": {{"{DEPENDENCY}": [""]}}}}}}}}"#
            ),
            "invalid_input_output_name",
        ),
        (
            format!(
                r#"{{"derivations": {{"{ROOT}": {{"outputs": {{"out": null}}, "inputDrvs": {{"{DEPENDENCY}": []}}}}}}}}"#
            ),
            "empty_input_output_selection",
        ),
    ];

    for (document, code) in cases {
        let error = DependencyGraph::from_reader(document.as_bytes(), &BTreeSet::new(), 10)
            .expect_err(&document);
        assert_eq!(error.code(), code, "{document}");
    }
}

#[test]
fn a_graph_stream_reports_the_parse_it_performed_and_refuses_a_stream_it_never_saw() {
    let stream = GraphStream::new(roots(&[ROOT]), 10);
    let document =
        format!(r#"{{"version": 4, "derivations": {{"{ROOT}": {{"outputs": {{"out": null}}}}}}}}"#);

    stream
        .consume(&mut document.as_bytes())
        .expect("stream read");

    assert_eq!(stream.take().expect("graph").nodes().len(), 1);
    assert_eq!(
        stream.take().expect_err("already taken").code(),
        "invalid_graph_json"
    );
}
