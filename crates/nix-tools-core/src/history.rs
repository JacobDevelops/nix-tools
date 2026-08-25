//! Schema-neutral timing history for schedule predictions.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A measured target cost keyed by an identity that should survive content changes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRecord {
    /// Stable lookup key.
    pub key: String,
    /// Optional concrete object measured for this record, such as a derivation path.
    pub source: Option<String>,
    /// Observed duration in milliseconds.
    pub duration_ms: u64,
    /// Optional observed size in bytes.
    pub size_bytes: Option<u64>,
    /// Recording time as Unix epoch milliseconds.
    pub recorded_at_ms: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HistoryDocument {
    schema: String,
    records: BTreeMap<String, HistoryRecord>,
}

/// Ordered in-memory timing history.
///
/// The repository supplies both the serialized schema and maximum record age; this type owns no
/// artifact name, cache location, or retention policy.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct History {
    records: BTreeMap<String, HistoryRecord>,
}

impl History {
    /// Parses a history document only when it matches `expected_schema` and every map key agrees
    /// with its embedded record key. Invalid or foreign documents are treated as absent.
    #[must_use]
    pub fn from_slice(bytes: &[u8], expected_schema: &str) -> Option<Self> {
        let document: HistoryDocument = serde_json::from_slice(bytes).ok()?;
        if document.schema != expected_schema
            || document
                .records
                .iter()
                .any(|(key, record)| key != &record.key)
        {
            return None;
        }
        Some(Self {
            records: document.records,
        })
    }

    /// Returns a record no older than `max_age_ms` according to the caller's clock and policy.
    #[must_use]
    pub fn lookup(&self, key: &str, now_ms: u64, max_age_ms: u64) -> Option<&HistoryRecord> {
        let record = self.records.get(key)?;
        (now_ms.saturating_sub(record.recorded_at_ms) <= max_age_ms).then_some(record)
    }

    /// Inserts or replaces one measurement.
    pub fn record(
        &mut self,
        key: &str,
        source: Option<&str>,
        duration_ms: u64,
        size_bytes: Option<u64>,
        now_ms: u64,
    ) {
        self.records.insert(
            key.to_owned(),
            HistoryRecord {
                key: key.to_owned(),
                source: source.map(str::to_owned),
                duration_ms,
                size_bytes,
                recorded_at_ms: now_ms,
            },
        );
    }

    /// Removes records older than the caller's retention policy.
    pub fn forget_stale(&mut self, now_ms: u64, max_age_ms: u64) {
        self.records
            .retain(|_, record| now_ms.saturating_sub(record.recorded_at_ms) <= max_age_ms);
    }

    /// Returns the number of stored records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Returns whether the history contains no records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Serializes records under a schema selected by the repository.
    ///
    /// # Errors
    ///
    /// Returns an error when JSON serialization fails.
    pub fn to_bytes(&self, schema: &str) -> Result<Vec<u8>, serde_json::Error> {
        let document = HistoryDocument {
            schema: schema.to_owned(),
            records: self.records.clone(),
        };
        let mut bytes = serde_json::to_vec_pretty(&document)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

/// Creates a stable key for a repository root selected by namespace and name.
#[must_use]
pub fn target_key(namespace: &str, name: &str) -> String {
    format!("{namespace}.{name}")
}

/// Creates a content-stable key from a Nix derivation path's store name.
#[must_use]
pub fn derivation_key(derivation_path: &str) -> String {
    let name = derivation_path
        .rsplit('/')
        .next()
        .unwrap_or(derivation_path);
    let name = name.strip_suffix(".drv").unwrap_or(name);
    let class = name
        .split_once('-')
        .map_or(name, |(_hash, remainder)| remainder);
    format!("derivation.{class}")
}

#[cfg(test)]
#[path = "history_test.rs"]
mod history_test;
