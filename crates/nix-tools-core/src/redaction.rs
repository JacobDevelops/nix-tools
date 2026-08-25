//! Concurrent secret registration and redaction for raw and rendered process output.

use std::os::unix::ffi::OsStrExt;
use std::sync::{Arc, RwLock};

use crate::terminal::normalize_terminal_output;

const REDACTED: &[u8] = b"[REDACTED]";
const AUTHORIZATION: &str = "AUTHORIZATION";
const ASSIGNMENT_VALUE_LOOKAHEAD: usize = 64;

const SENSITIVE_KEYS: &[&str] = &[
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "PASSWORD",
    "PASSPHRASE",
    "SECRET",
    "TOKEN",
    "CREDENTIAL",
    AUTHORIZATION,
];

fn byte_offsets<'a>(haystack: &'a [u8], needle: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    // `windows(0)` panics, and an empty needle must simply never match.
    let width = needle.len().max(1);
    haystack
        .windows(width)
        .enumerate()
        .filter_map(move |(start, window)| (window == needle).then_some(start))
}

fn find_from(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let tail = haystack.get(from..)?;
    byte_offsets(tail, needle)
        .next()
        .map(|offset| from + offset)
}

fn replace_all(input: &[u8], pattern: &[u8], replacement: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(start) = find_from(input, pattern, cursor) {
        output.extend_from_slice(&input[cursor..start]);
        output.extend_from_slice(replacement);
        cursor = start + pattern.len();
    }
    output.extend_from_slice(&input[cursor..]);
    output
}

/// A frame end of zero emits nothing and re-buffers the same bytes forever, stalling the stream.
const fn nonzero_frame_end(start: usize, fallback: usize) -> usize {
    if start == 0 { fallback } else { start }
}

/// Thread-safe registry that redacts literal, encoded, and assignment-shaped secrets.
///
/// `Debug` never exposes registered values. Clones share the same registry so values discovered by
/// one process invocation protect all output relayed through the clone set.
#[derive(Clone, Default)]
pub struct Redactor {
    secrets: Arc<RwLock<Vec<Vec<u8>>>>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Redactor")
            .field("secrets", &"[REDACTED]")
            .finish()
    }
}

impl Redactor {
    /// Registers a non-empty secret and common URL/base64/rendered variants.
    pub fn register(&self, secret: impl AsRef<[u8]>) {
        let secret = secret.as_ref();
        if secret.is_empty() {
            return;
        }
        let mut variants = secret_variants(secret);
        let normalized = normalize_terminal_output(secret);
        if normalized != secret {
            variants.extend(secret_variants(&normalized));
        }
        // Frames reach `redact` after a lossy UTF-8 render, which rewrites every invalid byte.
        let rendered = String::from_utf8_lossy(secret);
        if rendered.as_bytes() != secret {
            variants.extend(secret_variants(rendered.as_bytes()));
        }

        let mut secrets = self
            .secrets
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for variant in variants {
            if !variant.is_empty() && !secrets.contains(&variant) {
                secrets.push(variant);
            }
        }
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    }

    pub(crate) fn safe_frame_end(
        &self,
        input: &[u8],
        candidate: usize,
        end_of_stream: bool,
    ) -> Option<usize> {
        let secrets = self
            .secrets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let secret_lookahead = secrets
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or_default()
            .saturating_sub(1);
        let assignment_lookahead = SENSITIVE_KEYS
            .iter()
            .map(|key| key.len() + ASSIGNMENT_VALUE_LOOKAHEAD)
            .max()
            .unwrap_or_default();
        let lookahead = secret_lookahead.max(assignment_lookahead);
        let line_terminated = candidate > 0 && input.get(candidate - 1) == Some(&b'\n');
        if !end_of_stream && !line_terminated && input.len() < candidate.saturating_add(lookahead) {
            return None;
        }
        for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
            let bytes = secret.as_slice();
            for (start, window) in input.windows(bytes.len()).enumerate() {
                if window == bytes && start < candidate && start + bytes.len() > candidate {
                    return Some(nonzero_frame_end(start, start + bytes.len()));
                }
            }
            if line_terminated && !end_of_stream {
                let possible_overlap = bytes.len().saturating_sub(1).min(candidate);
                if (1..=possible_overlap)
                    .any(|length| input[candidate - length..candidate] == bytes[..length])
                {
                    return None;
                }
            }
        }
        drop(secrets);
        if line_terminated {
            return Some(candidate);
        }

