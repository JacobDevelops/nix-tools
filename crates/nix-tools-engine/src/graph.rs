use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use crate::{DerivationNode, EngineError};

/// Validated, deduplicated derivation dependency graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DependencyGraph {
    nodes: BTreeMap<String, DerivationNode>,
    order: Vec<String>,
}

impl DependencyGraph {
    /// Validates caller-supplied nodes, required roots, referenced outputs, and acyclicity.
    ///
    /// # Errors
    ///
    /// Returns a stable protocol error for a node limit, mismatched identity, missing reference,
    /// missing output, missing root, or dependency cycle.
    pub fn new(
        nodes: BTreeMap<String, DerivationNode>,
        roots: &BTreeSet<String>,
        max_nodes: usize,
    ) -> Result<Self, EngineError> {
        if nodes.len() > max_nodes {
            return Err(EngineError::new(
                "graph_node_limit_exceeded",
                format!(
                    "derivation graph contains {} nodes, exceeding the configured limit of {max_nodes}",
                    nodes.len()
                ),
            ));
        }
        for (path, node) in &nodes {
            if path != &node.drv_path {
                return Err(EngineError::new(
                    "derivation_identity_mismatch",
                    format!("graph key {path} does not match node {}", node.drv_path),
                ));
            }
        }
        for root in roots {
            if !nodes.contains_key(root) {
                return Err(EngineError::new(
                    "missing_graph_root",
                    format!("derivation graph omitted evaluated root {root}"),
                ));
            }
        }
        for (path, node) in &nodes {
            for (dependency, outputs) in &node.dependencies {
                let dependency_node = nodes.get(dependency).ok_or_else(|| {
                    EngineError::new(
                        "missing_graph_reference",
                        format!("derivation {path} references missing {dependency}"),
                    )
                })?;
                if let Some(output) = outputs
                    .iter()
                    .find(|output| !dependency_node.outputs.contains_key(*output))
                {
                    return Err(EngineError::new(
                        "missing_dependency_output",
                        format!(
                            "derivation {path} references missing output {output} from {dependency}"
                        ),
                    ));
                }
            }
        }
        let order = topological_order(&nodes)?;
        Ok(Self { nodes, order })
    }

    /// Parses either the legacy top-level derivation map or the versioned `derivations` map.
    ///
    /// # Errors
    ///
    /// Returns a stable protocol error for malformed JSON or an invalid graph.
    pub fn from_json(
        bytes: &[u8],
        roots: &BTreeSet<String>,
        max_nodes: usize,
    ) -> Result<Self, EngineError> {
        let value: Value = serde_json::from_slice(bytes).map_err(|error| {
            EngineError::new(
                "invalid_graph_json",
                format!("parse nix derivation graph JSON: {error}"),
            )
        })?;
        let object = value.as_object().ok_or_else(|| {
            EngineError::new(
                "invalid_graph_schema",
                "nix derivation graph must be a JSON object",
            )
        })?;
        let derivations = match object.get("derivations") {
            Some(Value::Object(derivations)) => derivations,
            Some(_) => {
                return Err(EngineError::new(
                    "invalid_graph_schema",
                    "derivations must be a JSON object",
                ));
            }
            None => object,
        };
        if derivations.len() > max_nodes.saturating_add(1) {
            return Err(EngineError::new(
                "graph_node_limit_exceeded",
                format!("derivation graph exceeds the configured limit of {max_nodes}"),
            ));
        }
        let mut nodes = BTreeMap::new();
        for (raw_path, value) in derivations {
            if raw_path == "version" {
                continue;
            }
            let drv_path = normalize_derivation_path(raw_path);
            let object = value.as_object().ok_or_else(|| {
                EngineError::new(
                    "invalid_graph_node",
                    format!("derivation {drv_path} must be an object"),
                )
            })?;
            let outputs = parse_outputs(&drv_path, object.get("outputs"))?;
            let dependencies = parse_dependencies(&drv_path, object)?;
            nodes.insert(
                drv_path.clone(),
                DerivationNode {
                    drv_path,
                    dependencies,
                    outputs,
                },
            );
        }
        Self::new(nodes, roots, max_nodes)
    }

    /// Returns graph nodes in derivation-path order.
    #[must_use]
    pub fn nodes(&self) -> &BTreeMap<String, DerivationNode> {
        &self.nodes
    }

    /// Returns the deterministic dependency-first order.
    #[must_use]
    pub fn topological_order(&self) -> &[String] {
        &self.order
    }

    /// Returns the node for a derivation path.
    #[must_use]
    pub fn get(&self, drv_path: &str) -> Option<&DerivationNode> {
        self.nodes.get(drv_path)
    }

    /// Returns whether the graph contains no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub(crate) fn required_outputs(
        &self,
        roots: &BTreeMap<String, BTreeSet<String>>,
    ) -> Result<BTreeMap<String, BTreeSet<String>>, EngineError> {
        let mut required = self
            .nodes
            .keys()
            .map(|path| (path.clone(), BTreeSet::new()))
            .collect::<BTreeMap<_, _>>();
        let mut pending = roots.keys().cloned().collect::<Vec<_>>();
        for (path, outputs) in roots {
            if let Some(selected) = required.get_mut(path) {
                selected.extend(outputs.iter().cloned());
            }
        }
        let mut seen = BTreeSet::new();
        while let Some(path) = pending.pop() {
            if !seen.insert(path.clone()) {
                continue;
            }
            let Some(node) = self.nodes.get(&path) else {
                continue;
            };
            for (dependency, outputs) in &node.dependencies {
                let selected = required.get_mut(dependency).ok_or_else(|| {
                    EngineError::new(
                        "missing_graph_reference",
                        format!("derivation {path} references missing {dependency}"),
                    )
                })?;
                selected.extend(outputs.iter().cloned());
                pending.push(dependency.clone());
            }
        }
        required.retain(|_, selected| !selected.is_empty());
        Ok(required)
    }
}

