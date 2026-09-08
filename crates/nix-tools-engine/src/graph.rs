use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::sync::{Arc, Mutex};

use nix_tools_core::process::StreamConsumer;
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

use crate::{DerivationNode, EngineError};

/// Validated, deduplicated derivation dependency graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DependencyGraph {
    nodes: BTreeMap<String, Arc<DerivationNode>>,
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
        let nodes = nodes
            .into_iter()
            .map(|(path, node)| (path, Arc::new(node)))
            .collect();
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
        max_retained_bytes: usize,
    ) -> Result<Self, EngineError> {
        Self::parse(
            serde_json::Deserializer::from_slice(bytes),
            roots,
            max_nodes,
            max_retained_bytes,
        )
    }

    /// Streams the same document from a reader, retaining only the nodes the graph keeps.
    ///
    /// Fields Atlas does not consume, `env` above all, are skipped by the parser rather than
    /// materialised, so peak memory follows the node count and not the bytes Nix emits.
    ///
    /// # Errors
    ///
    /// Returns a stable protocol error for malformed JSON or an invalid graph.
    pub fn from_reader<R: Read>(
        reader: R,
        roots: &BTreeSet<String>,
        max_nodes: usize,
        max_retained_bytes: usize,
    ) -> Result<Self, EngineError> {
        Self::parse(
            serde_json::Deserializer::from_reader(reader),
            roots,
            max_nodes,
            max_retained_bytes,
        )
    }

    /// Drives the streaming visitor over either source, keeping the borrowing slice fast path for
    /// callers that already hold the bytes.
    fn parse<'de, R: serde_json::de::Read<'de>>(
        mut deserializer: serde_json::Deserializer<R>,
        roots: &BTreeSet<String>,
        max_nodes: usize,
        max_retained_bytes: usize,
    ) -> Result<Self, EngineError> {
        let mut nodes = BTreeMap::new();
        let mut state = ParseState::new(max_retained_bytes);
        let parsed = (&mut deserializer).deserialize_any(DocumentVisitor {
            nodes: &mut nodes,
            max_nodes,
            failure: &mut state,
        });
        if let Err(error) = parsed.and_then(|()| deserializer.end()) {
            return Err(state.failure.unwrap_or_else(|| {
                // A failed read is not a malformed document. Collapsing the two would report a
                // truncated transport, a stream ceiling, or a cancelled child as bad JSON.
                let code = if error.is_io() {
                    "graph_stream_read_failed"
                } else {
                    "invalid_graph_json"
                };
                EngineError::new(code, format!("parse nix derivation graph JSON: {error}"))
            }));
        }
        Self::new(nodes, roots, max_nodes)
    }

    /// Returns graph nodes in derivation-path order; cloning a handle shares its immutable payload.
    #[must_use]
    pub fn nodes(&self) -> &BTreeMap<String, Arc<DerivationNode>> {
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
        self.nodes.get(drv_path).map(Arc::as_ref)
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

fn normalize_derivation_path(mut path: String) -> String {
    if path.contains('/') || path.strip_suffix(".drv").is_none() {
        return path;
    }
    let hash = path.split_once('-').map(|(hash, _)| hash);
    if hash.is_some_and(|hash| hash.len() == 32) {
        path.insert_str(0, "/nix/store/");
    }
    path
}

fn normalize_output_path(mut path: String) -> String {
    if path.contains('/') {
        return path;
    }
    let hash = path.split_once('-').map(|(hash, _)| hash);
    if hash.is_some_and(|hash| hash.len() == 32) {
        path.insert_str(0, "/nix/store/");
    }
    path
}

/// A retained string costs far more than its own bytes: a `String` header, a map or set slot, the
/// value beside it, and the allocator's rounding. Measured at about 113 bytes per short output
/// name in a `BTreeMap<String, Option<String>>`, so the charge is rounded up from there and the
/// budget approximates resident bytes rather than understating them.
const RETAINED_ENTRY_OVERHEAD_BYTES: usize = 128;

/// Extra charge for an entry that also allocates a container of its own.
///
/// Each dependency owns a `BTreeSet` of the outputs it selects, and a set holding one name still
/// costs a whole B-tree node. Measured at about 390 bytes on top of the entry itself, so a
/// dependency-heavy graph is charged what it actually costs instead of a fifth of it.
const RETAINED_CONTAINER_OVERHEAD_BYTES: usize = 384;

/// Ceiling on one retained name or path.
///
/// A store path is a couple of hundred bytes, so this is far above anything Nix emits. It exists
/// because these strings are quoted back into diagnostics that are persisted in a manifest: without
/// it a single attacker-chosen output name of arbitrary length reaches a stored record.
const MAX_RETAINED_STRING_BYTES: usize = 4096;

/// Parse state shared by every visitor: the first domain error, and the memory the retained graph
/// may still spend.
struct ParseState {
    failure: Option<EngineError>,
    limit_bytes: usize,
    remaining_bytes: usize,
}

impl ParseState {
    fn new(limit_bytes: usize) -> Self {
        Self {
            failure: None,
            limit_bytes,
            remaining_bytes: limit_bytes,
        }
    }

    /// Records the stable protocol error a `serde` type error would otherwise erase, then reports
    /// the same message through the deserializer so parsing stops at the first offending value.
    fn fail<E: de::Error>(&mut self, code: &'static str, message: impl Into<String>) -> E {
        let message = message.into();
        let error = E::custom(&message);
        self.failure
            .get_or_insert_with(|| EngineError::new(code, message));
        error
    }

    /// Charges one string the graph is about to retain, before it is retained.
    ///
    /// Node count alone bounds nothing: one derivation may declare unlimited outputs and unlimited
    /// inputs, so the budget is spent per entry as entries arrive rather than per completed node.
    /// Charges an entry that also allocates its own container.
    fn charge_container<E: de::Error>(&mut self, retained: &str) -> Result<(), E> {
        self.charge_with(retained, RETAINED_CONTAINER_OVERHEAD_BYTES)
    }

    fn charge<E: de::Error>(&mut self, retained: &str) -> Result<(), E> {
        self.charge_with(retained, 0)
    }

    fn charge_with<E: de::Error>(&mut self, retained: &str, extra: usize) -> Result<(), E> {
        if retained.len() > MAX_RETAINED_STRING_BYTES {
            // Deliberately reports the length rather than the value: this text ends up in a
            // diagnostic, and quoting the offending string back would defeat the ceiling.
            return Err(self.fail(
                "graph_string_limit_exceeded",
                format!(
                    "derivation graph contains a {}-byte name, exceeding the {MAX_RETAINED_STRING_BYTES}-byte limit",
                    retained.len()
                ),
            ));
        }
        let cost = retained
            .len()
            .saturating_add(RETAINED_ENTRY_OVERHEAD_BYTES)
            .saturating_add(extra);
        if let Some(remaining) = self.remaining_bytes.checked_sub(cost) {
            self.remaining_bytes = remaining;
            return Ok(());
        }
        Err(self.fail(
            "graph_memory_limit_exceeded",
            format!(
                "derivation graph retains more than the configured limit of {} bytes",
                self.limit_bytes
            ),
        ))
    }
}

/// Rejects the scalar shapes no derivation graph value ever takes.
macro_rules! reject_scalars {
    () => {
        fn visit_bool<E: de::Error>(self, _value: bool) -> Result<Self::Value, E> {
            Err(self.reject())
        }

        fn visit_i64<E: de::Error>(self, _value: i64) -> Result<Self::Value, E> {
            Err(self.reject())
        }

        fn visit_u64<E: de::Error>(self, _value: u64) -> Result<Self::Value, E> {
            Err(self.reject())
        }

        fn visit_f64<E: de::Error>(self, _value: f64) -> Result<Self::Value, E> {
            Err(self.reject())
        }
    };
}

/// Rejects an array where one is never valid.
macro_rules! reject_seq {
    () => {
        fn visit_seq<A: SeqAccess<'de>>(self, _sequence: A) -> Result<Self::Value, A::Error> {
            Err(self.reject())
        }
    };
}