        let uppercase = input.to_ascii_uppercase();
        for key in SENSITIVE_KEYS {
            for start in byte_offsets(&uppercase, key.as_bytes()) {
                if start < candidate
                    && start.saturating_add(key.len() + ASSIGNMENT_VALUE_LOOKAHEAD) > candidate
                {
                    return Some(nonzero_frame_end(start, candidate));
                }
            }
        }
        Some(candidate)
    }

    pub(crate) fn contains_sensitive_assignment(input: &[u8]) -> bool {
        let uppercase = input.to_ascii_uppercase();
        SENSITIVE_KEYS.iter().any(|key| {
            byte_offsets(&uppercase, key.as_bytes())
                .any(|start| assignment_separator(input, start + key.len()).is_some())
        })
    }

    pub(crate) fn contains_sensitive_assignment_prefix(input: &[u8]) -> bool {
        let uppercase = input.to_ascii_uppercase();
        SENSITIVE_KEYS.iter().any(|key| {
            byte_offsets(&uppercase, key.as_bytes()).any(|start| {
                let mut suffix = &input[start + key.len()..];
                if suffix
                    .first()
                    .is_some_and(|byte| matches!(byte, b'\"' | b'\''))
                {
                    suffix = &suffix[1..];
                }
                !suffix.is_empty() && suffix.iter().all(|byte| matches!(byte, b' ' | b'\t'))
            })
        })
    }

    /// Registers values whose environment keys look credential-bearing.
    ///
    /// Matching is case-insensitive and recognizes common password, passphrase, secret, token,
    /// credential, authorization, and AWS session key fragments.
    pub fn register_sensitive_environment(
        &self,
        environment: &std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    ) {
        for (key, value) in environment {
            let key = key.as_bytes().to_ascii_uppercase();
            if SENSITIVE_KEYS
                .iter()
                .any(|sensitive| byte_offsets(&key, sensitive.as_bytes()).next().is_some())
            {
                self.register(value.as_bytes());
            }
        }
    }

    /// Redacts registered values and sensitive assignments in UTF-8 text.
    #[must_use]
    pub fn redact(&self, input: &str) -> String {
        String::from_utf8_lossy(&self.redact_bytes(input.as_bytes())).into_owned()
    }

    /// Redacts raw bytes, so a secret that is not valid UTF-8 still matches what the child printed.
    #[must_use]
    pub fn redact_bytes(&self, input: &[u8]) -> Vec<u8> {
        let secrets = self
            .secrets
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut redacted = redact_assignments(input, &secrets);
        for secret in secrets.iter() {
            redacted = replace_all(&redacted, secret, REDACTED);
        }
        redacted
    }
}

fn secret_variants(secret: &[u8]) -> Vec<Vec<u8>> {
    if secret.is_empty() {
        return Vec::new();
    }
    let encoded_upper = percent_encode(secret, b"0123456789ABCDEF");
    let encoded_lower = percent_encode(secret, b"0123456789abcdef");
    let base64 = base64_encode(secret);
    let base64_url: Vec<u8> = base64
        .iter()
        .map(|byte| match byte {
            b'+' => b'-',
            b'/' => b'_',
            byte => *byte,
        })
        .collect();
    vec![
        secret.to_vec(),
        trim_base64_padding(&base64).to_vec(),
        base64,
        trim_base64_padding(&base64_url).to_vec(),
        base64_url,
        encoded_upper,
        encoded_lower,
    ]
}

