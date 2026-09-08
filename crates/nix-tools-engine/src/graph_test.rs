use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use nix_tools_core::process::StreamConsumer;

use super::{DependencyGraph, DerivationNode, EngineError};
use crate::graph::GraphStream;

const ROOT: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-root.drv";
const DEPENDENCY: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dependency.drv";

/// Generous next to every fixture here, so only a test that means to exhaust it does.
const RETAINED_BYTES: usize = 1024 * 1024;

/// Runs a document through both public entry points and requires them to agree.
///
/// They share one parser, so a divergence would mean the slice and reader paths had drifted apart
/// rather than that a document was mis-parsed.
fn parse_both(
    document: &str,
    roots: &BTreeSet<String>,
    max_nodes: usize,
    max_retained_bytes: usize,
) -> Result<DependencyGraph, EngineError> {
    let buffered =
        DependencyGraph::from_json(document.as_bytes(), roots, max_nodes, max_retained_bytes);
    let streamed =
        DependencyGraph::from_reader(document.as_bytes(), roots, max_nodes, max_retained_bytes);
    match (&buffered, &streamed) {
        (Ok(left), Ok(right)) => assert_eq!(left.nodes(), right.nodes(), "{document}"),
        (Err(left), Err(right)) => {
            assert_eq!(
                (left.code(), left.message()),
                (right.code(), right.message())
            );
        }
        _ => panic!("entry points disagreed on {document}"),
    }
    streamed
}

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
        RETAINED_BYTES,
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
        DependencyGraph::from_reader(document.as_bytes(), &roots(&[ROOT]), 10, RETAINED_BYTES)
            .expect("graph");

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
        DependencyGraph::from_reader(document.as_bytes(), &roots(&[ROOT]), 10, RETAINED_BYTES)
            .expect("graph");

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
        RETAINED_BYTES,
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

    let error =
        DependencyGraph::from_reader(truncated.as_bytes(), &roots(&[ROOT]), 10, RETAINED_BYTES)
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

    let error =
        DependencyGraph::from_reader(document.as_bytes(), &BTreeSet::new(), 1, RETAINED_BYTES)
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
        let error =
            parse_both(&document, &BTreeSet::new(), 10, RETAINED_BYTES).expect_err(&document);
        assert_eq!(error.code(), code, "{document}");
    }
}