/// Rejects the remaining shapes where only an object is accepted.
macro_rules! reject_text {
    () => {
        fn visit_str<E: de::Error>(self, _value: &str) -> Result<Self::Value, E> {
            Err(self.reject())
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Err(self.reject())
        }
    };
}

/// Classifies a map key against the names we consume without allocating for the ones we know.
macro_rules! key_visitor {
    (
        $name:ident,
        $value:ty,
        $expecting:literal,
        $($literal:literal => $variant:expr,)*
        $binding:ident => $fallback:expr
    ) => {
        struct $name;

        impl<'de> Visitor<'de> for $name {
            type Value = $value;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str($expecting)
            }

            fn visit_str<E: de::Error>(self, $binding: &str) -> Result<Self::Value, E> {
                Ok(match $binding {
                    $($literal => $variant,)*
                    _ => $fallback,
                })
            }
        }

        impl<'de> serde::Deserialize<'de> for $value {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserializer.deserialize_str($name)
            }
        }
    };
}

enum DocumentKey {
    Derivations,
    Version,
    Derivation(String),
}

key_visitor!(
    DocumentKeyVisitor,
    DocumentKey,
    "a derivation graph key",
    "derivations" => DocumentKey::Derivations,
    "version" => DocumentKey::Version,
    value => DocumentKey::Derivation(value.to_owned())
);