fn trim_base64_padding(encoded: &[u8]) -> &[u8] {
    let end = encoded
        .iter()
        .rposition(|byte| *byte != b'=')
        .map_or(0, |index| index + 1);
    &encoded[..end]
}

fn percent_encode(input: &[u8], hex: &[u8; 16]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(input.len());
    for &byte in input {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            encoded.push(byte);
        } else {
            encoded.push(b'%');
            encoded.push(hex[usize::from(byte >> 4)]);
            encoded.push(hex[usize::from(byte & 0x0f)]);
        }
    }
    encoded
}

fn base64_encode(input: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let symbol = |value: u32, shift: u32| {
        let index = u8::try_from((value >> shift) & 0x3f_u32).expect("base64 index is six bits");
        ALPHABET[usize::from(index)]
    };
    let mut encoded = Vec::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in &mut chunks {
        let value = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        for shift in [18, 12, 6, 0] {
            encoded.push(symbol(value, shift));
        }
    }
    match chunks.remainder() {
        [first] => {
            let value = u32::from(*first) << 16;
            encoded.push(symbol(value, 18));
            encoded.push(symbol(value, 12));
            encoded.extend_from_slice(b"==");
        }
        [first, second] => {
            let value = (u32::from(*first) << 16) | (u32::from(*second) << 8);
            encoded.push(symbol(value, 18));
            encoded.push(symbol(value, 12));
            encoded.push(symbol(value, 6));
            encoded.push(b'=');
        }
        [] => {}
        _ => unreachable!(),
    }
    encoded
}

fn redact_assignments(input: &[u8], secrets: &[Vec<u8>]) -> Vec<u8> {
    let mut output = input.to_vec();
    for key in SENSITIVE_KEYS {
        output = redact_key(&output, key, secrets);
    }
    output
}

fn assignment_separator(input: &[u8], after_key: usize) -> Option<usize> {
    let mut separator = after_key;
    if input
        .get(separator)
        .is_some_and(|byte| matches!(byte, b'\"' | b'\''))
    {
        separator += 1;
    }
    while input
        .get(separator)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        separator += 1;
    }
    input
        .get(separator)
        .is_some_and(|byte| matches!(byte, b'=' | b':'))
        .then_some(separator)
}

fn redact_key(input: &[u8], key: &str, secrets: &[Vec<u8>]) -> Vec<u8> {
    let uppercase = input.to_ascii_uppercase();
    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0;

    while let Some(start) = find_from(&uppercase, key.as_bytes(), cursor) {
        let after_key = start + key.len();
        let Some(separator) = assignment_separator(input, after_key) else {
            output.extend_from_slice(&input[cursor..after_key]);
            cursor = after_key;
            continue;
        };
        let mut value_start = separator + 1;
        while value_start < input.len() && matches!(input[value_start], b' ' | b'\t') {
            value_start += 1;
        }
        let quote = input
            .get(value_start)
            .copied()
            .filter(|byte| matches!(byte, b'\"' | b'\''));
        if quote.is_some() {
            value_start += 1;
        }
        let mut end = value_start;
        let mut escaped = false;
        while end < input.len()
            && match quote {
                Some(quote) => {
                    let terminator = input[end] == quote && !escaped;
                    escaped = input[end] == b'\\' && !escaped;
                    !terminator
                }
                // A `Bearer <token>` value contains a space, so space cannot terminate it.
                None if key == AUTHORIZATION => !matches!(input[end], b'\r' | b'\n' | b',' | b';'),
                None => !matches!(input[end], b' ' | b'\t' | b'\r' | b'\n' | b',' | b';'),
            }
        {
            end += 1;
        }
        if quote.is_none()
            && let Some(secret) = secrets
                .iter()
                .find(|secret| input[value_start..].starts_with(secret))
        {
            end = end.max(value_start + secret.len());
        }
        output.extend_from_slice(&input[cursor..value_start]);
        output.extend_from_slice(REDACTED);
        cursor = end;
    }
    output.extend_from_slice(&input[cursor..]);
    output
}

#[cfg(test)]
#[path = "redaction_test.rs"]
mod redaction_test;
