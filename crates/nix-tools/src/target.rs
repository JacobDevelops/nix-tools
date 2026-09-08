//! Shared service selectors for repository CLIs and `mkServiceTargets` outputs.
//!
//! ```
//! use nix_tools::{CheckSelector, ServiceCheckSelector, ServiceTarget};
//!
//! let target: ServiceTarget = "web:dev".parse()?;
//! assert_eq!(target.output_name(), "web:dev");
//! let checks = vec!["web:lint".to_owned(), "web:typecheck".to_owned()];
//! assert_eq!(ServiceCheckSelector.select("web", &checks)?, checks);
//! # Ok::<(), nix_tools_core::outcome::Error>(())
//! ```

use std::{fmt, str::FromStr};

use nix_tools_core::outcome::{Error, Result};

use crate::CheckSelector;

/// A validated `service:job` selector, also the exact standard Nix output name.
/// Components start with an ASCII letter or digit and contain only letters, digits, `_`, `.`, or `-`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceTarget {
    service: String,
    job: String,
}

impl ServiceTarget {
    /// Service that owns this target.
    #[must_use]
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Named job within the service.
    #[must_use]
    pub fn job(&self) -> &str {
        &self.job
    }

    /// Exact attribute name beneath `apps.<system>` or `checks.<system>`.
    #[must_use]
    pub fn output_name(&self) -> String {
        self.to_string()
    }
}

impl FromStr for ServiceTarget {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        let (service, job) = value
            .split_once(':')
            .ok_or_else(|| Error::usage("target must be service:job (for example web:dev)"))?;
        validate_component(service)?;
        validate_component(job)?;
        Ok(Self {
            service: service.to_owned(),
            job: job.to_owned(),
        })
    }
}

impl fmt::Display for ServiceTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.service, self.job)
    }
}

fn validate_component(value: &str) -> Result<()> {
    if value
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    {
        Ok(())
    } else {
        Err(Error::usage(format!(
            "invalid service or target name {value:?}: use an ASCII letter or digit followed by letters, digits, '_', '.', or '-'"
        )))
    }
}

/// Selects all `service:*` checks or one exact `service:check`, sorted and deduplicated.
/// Unknown selections fail; they never expand to all checks. Legacy hyphen names are not selected.
#[derive(Clone, Copy, Debug, Default)]
pub struct ServiceCheckSelector;

impl CheckSelector for ServiceCheckSelector {
    fn select(&self, scope: &str, checks: &[String]) -> Result<Vec<String>> {
        let exact = if scope.contains(':') {
            Some(scope.parse::<ServiceTarget>()?.output_name())
        } else {
            validate_component(scope)?;
            None
        };
        let prefix = format!("{scope}:");
        let mut selected: Vec<String> = checks
            .iter()
            .filter(|name| {
                exact.as_ref().map_or_else(
                    || {
                        name.strip_prefix(&prefix)
                            .is_some_and(|job| validate_component(job).is_ok())
                    },
                    |exact| *name == exact,
                )
            })
            .cloned()
            .collect();
        selected.sort_unstable();
        selected.dedup();
        if selected.is_empty() {
            return Err(Error::not_found(format!(
                "no checks match selector {scope}"
            )));
        }
        Ok(selected)
    }
}