enum NodeField {
    Outputs,
    Inputs,
    InputDrvs,
    Other,
}

key_visitor!(
    NodeFieldVisitor,
    NodeField,
    "a derivation field",
    "outputs" => NodeField::Outputs,
    "inputs" => NodeField::Inputs,
    "inputDrvs" => NodeField::InputDrvs,
    value => NodeField::Other
);

enum DrvsField {
    Drvs,
    Other,
}

key_visitor!(
    DrvsFieldVisitor,
    DrvsField,
    "a derivation input field",
    "drvs" => DrvsField::Drvs,
    value => DrvsField::Other
);

enum PathField {
    Path,
    Other,
}

key_visitor!(
    PathFieldVisitor,
    PathField,
    "a derivation output field",
    "path" => PathField::Path,
    value => PathField::Other
);

enum OutputsField {
    Outputs,
    Other,
}

key_visitor!(
    OutputsFieldVisitor,
    OutputsField,
    "a derivation input selection field",
    "outputs" => OutputsField::Outputs,
    value => OutputsField::Other
);

type Outputs = BTreeMap<String, Option<String>>;
type Dependencies = BTreeMap<String, BTreeSet<String>>;

struct DocumentVisitor<'a> {
    nodes: &'a mut BTreeMap<String, DerivationNode>,
    max_nodes: usize,
    failure: &'a mut ParseState,
}

// Speculative legacy nodes must finish their JSON value after an error so a later wrapper can discard them.
struct DrainingSeed<'a, S> {
    seed: S,
    failed: &'a Cell<bool>,
}

impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for DrainingSeed<'_, S> {
    type Value = S::Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        self.seed.deserialize(DrainingDeserializer {
            deserializer,
            failed: self.failed,
        })
    }
}

struct DrainingDeserializer<'a, D> {
    deserializer: D,
    failed: &'a Cell<bool>,
}

impl<'de, D: Deserializer<'de>> Deserializer<'de> for DrainingDeserializer<'_, D> {
    type Error = D::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserializer.deserialize_any(DrainingVisitor {
            visitor,
            failed: self.failed,
        })
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.deserializer.deserialize_ignored_any(visitor)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier
    }
}

struct DrainingVisitor<'a, V> {
    visitor: V,
    failed: &'a Cell<bool>,
}

macro_rules! forward_scalar {
    ($method:ident, $ty:ty) => {
        fn $method<E: de::Error>(self, value: $ty) -> Result<Self::Value, E> {
            self.visitor.$method(value)
        }
    };
}