#[test]
fn a_graph_stream_reports_the_parse_it_performed_and_refuses_a_stream_it_never_saw() {
    let stream = GraphStream::new(roots(&[ROOT]), 10, RETAINED_BYTES);
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

/// One derivation declaring more outputs than the budget allows, with unparseable trailing bytes
/// so that only an incremental charge can report the budget rather than a syntax error.
fn one_node_with_outputs(count: usize) -> String {
    let mut document = format!(r#"{{"version": 4, "derivations": {{"{ROOT}": {{"outputs": {{"#);
    for index in 0..count {
        if index > 0 {
            document.push(',');
        }
        let _ = write!(document, r#""out{index}": null"#);
    }
    document.push_str("not json at all");
    document
}

#[test]
fn one_derivation_cannot_exhaust_memory_through_its_outputs() {
    let error = DependencyGraph::from_reader(
        one_node_with_outputs(20_000).as_bytes(),
        &BTreeSet::new(),
        10,
        16 * 1024,
    )
    .expect_err("output budget");

    assert_eq!(error.code(), "graph_memory_limit_exceeded");
    assert_eq!(
        error.message(),
        "derivation graph retains more than the configured limit of 16384 bytes"
    );
}

#[test]
fn one_derivation_cannot_exhaust_memory_through_its_inputs() {
    let mut document = format!(
        r#"{{"version": 4, "derivations": {{"{ROOT}": {{"outputs": {{"out": null}}, "inputs": {{"drvs": {{"#
    );
    for index in 0..20_000 {
        if index > 0 {
            document.push(',');
        }
        let _ = write!(document, r#""/nix/store/{index:032}-input.drv": ["out"]"#);
    }
    document.push_str("not json at all");

    let error = DependencyGraph::from_reader(document.as_bytes(), &BTreeSet::new(), 10, 16 * 1024)
        .expect_err("input budget");

    assert_eq!(error.code(), "graph_memory_limit_exceeded");
}

#[test]
fn one_input_cannot_exhaust_memory_through_its_selected_output_names() {
    let mut document = format!(
        r#"{{"version": 4, "derivations": {{"{ROOT}": {{"outputs": {{"out": null}}, "inputs": {{"drvs": {{"{DEPENDENCY}": ["#
    );
    for index in 0..20_000 {
        if index > 0 {
            document.push(',');
        }
        let _ = write!(document, r#""out{index}""#);
    }
    document.push_str("not json at all");

    let error = DependencyGraph::from_reader(document.as_bytes(), &BTreeSet::new(), 10, 16 * 1024)
        .expect_err("selection budget");

    assert_eq!(error.code(), "graph_memory_limit_exceeded");
}

#[test]
fn many_small_derivations_cannot_exhaust_memory_under_the_node_limit() {
    let mut document = r#"{"version": 4, "derivations": {"#.to_owned();
    for index in 0..20_000 {
        if index > 0 {
            document.push(',');
        }
        let _ = write!(
            document,
            r#""/nix/store/{index:032}-spread.drv": {{"outputs": {{"out": null}}}}"#
        );
    }
    document.push_str("not json at all");

    // The node limit is deliberately high enough that only the byte budget can stop this.
    let error =
        DependencyGraph::from_reader(document.as_bytes(), &BTreeSet::new(), 1_000_000, 16 * 1024)
            .expect_err("spread budget");

    assert_eq!(error.code(), "graph_memory_limit_exceeded");
}

#[test]
fn the_budget_charges_entries_rather_than_whole_nodes() {
    // 200 outputs against a budget that fits far fewer: the parse must stop inside the first node.
    let error = DependencyGraph::from_reader(
        one_node_with_outputs(200).as_bytes(),
        &BTreeSet::new(),
        10,
        RETAINED_BYTES,
    )
    .expect_err("trailing garbage");
    // With a budget this generous the entries all fit, so the garbage is what fails: proof the
    // earlier cases stopped on the budget and not on the syntax error.
    assert_eq!(error.code(), "invalid_graph_json");
}

#[test]
fn the_versioned_wrapper_ignores_sibling_keys_the_way_reading_the_whole_object_did() {
    let node = format!(r#""{DEPENDENCY}": {{"outputs": {{"out": null}}}}"#);
    let phantom = format!(r#""{ROOT}": {{"outputs": {{"out": null}}}}"#);

    // A sibling that is not a derivation at all must not be read as one.
    let graph = parse_both(
        &format!(r#"{{"version": 4, "derivations": {{{node}}}, "noise": "x"}}"#),
        &BTreeSet::new(),
        10,
        RETAINED_BYTES,
    )
    .expect("sibling ignored");
    assert_eq!(graph.nodes().len(), 1);

    // A sibling shaped like a derivation must not enter the graph from outside the wrapper,
    // whether it arrives before or after it.
    for document in [
        format!(r#"{{"derivations": {{{node}}}, {phantom}}}"#),
        format!(r#"{{{phantom}, "derivations": {{{node}}}}}"#),
    ] {
        let graph =
            parse_both(&document, &BTreeSet::new(), 10, RETAINED_BYTES).expect("no phantom node");
        assert_eq!(graph.nodes().len(), 1, "{document}");
        assert!(graph.get(DEPENDENCY).is_some(), "{document}");
    }

    // A repeated wrapper keeps the last map, as reading the document into one object did.
    let graph = parse_both(
        &format!(r#"{{"derivations": {{{phantom}}}, "derivations": {{{node}}}}}"#),
        &BTreeSet::new(),
        10,
        RETAINED_BYTES,
    )
    .expect("last wrapper wins");
    assert_eq!(graph.nodes().len(), 1);
    assert!(graph.get(DEPENDENCY).is_some());
}

#[test]
fn wrapper_siblings_are_ignored_before_the_wrapper_even_when_they_are_invalid_nodes() {
    for sibling in [
        r#""noise": "x""#.to_owned(),
        r#""noise": [true, 2, 3.5, null, "text"]"#.to_owned(),
        format!(r#""{ROOT}": {{"inputDrvs": {{}}}}"#),
        format!(r#""{ROOT}": {{"outputs": {{"": {{"path": "unused"}}}}}}"#),
        format!(
            r#""{ROOT}": {{"outputs": {{"out": {{"path": []}}, "other": null}}, "inputDrvs": {{}}}}"#
        ),
        format!(
            r#""{ROOT}": {{"outputs": {{"out": null}}, "inputDrvs": {{"{DEPENDENCY}": [{{"bad": true}}, "out"]}}}}"#
        ),
    ] {
        let document = format!(
            r#"{{{sibling}, "derivations": {{"{DEPENDENCY}": {{"outputs": {{"out": null}}}}}}}}"#
        );
        let graph = parse_both(&document, &roots(&[DEPENDENCY]), 10, RETAINED_BYTES)
            .expect("wrapper ignores preceding sibling");
        assert_eq!(graph.nodes().len(), 1);
    }
}

#[test]
fn discarded_wrapper_siblings_do_not_spend_the_graph_limits() {
    let node = format!(r#""{DEPENDENCY}": {{"outputs": {{"out": null}}}}"#);
    for sibling in [
        format!(r#""{}": {{"outputs": {{"out": null}}}}"#, "x".repeat(5000)),
        format!(
            r#""{ROOT}": {{"outputs": {{"{}": null}}}}"#,
            "x".repeat(5000)
        ),
        format!(
            r#""{ROOT}": {{"outputs": {{"out": null}}}}, "another.drv": {{"outputs": {{"out": null}}}}"#
        ),
    ] {
        let graph = parse_both(
            &format!(r#"{{{sibling}, "derivations": {{{node}}}}}"#),
            &roots(&[DEPENDENCY]),
            1,
            512,
        )
        .expect("discarded siblings do not count");
        assert_eq!(graph.nodes().len(), 1);
    }
}

#[test]
fn speculative_legacy_errors_survive_without_a_wrapper() {
    for (node, code) in [
        (r#""text""#, "invalid_graph_node"),
        (
            r#"{"outputs":{"out":{"path":[]}}}"#,
            "invalid_graph_output_path",
        ),
        (r#"{"inputDrvs":{}}"#, "invalid_graph_outputs"),
    ] {
        let error = parse_both(
            &format!(r#"{{"{ROOT}": {node}}}"#),
            &BTreeSet::new(),
            10,
            RETAINED_BYTES,
        )
        .expect_err("legacy error retained");
        assert_eq!(error.code(), code);
    }
}

#[test]
fn a_syntax_error_while_draining_a_sibling_cannot_be_hidden_by_the_wrapper() {
    for sibling in [
        r#""noise": [true,]"#.to_owned(),
        format!(r#""{ROOT}": {{"outputs": {{"out": {{"path": []}}, "extra": }}}}"#),
        format!(r#""{}": [true,]"#, "x".repeat(5000)),
    ] {
        let error = parse_both(
            &format!(r#"{{{sibling}, "derivations": {{}}}}"#),
            &BTreeSet::new(),
            10,
            RETAINED_BYTES,
        )
        .expect_err("invalid JSON cannot be discarded");
        assert_eq!(error.code(), "invalid_graph_json");
    }
}

#[test]
fn a_node_reports_the_first_defect_it_meets_whichever_key_carries_it() {
    // Streaming adjudicates fields as they arrive, so the code follows document order by design
    // rather than by accident. Both orderings are pinned here so the contract cannot drift.
    let inputs_first =
        format!(r#"{{"derivations": {{"{ROOT}": {{"inputs": {{"drvs": 4}}, "outputs": 4}}}}}}"#);
    let outputs_first =
        format!(r#"{{"derivations": {{"{ROOT}": {{"outputs": 4, "inputs": {{"drvs": 4}}}}}}}}"#);

    assert_eq!(
        parse_both(&inputs_first, &BTreeSet::new(), 10, RETAINED_BYTES)
            .expect_err("inputs first")
            .code(),
        "invalid_input_derivations"
    );
    assert_eq!(
        parse_both(&outputs_first, &BTreeSet::new(), 10, RETAINED_BYTES)
            .expect_err("outputs first")
            .code(),
        "invalid_graph_outputs"
    );

    // A missing `outputs` is adjudicated after the whole node, so it stays order-independent.
    for document in [
        format!(r#"{{"derivations": {{"{ROOT}": {{"inputDrvs": {{}}, "name": "x"}}}}}}"#),
        format!(r#"{{"derivations": {{"{ROOT}": {{"name": "x", "inputDrvs": {{}}}}}}}}"#),
    ] {
        assert_eq!(
            parse_both(&document, &BTreeSet::new(), 10, RETAINED_BYTES)
                .expect_err("missing outputs")
                .code(),
            "invalid_graph_outputs",
            "{document}"
        );
    }
}

#[test]
fn a_malformed_inputs_field_is_refused_rather_than_silently_dropping_dependencies() {
    // The parser this replaced fell through to `inputDrvs` and produced a node with no
    // dependencies at all, which is a wrong answer a build graph must not give.
    for inputs in ["\"x\"", "[]", "4"] {
        let document = format!(
            r#"{{"derivations": {{"{ROOT}": {{"outputs": {{"out": null}}, "inputs": {inputs}}}}}}}"#
        );
        assert_eq!(
            parse_both(&document, &BTreeSet::new(), 10, RETAINED_BYTES)
                .expect_err("malformed inputs")
                .code(),
            "invalid_input_derivations",
            "{document}"
        );
    }
}

#[test]
fn a_duplicated_key_is_refused_rather_than_resolved_to_the_last_value() {
    // Streaming cannot see the second value without having consumed the first, and an object that
    // says two things is ambiguous input rather than input with a defined answer.
    let duplicate_node =
        format!(r#"{{"derivations": {{"{ROOT}": 4, "{ROOT}": {{"outputs": {{"out": null}}}}}}}}"#);
    assert_eq!(
        parse_both(&duplicate_node, &BTreeSet::new(), 10, RETAINED_BYTES)
            .expect_err("duplicate node")
            .code(),
        "invalid_graph_node"
    );

    let duplicate_outputs =
        format!(r#"{{"derivations": {{"{ROOT}": {{"outputs": 4, "outputs": {{"out": null}}}}}}}}"#);
    assert_eq!(
        parse_both(&duplicate_outputs, &BTreeSet::new(), 10, RETAINED_BYTES)
            .expect_err("duplicate outputs")
            .code(),
        "invalid_graph_outputs"
    );
}

#[test]
fn one_oversized_name_is_refused_without_quoting_it_back() {
    let name = "o".repeat(5000);
    let document = format!(r#"{{"derivations": {{"{ROOT}": {{"outputs": {{"{name}": null}}}}}}}}"#);

    let error =
        parse_both(&document, &BTreeSet::new(), 10, RETAINED_BYTES).expect_err("oversized name");

    assert_eq!(error.code(), "graph_string_limit_exceeded");
    assert_eq!(
        error.message(),
        "derivation graph contains a 5000-byte name, exceeding the 4096-byte limit"
    );
    // The message is what reaches a persisted diagnostic, so it must not carry the value.
    assert!(!error.message().contains(&name));
}

#[test]
fn a_failed_read_is_not_reported_as_a_malformed_document() {
    struct FailingReader;

    impl std::io::Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("pipe went away"))
        }
    }

    let error = DependencyGraph::from_reader(FailingReader, &BTreeSet::new(), 10, RETAINED_BYTES)
        .expect_err("read failure");

    assert_eq!(error.code(), "graph_stream_read_failed");

    for prefix in [
        r#"{"noise": "x", "more": "#.to_owned(),
        r#"{"noise": [true,"#.to_owned(),
        format!(r#"{{"{ROOT}": {{"outputs": {{"out": {{"path": []}}, "extra": "#),
        format!(r#"{{"{}": [true,"#, "x".repeat(5000)),
    ] {
        let reader = std::io::Read::chain(prefix.as_bytes(), FailingReader);
        let error = DependencyGraph::from_reader(reader, &BTreeSet::new(), 10, RETAINED_BYTES)
            .expect_err("read failure after speculative semantic error");
        assert_eq!(error.code(), "graph_stream_read_failed", "{prefix}");
        assert!(error.message().contains("pipe went away"));
    }

    // The consumer must hand that back to the runner instead of storing it as a parse verdict.
    let stream = GraphStream::new(BTreeSet::new(), 10, RETAINED_BYTES);
    let failure = stream
        .consume(&mut FailingReader)
        .expect_err("consumer reports the read failure");
    assert!(failure.to_string().contains("pipe went away"));
}