fn normalize_derivation_path(path: &str) -> String {
    if path.contains('/') || path.strip_suffix(".drv").is_none() {
        path.to_owned()
    } else {
        let hash = path.split_once('-').map(|(hash, _)| hash);
        if hash.is_some_and(|hash| hash.len() == 32) {
            format!("/nix/store/{path}")
        } else {
            path.to_owned()
        }
    }
}

fn normalize_output_path(path: &str) -> String {
    if path.contains('/') {
        return path.to_owned();
    }
    let hash = path.split_once('-').map(|(hash, _)| hash);
    if hash.is_some_and(|hash| hash.len() == 32) {
        format!("/nix/store/{path}")
    } else {
        path.to_owned()
    }
}

fn parse_outputs(
    drv_path: &str,
    value: Option<&Value>,
) -> Result<BTreeMap<String, Option<String>>, EngineError> {
    let outputs = value.and_then(Value::as_object).ok_or_else(|| {
        EngineError::new(
            "invalid_graph_outputs",
            format!("derivation {drv_path} outputs must be an object"),
        )
    })?;
    outputs
        .iter()
        .map(|(name, value)| {
            if name.is_empty() {
                return Err(EngineError::new(
                    "invalid_output_name",
                    format!("derivation {drv_path} has an empty output name"),
                ));
            }
            let path = match value {
                Value::Null => None,
                Value::String(path) => Some(normalize_output_path(path)),
                Value::Object(output) => match output.get("path") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(path)) => Some(normalize_output_path(path)),
                    Some(_) => {
                        return Err(EngineError::new(
                            "invalid_graph_output_path",
                            format!("derivation {drv_path} output {name} path must be a string"),
                        ));
                    }
                },
                _ => {
                    return Err(EngineError::new(
                        "invalid_graph_output",
                        format!("derivation {drv_path} output {name} must be an object"),
                    ));
                }
            };
            Ok((name.clone(), path))
        })
        .collect()
}

fn parse_dependencies(
    drv_path: &str,
    node: &Map<String, Value>,
) -> Result<BTreeMap<String, BTreeSet<String>>, EngineError> {
    let raw = node
        .get("inputs")
        .and_then(Value::as_object)
        .and_then(|inputs| inputs.get("drvs"))
        .or_else(|| node.get("inputDrvs"));
    let Some(raw) = raw else {
        return Ok(BTreeMap::new());
    };
    let drvs = raw.as_object().ok_or_else(|| {
        EngineError::new(
            "invalid_input_derivations",
            format!("derivation {drv_path} input derivations must be an object"),
        )
    })?;
    drvs.iter()
        .map(|(raw_dependency, value)| {
            let dependency = normalize_derivation_path(raw_dependency);
            let values = value.as_array().or_else(|| {
                value
                    .as_object()
                    .and_then(|object| object.get("outputs"))
                    .and_then(Value::as_array)
            });
            let values = values.ok_or_else(|| {
                EngineError::new(
                    "invalid_input_outputs",
                    format!(
                        "derivation {drv_path} input {dependency} outputs must be an array"
                    ),
                )
            })?;
            let outputs = values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .filter(|output| !output.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            EngineError::new(
                                "invalid_input_output_name",
                                format!(
                                    "derivation {drv_path} input {dependency} output names must be non-empty strings"
                                ),
                            )
                        })
                })
                .collect::<Result<BTreeSet<_>, _>>()?;
            if outputs.is_empty() {
                return Err(EngineError::new(
                    "empty_input_output_selection",
                    format!("derivation {drv_path} selects no outputs from {dependency}"),
                ));
            }
            Ok((dependency, outputs))
        })
        .collect()
}

fn topological_order(nodes: &BTreeMap<String, DerivationNode>) -> Result<Vec<String>, EngineError> {
    let mut remaining = nodes
        .iter()
        .map(|(path, node)| (path.clone(), node.dependencies.len()))
        .collect::<BTreeMap<_, _>>();
    let mut ready = remaining
        .iter()
        .filter_map(|(path, count)| (*count == 0).then_some(path.clone()))
        .collect::<BTreeSet<_>>();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (path, node) in nodes {
        for dependency in node.dependencies.keys() {
            dependents
                .entry(dependency)
                .or_default()
                .push(path.as_str());
        }
    }
    let mut order = Vec::with_capacity(nodes.len());
    while let Some(path) = ready.pop_first() {
        order.push(path.clone());
        for dependent in dependents.get(path.as_str()).into_iter().flatten() {
            let count = remaining.get_mut(*dependent).ok_or_else(|| {
                EngineError::new(
                    "missing_graph_reference",
                    format!("topological plan references missing {dependent}"),
                )
            })?;
            *count -= 1;
            if *count == 0 {
                ready.insert((*dependent).to_owned());
            }
        }
    }
    if order.len() != nodes.len() {
        let cycle = remaining
            .into_iter()
            .filter_map(|(path, count)| (count > 0).then_some(path))
            .collect::<Vec<_>>();
        return Err(EngineError::new(
            "derivation_cycle",
            format!("derivation graph contains a cycle: {}", cycle.join(", ")),
        ));
    }
    Ok(order)
}