impl<'de, V: Visitor<'de>> Visitor<'de> for DrainingVisitor<'_, V> {
    type Value = V::Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.visitor.expecting(formatter)
    }

    forward_scalar!(visit_bool, bool);
    forward_scalar!(visit_i64, i64);
    forward_scalar!(visit_u64, u64);
    forward_scalar!(visit_f64, f64);
    forward_scalar!(visit_str, &str);
    forward_scalar!(visit_borrowed_str, &'de str);
    forward_scalar!(visit_string, String);

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        self.visitor.visit_unit()
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        let mut access = DrainingMap {
            map,
            pending_value: false,
            failed: self.failed,
        };
        let outcome = self.visitor.visit_map(&mut access);
        if outcome.is_err() && !self.failed.get() {
            let drained = (|| {
                if access.pending_value {
                    access.map.next_value::<IgnoredAny>()?;
                }
                while access.map.next_key::<IgnoredAny>()?.is_some() {
                    access.map.next_value::<IgnoredAny>()?;
                }
                Ok(())
            })();
            if let Err(error) = drained {
                self.failed.set(true);
                return Err(error);
            }
        }
        outcome
    }

    fn visit_seq<A: SeqAccess<'de>>(self, sequence: A) -> Result<Self::Value, A::Error> {
        let mut access = DrainingSequence {
            sequence,
            failed: self.failed,
        };
        let outcome = self.visitor.visit_seq(&mut access);
        if outcome.is_err() && !self.failed.get() {
            loop {
                match access.sequence.next_element::<IgnoredAny>() {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(error) => {
                        self.failed.set(true);
                        return Err(error);
                    }
                }
            }
        }
        outcome
    }
}

struct DrainingMap<'a, A> {
    map: A,
    pending_value: bool,
    failed: &'a Cell<bool>,
}

impl<'de, A: MapAccess<'de>> MapAccess<'de> for &mut DrainingMap<'_, A> {
    type Error = A::Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, A::Error> {
        let key = self.map.next_key_seed(seed)?;
        self.pending_value = key.is_some();
        Ok(key)
    }

    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value, A::Error> {
        self.pending_value = false;
        self.map.next_value_seed(DrainingSeed {
            seed,
            failed: self.failed,
        })
    }
}

struct DrainingSequence<'a, A> {
    sequence: A,
    failed: &'a Cell<bool>,
}

impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for &mut DrainingSequence<'_, A> {
    type Error = A::Error;

    fn next_element_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, A::Error> {
        self.sequence.next_element_seed(DrainingSeed {
            seed,
            failed: self.failed,
        })
    }
}

/// Whether the versioned wrapper has been seen, which decides what other top-level keys mean.
///
/// Without a `derivations` key the document is the legacy top-level map and every key is a
/// derivation. With one, the wrapper is the whole graph and siblings are not derivations, so they
/// are ignored rather than parsed. Streaming cannot know which form it has until the key arrives,
/// so siblings read before it are accumulated and then discarded.
#[derive(Clone, Copy, Eq, PartialEq)]
enum DocumentForm {
    Legacy,
    Wrapped,
}

impl DocumentVisitor<'_> {
    fn reject<E: de::Error>(self) -> E {
        self.failure.fail(
            "invalid_graph_schema",
            "nix derivation graph must be a JSON object",
        )
    }
}

impl<'de> Visitor<'de> for DocumentVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a nix derivation graph object")
    }

    reject_scalars!();
    reject_seq!();
    reject_text!();

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let Self {
            nodes,
            max_nodes,
            failure,
        } = self;
        let mut form = DocumentForm::Legacy;
        let mut deferred = None;
        while let Some(key) = map.next_key::<DocumentKey>()? {
            match key {
                DocumentKey::Version => {
                    map.next_value::<IgnoredAny>()?;
                }
                DocumentKey::Derivations => {
                    // A repeated wrapper key keeps the last map, as reading the whole document
                    // into one object did.
                    nodes.clear();
                    *failure = ParseState::new(failure.limit_bytes);
                    deferred = None;
                    form = DocumentForm::Wrapped;
                    map.next_value_seed(DerivationsVisitor {
                        nodes: &mut *nodes,
                        max_nodes,
                        failure: &mut *failure,
                    })?;
                }
                DocumentKey::Derivation(raw_path) => match form {
                    DocumentForm::Wrapped => {
                        map.next_value::<IgnoredAny>()?;
                    }
                    DocumentForm::Legacy => {
                        if deferred.is_some() {
                            map.next_value::<IgnoredAny>()?;
                        } else if let Err(error) = insert_node(
                            &mut *nodes,
                            max_nodes,
                            &mut *failure,
                            &mut map,
                            raw_path,
                            true,
                        ) {
                            let Some(domain_error) = failure.failure.take() else {
                                return Err(error);
                            };
                            deferred = Some(domain_error);
                        }
                    }
                },
            }
        }
        if let Some(error) = deferred {
            let parse_error = de::Error::custom(error.message());
            failure.failure = Some(error);
            return Err(parse_error);
        }
        Ok(())
    }
}

struct DerivationsVisitor<'a> {
    nodes: &'a mut BTreeMap<String, DerivationNode>,
    max_nodes: usize,
    failure: &'a mut ParseState,
}

impl DerivationsVisitor<'_> {
    fn reject<E: de::Error>(self) -> E {
        self.failure
            .fail("invalid_graph_schema", "derivations must be a JSON object")
    }
}

impl<'de> DeserializeSeed<'de> for DerivationsVisitor<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for DerivationsVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a derivation map")
    }

    reject_scalars!();
    reject_seq!();
    reject_text!();

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let Self {
            nodes,
            max_nodes,
            failure,
        } = self;
        while let Some(raw_path) = map.next_key::<String>()? {
            if raw_path == "version" {
                map.next_value::<IgnoredAny>()?;
                continue;
            }
            insert_node(
                &mut *nodes,
                max_nodes,
                &mut *failure,
                &mut map,
                raw_path,
                false,
            )?;
        }
        Ok(())
    }
}

/// Builds one node from the value following its key, bailing as soon as the graph outgrows its
/// limit rather than after the whole document has been materialised.
fn insert_node<'de, A: MapAccess<'de>>(
    nodes: &mut BTreeMap<String, DerivationNode>,
    max_nodes: usize,
    failure: &mut ParseState,
    map: &mut A,
    raw_path: String,
    speculative: bool,
) -> Result<(), A::Error> {
    let drv_path = normalize_derivation_path(raw_path);
    // The graph keeps the path twice, as the map key and inside the node.
    if let Err(error) = failure
        .charge(&drv_path)
        .and_then(|()| failure.charge(&drv_path))
    {
        if speculative && let Err(read_error) = map.next_value::<IgnoredAny>() {
            failure.failure = None;
            return Err(read_error);
        }
        return Err(error);
    }
    let visitor = NodeVisitor {
        drv_path: &drv_path,
        failure: &mut *failure,
    };
    let (dependencies, outputs) = if speculative {
        let failed = Cell::new(false);
        let result = map.next_value_seed(DrainingSeed {
            seed: visitor,
            failed: &failed,
        });
        if failed.get() {
            failure.failure = None;
        }
        result?
    } else {
        map.next_value_seed(visitor)?
    };
    nodes.insert(
        drv_path.clone(),
        DerivationNode {
            drv_path,
            dependencies,
            outputs,
        },
    );
    if nodes.len() > max_nodes {
        return Err(failure.fail(
            "graph_node_limit_exceeded",
            format!("derivation graph exceeds the configured limit of {max_nodes}"),
        ));
    }
    Ok(())
}

struct NodeVisitor<'a> {
    drv_path: &'a str,
    failure: &'a mut ParseState,
}

impl NodeVisitor<'_> {
    fn reject<E: de::Error>(self) -> E {
        self.failure.fail(
            "invalid_graph_node",
            format!("derivation {} must be an object", self.drv_path),
        )
    }
}

impl<'de> DeserializeSeed<'de> for NodeVisitor<'_> {
    type Value = (Dependencies, Outputs);

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for NodeVisitor<'_> {
    type Value = (Dependencies, Outputs);

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a derivation object")
    }

    reject_scalars!();
    reject_seq!();
    reject_text!();

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let Self { drv_path, failure } = self;
        let mut outputs = None;
        let mut dependencies = None;
        let mut legacy_dependencies = None;
        while let Some(field) = map.next_key::<NodeField>()? {
            match field {
                NodeField::Outputs => {
                    outputs = Some(map.next_value_seed(OutputsVisitor {
                        drv_path,
                        failure: &mut *failure,
                    })?);
                }
                NodeField::Inputs => {
                    dependencies = map.next_value_seed(InputsVisitor {
                        drv_path,
                        failure: &mut *failure,
                    })?;
                }
                NodeField::InputDrvs => {
                    legacy_dependencies = Some(map.next_value_seed(DrvsVisitor {
                        drv_path,
                        failure: &mut *failure,
                    })?);
                }
                NodeField::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        let Some(outputs) = outputs else {
            return Err(failure.fail(
                "invalid_graph_outputs",
                format!("derivation {drv_path} outputs must be an object"),
            ));
        };
        Ok((
            dependencies.or(legacy_dependencies).unwrap_or_default(),
            outputs,
        ))
    }
}

struct InputsVisitor<'a> {
    drv_path: &'a str,
    failure: &'a mut ParseState,
}

impl InputsVisitor<'_> {
    fn reject<E: de::Error>(self) -> E {
        self.failure.fail(
            "invalid_input_derivations",
            format!("derivation {} inputs must be an object", self.drv_path),
        )
    }
}

impl<'de> DeserializeSeed<'de> for InputsVisitor<'_> {
    type Value = Option<Dependencies>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for InputsVisitor<'_> {
    type Value = Option<Dependencies>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a derivation inputs object")
    }

    reject_scalars!();
    reject_seq!();
    reject_text!();

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let Self { drv_path, failure } = self;
        let mut drvs = None;
        while let Some(field) = map.next_key::<DrvsField>()? {
            match field {
                DrvsField::Drvs => {
                    drvs = Some(map.next_value_seed(DrvsVisitor {
                        drv_path,
                        failure: &mut *failure,
                    })?);
                }
                DrvsField::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(drvs)
    }
}

struct DrvsVisitor<'a> {
    drv_path: &'a str,
    failure: &'a mut ParseState,
}

impl DrvsVisitor<'_> {
    fn reject<E: de::Error>(self) -> E {
        self.failure.fail(
            "invalid_input_derivations",
            format!(
                "derivation {} input derivations must be an object",
                self.drv_path
            ),
        )
    }
}

impl<'de> DeserializeSeed<'de> for DrvsVisitor<'_> {
    type Value = Dependencies;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for DrvsVisitor<'_> {
    type Value = Dependencies;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a derivation input map")
    }

    reject_scalars!();
    reject_seq!();
    reject_text!();

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let Self { drv_path, failure } = self;
        let mut dependencies = Dependencies::new();
        while let Some(raw_dependency) = map.next_key::<String>()? {
            let dependency = normalize_derivation_path(raw_dependency);
            failure.charge_container(&dependency)?;
            let outputs = map.next_value_seed(SelectionVisitor {
                drv_path,
                dependency: &dependency,
                failure: &mut *failure,
            })?;
            if outputs.is_empty() {
                return Err(failure.fail(
                    "empty_input_output_selection",
                    format!("derivation {drv_path} selects no outputs from {dependency}"),
                ));
            }
            dependencies.insert(dependency, outputs);
        }
        Ok(dependencies)
    }
}

struct SelectionVisitor<'a> {
    drv_path: &'a str,
    dependency: &'a str,
    failure: &'a mut ParseState,
}

impl SelectionVisitor<'_> {
    fn reject<E: de::Error>(self) -> E {
        self.failure.fail(
            "invalid_input_outputs",
            format!(
                "derivation {} input {} outputs must be an array",
                self.drv_path, self.dependency
            ),
        )
    }
}

impl<'de> DeserializeSeed<'de> for SelectionVisitor<'_> {
    type Value = BTreeSet<String>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for SelectionVisitor<'_> {
    type Value = BTreeSet<String>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an output name array or an object selecting output names")
    }

    reject_scalars!();
    reject_text!();

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        let Self {
            drv_path,
            dependency,
            failure,
        } = self;
        collect_output_names(&mut sequence, drv_path, dependency, failure)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let Self {
            drv_path,
            dependency,
            failure,
        } = self;
        let mut outputs = None;
        while let Some(field) = map.next_key::<OutputsField>()? {
            match field {
                OutputsField::Outputs => {
                    outputs = Some(map.next_value_seed(SelectionNamesVisitor {
                        drv_path,
                        dependency,
                        failure: &mut *failure,
                    })?);
                }
                OutputsField::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        outputs.ok_or_else(|| {
            failure.fail(
                "invalid_input_outputs",
                format!("derivation {drv_path} input {dependency} outputs must be an array"),
            )
        })
    }
}

fn collect_output_names<'de, A: SeqAccess<'de>>(
    sequence: &mut A,
    drv_path: &str,
    dependency: &str,
    failure: &mut ParseState,
) -> Result<BTreeSet<String>, A::Error> {
    let mut outputs = BTreeSet::new();
    while let Some(output) = sequence.next_element_seed(OutputNameVisitor {
        drv_path,
        dependency,
        failure: &mut *failure,
    })? {
        outputs.insert(output);
    }
    Ok(outputs)
}

struct SelectionNamesVisitor<'a> {
    drv_path: &'a str,
    dependency: &'a str,
    failure: &'a mut ParseState,
}

impl SelectionNamesVisitor<'_> {
    fn reject<E: de::Error>(self) -> E {
        self.failure.fail(
            "invalid_input_outputs",
            format!(
                "derivation {} input {} outputs must be an array",
                self.drv_path, self.dependency
            ),
        )
    }
}

impl<'de> DeserializeSeed<'de> for SelectionNamesVisitor<'_> {
    type Value = BTreeSet<String>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for SelectionNamesVisitor<'_> {
    type Value = BTreeSet<String>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an output name array")
    }

    reject_scalars!();
    reject_text!();

    fn visit_map<A: MapAccess<'de>>(self, _map: A) -> Result<Self::Value, A::Error> {
        Err(self.reject())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        let Self {
            drv_path,
            dependency,
            failure,
        } = self;
        collect_output_names(&mut sequence, drv_path, dependency, failure)
    }
}

struct OutputNameVisitor<'a> {
    drv_path: &'a str,
    dependency: &'a str,
    failure: &'a mut ParseState,
}

impl OutputNameVisitor<'_> {
    fn reject<E: de::Error>(self) -> E {
        self.failure.fail(
            "invalid_input_output_name",
            format!(
                "derivation {} input {} output names must be non-empty strings",
                self.drv_path, self.dependency
            ),
        )
    }
}

impl<'de> DeserializeSeed<'de> for OutputNameVisitor<'_> {
    type Value = String;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for OutputNameVisitor<'_> {
    type Value = String;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a non-empty output name")
    }

    reject_scalars!();
    reject_seq!();

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Err(self.reject())
    }

    fn visit_map<A: MapAccess<'de>>(self, _map: A) -> Result<Self::Value, A::Error> {
        Err(self.reject())
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        if value.is_empty() {
            return Err(self.reject());
        }
        self.failure.charge(value)?;
        Ok(value.to_owned())
    }
}

struct OutputsVisitor<'a> {
    drv_path: &'a str,
    failure: &'a mut ParseState,
}

impl OutputsVisitor<'_> {
    fn reject<E: de::Error>(self) -> E {
        self.failure.fail(
            "invalid_graph_outputs",
            format!("derivation {} outputs must be an object", self.drv_path),
        )
    }
}

impl<'de> DeserializeSeed<'de> for OutputsVisitor<'_> {
    type Value = Outputs;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for OutputsVisitor<'_> {
    type Value = Outputs;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a derivation outputs object")
    }

    reject_scalars!();
    reject_seq!();
    reject_text!();

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let Self { drv_path, failure } = self;
        let mut outputs = Outputs::new();
        while let Some(name) = map.next_key::<String>()? {
            if name.is_empty() {
                return Err(failure.fail(
                    "invalid_output_name",
                    format!("derivation {drv_path} has an empty output name"),
                ));
            }
            failure.charge(&name)?;
            let path = map.next_value_seed(OutputVisitor {
                drv_path,
                name: &name,
                failure: &mut *failure,
            })?;
            if let Some(path) = &path {
                failure.charge(path)?;
            }
            outputs.insert(name, path);
        }
        Ok(outputs)
    }
}

struct OutputVisitor<'a> {
    drv_path: &'a str,
    name: &'a str,
    failure: &'a mut ParseState,
}

impl OutputVisitor<'_> {
    fn reject<E: de::Error>(self) -> E {
        self.failure.fail(
            "invalid_graph_output",
            format!(
                "derivation {} output {} must be an object",
                self.drv_path, self.name
            ),
        )
    }
}

impl<'de> DeserializeSeed<'de> for OutputVisitor<'_> {
    type Value = Option<String>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for OutputVisitor<'_> {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an output path, null, or an output object")
    }

    reject_scalars!();
    reject_seq!();

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Some(normalize_output_path(value.to_owned())))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let Self {
            drv_path,
            name,
            failure,
        } = self;
        let mut path = None;
        while let Some(field) = map.next_key::<PathField>()? {
            match field {
                PathField::Path => {
                    path = map.next_value_seed(OutputPathVisitor {
                        drv_path,
                        name,
                        failure: &mut *failure,
                    })?;
                }
                PathField::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(path)
    }
}

struct OutputPathVisitor<'a> {
    drv_path: &'a str,
    name: &'a str,
    failure: &'a mut ParseState,
}

impl OutputPathVisitor<'_> {
    fn reject<E: de::Error>(self) -> E {
        self.failure.fail(
            "invalid_graph_output_path",
            format!(
                "derivation {} output {} path must be a string",
                self.drv_path, self.name
            ),
        )
    }
}

impl<'de> DeserializeSeed<'de> for OutputPathVisitor<'_> {
    type Value = Option<String>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for OutputPathVisitor<'_> {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an output path or null")
    }

    reject_scalars!();
    reject_seq!();

    fn visit_map<A: MapAccess<'de>>(self, _map: A) -> Result<Self::Value, A::Error> {
        Err(self.reject())
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Some(normalize_output_path(value.to_owned())))
    }
}

/// Parses one `nix derivation show` stream into a graph while the child writing it still runs.
pub(crate) struct GraphStream {
    roots: BTreeSet<String>,
    max_nodes: usize,
    max_retained_bytes: usize,
    outcome: Mutex<Option<Result<DependencyGraph, EngineError>>>,
}

impl GraphStream {
    pub(crate) fn new(
        roots: BTreeSet<String>,
        max_nodes: usize,
        max_retained_bytes: usize,
    ) -> Self {
        Self {
            roots,
            max_nodes,
            max_retained_bytes,
            outcome: Mutex::new(None),
        }
    }

    /// Returns the parsed graph, or why the stream did not produce one.
    pub(crate) fn take(&self) -> Result<DependencyGraph, EngineError> {
        self.outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| {
                Err(EngineError::new(
                    "invalid_graph_json",
                    "nix derivation show produced no derivation graph",
                ))
            })
    }
}

impl StreamConsumer for GraphStream {
    fn consume(&self, reader: &mut dyn Read) -> std::io::Result<()> {
        let parsed = DependencyGraph::from_reader(
            reader,
            &self.roots,
            self.max_nodes,
            self.max_retained_bytes,
        );
        // A read that failed is the runner's to report, so that "the stream broke" and "the
        // document was malformed" stay distinguishable in the diagnostic.
        if let Err(error) = &parsed
            && error.code() == "graph_stream_read_failed"
        {
            return Err(std::io::Error::other(error.message().to_owned()));
        }
        *self
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(parsed);
        Ok(())
    }
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
