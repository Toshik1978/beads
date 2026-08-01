//! ID generation for issues.
//!
//! Implements classic bd ID format: `<prefix>-<hash>` where hash is
//! base36 lowercase (0-9, a-z) with adaptive length based on DB size.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

pub const MAX_ID_PREFIX_LEN: usize = 64;
pub const MAX_ID_HASH_LEN: usize = 40;
pub const MAX_ID_LENGTH: usize = MAX_ID_PREFIX_LEN + 1 + MAX_ID_HASH_LEN;
const FALLBACK_HASH_LENGTH: usize = 12;
const MAX_FALLBACK_NONCE: u32 = 2000;

/// Default ID generation configuration.
#[derive(Debug, Clone)]
pub struct IdConfig {
    /// Issue ID prefix (e.g., "bd", "`beads`").
    pub prefix: String,
    /// Minimum hash length.
    pub min_hash_length: usize,
    /// Maximum hash length.
    pub max_hash_length: usize,
    /// Maximum collision probability before increasing length.
    pub max_collision_prob: f64,
}

impl Default for IdConfig {
    fn default() -> Self {
        Self {
            prefix: "br".to_string(),
            min_hash_length: 3,
            max_hash_length: 8,
            max_collision_prob: 0.25,
        }
    }
}

impl IdConfig {
    /// Create a new ID config with the given prefix.
    ///
    /// # Errors
    ///
    /// Returns a configuration error if the prefix contains no usable
    /// characters after normalization.
    pub fn with_prefix(prefix: impl Into<String>) -> Result<Self> {
        Ok(Self {
            prefix: normalize_configured_prefix(&prefix.into())?,
            ..Default::default()
        })
    }
}

/// ID generator that produces unique issue IDs.
#[derive(Debug, Clone)]
pub struct IdGenerator {
    config: IdConfig,
}

impl IdGenerator {
    /// Create a new ID generator with the given config.
    #[must_use]
    pub const fn new(config: IdConfig) -> Self {
        Self { config }
    }

    /// Compute the optimal hash length for a given issue count.
    ///
    /// Uses birthday problem approximation to estimate collision probability.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    pub fn optimal_length(&self, issue_count: usize) -> usize {
        let n = issue_count as f64;
        let max_prob = self.config.max_collision_prob;

        for len in self.config.min_hash_length..=self.config.max_hash_length {
            // Base36 has 36^len possible values
            let space = 36_f64.powi(len as i32);
            // Birthday problem: P(collision) ≈ 1 - e^(-n²/2d)
            let prob = 1.0 - (-n * n / (2.0 * space)).exp();
            if prob < max_prob {
                return len;
            }
        }
        self.config.max_hash_length
    }

    /// Generate a candidate ID with the given parameters.
    #[must_use]
    pub fn generate_candidate(
        &self,
        title: &str,
        description: Option<&str>,
        creator: Option<&str>,
        created_at: DateTime<Utc>,
        nonce: u32,
        hash_length: usize,
    ) -> String {
        let seed = generate_id_seed(title, description, creator, created_at, nonce);
        let hash_str = compute_id_hash(&seed, hash_length);
        format!("{}-{hash_str}", self.config.prefix)
    }

    /// Generate an ID, checking for collisions with the provided checker.
    ///
    /// The checker function should return `true` if the ID already exists.
    /// Lookup failures are returned immediately. If every candidate in the
    /// bounded collision ladder is occupied, this fails closed with
    /// [`BeadsError::IdCollision`] instead of fabricating an unchecked ID.
    ///
    /// # Errors
    ///
    /// Returns the collision checker's error, or `IdCollision` if no free ID
    /// can be found within the bounded search.
    pub fn generate<F>(
        &self,
        title: &str,
        description: Option<&str>,
        creator: Option<&str>,
        created_at: DateTime<Utc>,
        issue_count: usize,
        mut exists: F,
    ) -> Result<String>
    where
        F: FnMut(&str) -> Result<bool>,
    {
        let mut length = self.optimal_length(issue_count);

        loop {
            // Try nonces 0..10 at this length
            for nonce in 0..10 {
                let id =
                    self.generate_candidate(title, description, creator, created_at, nonce, length);
                if !exists(&id)? {
                    return Ok(id);
                }
            }

            // All nonces collided, increase length
            if length < self.config.max_hash_length {
                length += 1;
            } else {
                // The normal adaptive range is exhausted. Widen to a
                // 12-character hash and try a bounded nonce range. Every
                // returned candidate must have passed the caller's lookup.
                for nonce in 0..MAX_FALLBACK_NONCE {
                    let seed = generate_id_seed(title, description, creator, created_at, nonce);
                    let hash_str = compute_id_hash(&seed, FALLBACK_HASH_LENGTH);
                    let id = format!("{}-{hash_str}", self.config.prefix);

                    if !exists(&id)? {
                        return Ok(id);
                    }
                }

                let seed =
                    generate_id_seed(title, description, creator, created_at, MAX_FALLBACK_NONCE);
                let hash_str = compute_id_hash(&seed, FALLBACK_HASH_LENGTH);
                let id = format!("{}-{hash_str}", self.config.prefix);
                return if exists(&id)? {
                    Err(BeadsError::IdCollision { id })
                } else {
                    Ok(id)
                };
            }
        }
    }
}

/// Generate the seed string for ID generation.
///
/// Inputs are length-prefixed as `len:value` fields so embedded separators in
/// titles or descriptions cannot collide with adjacent fields.
#[must_use]
pub fn generate_id_seed(
    title: &str,
    description: Option<&str>,
    creator: Option<&str>,
    created_at: DateTime<Utc>,
    nonce: u32,
) -> String {
    let timestamp = created_at.timestamp_nanos_opt().unwrap_or(0).to_string();
    let nonce = nonce.to_string();

    let mut seed = String::new();
    append_seed_part(&mut seed, title);
    append_seed_part(&mut seed, description.unwrap_or(""));
    append_seed_part(&mut seed, creator.unwrap_or(""));
    append_seed_part(&mut seed, &timestamp);
    append_seed_part(&mut seed, &nonce);
    seed
}

fn append_seed_part(seed: &mut String, value: &str) {
    use std::fmt::Write;
    write!(seed, "{}:", value.len()).expect("writing to String never fails");
    seed.push_str(value);
}

/// Compute a base36 hash of the input string with a specific length.
///
/// Uses SHA256 to hash the input, then converts the first 8 bytes to a u64,
/// encodes as base36, and truncates to the requested length.
#[must_use]
pub fn compute_id_hash(input: &str, length: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();

    // Use first 8 bytes for a 64-bit integer
    let mut num = 0u64;
    for &byte in result.iter().take(8) {
        num = (num << 8) | u64::from(byte);
    }

    let encoded = base36_encode(num);

    // Pad with '0' if too short (unlikely for u64 but possible)
    let mut s = encoded;
    if s.len() < length {
        s = format!("{s:0>length$}");
    }

    // Take the last `length` characters to ensure full entropy from
    // the least significant digits of the base36 encoding.
    let start = s.len().saturating_sub(length);
    s.chars().skip(start).collect()
}

fn base36_encode(mut num: u64) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if num == 0 {
        return "0".to_string();
    }
    let mut chars = Vec::new();
    while num > 0 {
        chars.push(ALPHABET[(num % 36) as usize] as char);
        num /= 36;
    }
    chars.into_iter().rev().collect()
}

// ============================================================================
// Child ID Helpers
// ============================================================================

/// Generate child ID from parent.
///
/// Child IDs have format: `<parent>.<n>` where n is the child number.
#[must_use]
pub fn child_id(parent_id: &str, child_number: u32) -> String {
    format!("{parent_id}.{child_number}")
}

fn issue_id_separator(id: &str) -> Option<usize> {
    id.rfind('-')
}

pub(crate) fn split_prefix_remainder(id: &str) -> Option<(&str, &str)> {
    let dash_pos = issue_id_separator(id)?;
    let (prefix, remainder_with_dash) = id.split_at(dash_pos);
    let remainder = remainder_with_dash.strip_prefix('-')?;
    if prefix.is_empty() || remainder.is_empty() {
        return None;
    }
    Some((prefix, remainder))
}

// ============================================================================
// ID Parsing and Validation
// ============================================================================

use crate::error::{BeadsError, Result};

/// Parsed components of an issue ID.
///
/// Supports both root IDs (`br-abc123`) and hierarchical IDs (`br-abc123.1.2`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedId {
    /// The prefix (e.g., "br").
    pub prefix: String,
    /// The hash portion (e.g., "abc123").
    pub hash: String,
    /// Child path segments if this is a hierarchical ID (e.g., `[1, 2]` for `.1.2`).
    pub child_path: Vec<u32>,
}

impl ParsedId {
    /// Returns true if this is a root (non-hierarchical) ID.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.child_path.is_empty()
    }

    /// Reconstruct the full ID string.
    ///
    /// No production caller: `parse_id` is the live direction. This is the
    /// inverse, and it exists so `tests/proptest/id.rs` can state the
    /// round-trip invariant — every ID `parse_id` accepts must format back to
    /// the bytes it was given.
    #[must_use]
    pub fn to_id_string(&self) -> String {
        if self.child_path.is_empty() {
            format!("{}-{}", self.prefix, self.hash)
        } else {
            let path_str = format_child_path(&self.child_path);
            format!("{}-{}{}", self.prefix, self.hash, path_str)
        }
    }

    /// Get the parent ID if this is a child.
    ///
    /// Returns `None` for root IDs.
    #[must_use]
    pub fn parent(&self) -> Option<String> {
        if self.child_path.is_empty() {
            return None;
        }

        let mut parent_path = self.child_path.clone();
        parent_path.pop();

        if parent_path.is_empty() {
            Some(format!("{}-{}", self.prefix, self.hash))
        } else {
            let path_str = format_child_path(&parent_path);
            Some(format!("{}-{}{}", self.prefix, self.hash, path_str))
        }
    }
}

fn format_child_path(path: &[u32]) -> String {
    let mut out = String::new();
    for segment in path {
        use std::fmt::Write;
        let _ = write!(out, ".{segment}");
    }
    out
}

/// Parse an issue ID into its components.
///
/// # Errors
///
/// Returns `InvalidId` if the ID format is invalid.
pub fn parse_id(id: &str) -> Result<ParsedId> {
    // Find the prefix-hash separator (supports hyphenated prefixes)
    let Some((prefix, remainder)) = split_prefix_remainder(id) else {
        return Err(BeadsError::InvalidId { id: id.to_string() });
    };

    if prefix.is_empty() || prefix.len() > MAX_ID_PREFIX_LEN {
        return Err(BeadsError::InvalidId { id: id.to_string() });
    }

    if !prefix.chars().all(|c| {
        c.is_ascii_lowercase()
            || c.is_ascii_digit()
            || c == '_'
            || c == '-'
            || c == '.'
            || c == ':'
            || c == '#'
    }) {
        return Err(BeadsError::InvalidId { id: id.to_string() });
    }

    // Split remainder by '.' to get hash and child path
    let parts: Vec<&str> = remainder.split('.').collect();
    let hash = parts[0].to_string();

    if hash.is_empty() || hash.len() > MAX_ID_HASH_LEN {
        return Err(BeadsError::InvalidId { id: id.to_string() });
    }

    // Validate hash is base36 (lowercase alphanumeric)
    if !hash
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err(BeadsError::InvalidId { id: id.to_string() });
    }

    // Parse child path segments
    let mut child_path = Vec::new();
    for part in parts.iter().skip(1) {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return Err(BeadsError::InvalidId { id: id.to_string() });
        }
        match part.parse::<u32>() {
            Ok(n) => child_path.push(n),
            Err(_) => return Err(BeadsError::InvalidId { id: id.to_string() }),
        }
    }

    Ok(ParsedId {
        prefix: prefix.to_string(),
        hash,
        child_path,
    })
}

/// Normalize an ID to consistent lowercase format.
#[must_use]
pub fn normalize_id(id: &str) -> String {
    id.to_lowercase()
}

fn normalized_prefix_chars(prefix: &str) -> String {
    let normalized: String = prefix
        .trim()
        .chars()
        .filter_map(|c| {
            let normalized = c.to_ascii_lowercase();
            (normalized.is_ascii_lowercase()
                || normalized.is_ascii_digit()
                || matches!(normalized, '_' | '-' | '.' | ':' | '#'))
            .then_some(normalized)
        })
        .take(MAX_ID_PREFIX_LEN)
        .collect();

    normalized
        .trim_end_matches(['_', '-', '.', ':', '#'])
        .to_string()
}

/// Normalize an inferred issue prefix into a valid runtime form.
///
/// This trims whitespace, lowercases ASCII letters, removes unsupported
/// characters, and clamps the prefix to the maximum supported length. Inferred
/// prefixes that contain no usable characters fall back to `br`; explicitly
/// configured prefixes should use [`normalize_configured_prefix`] instead.
#[must_use]
pub fn normalize_prefix(prefix: &str) -> String {
    let normalized = normalized_prefix_chars(prefix);

    if normalized.is_empty() {
        "br".to_string()
    } else {
        normalized
    }
}

/// Normalize an explicitly configured issue prefix.
///
/// Unlike [`normalize_prefix`], this rejects values that contain no usable
/// prefix characters so a typo cannot silently mint IDs in the `br` namespace.
///
/// # Errors
///
/// Returns a configuration error when the configured value is empty after
/// normalization.
pub fn normalize_configured_prefix(prefix: &str) -> Result<String> {
    let normalized = normalized_prefix_chars(prefix);
    if normalized.is_empty() {
        return Err(BeadsError::Config(format!(
            "issue_prefix {prefix:?} contains no usable ASCII letters, digits, or separators"
        )));
    }
    Ok(normalized)
}

/// Abbreviate a long auto-derived issue ID prefix.
///
/// If the normalized prefix is short enough (<= 6 chars), it is returned
/// unchanged. Otherwise, multi-word prefixes are abbreviated to their initials
/// and single-word prefixes fall back to their first three alphanumeric chars.
#[must_use]
pub fn abbreviate_prefix(prefix: &str) -> String {
    let normalized = normalize_prefix(prefix);
    if normalized.len() <= 6 {
        return normalized;
    }

    let segments: Vec<&str> = normalized
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|segment| !segment.is_empty())
        .collect();

    if segments.len() > 1 {
        let abbrev: String = segments
            .iter()
            .filter_map(|segment| segment.chars().next())
            .collect();
        if abbrev.len() > 1 {
            return abbrev;
        }
    }

    let fallback: String = normalized
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(3)
        .collect();
    if fallback.is_empty() {
        normalized
    } else {
        fallback
    }
}

/// Check if a string looks like a valid issue ID format.
#[must_use]
pub fn is_valid_id_format(id: &str) -> bool {
    parse_id(id).is_ok()
}

// ============================================================================
// ID Resolution
// ============================================================================

/// Configuration for ID resolution.
#[derive(Debug, Clone)]
pub struct ResolverConfig {
    /// Default prefix to use when input lacks one.
    pub default_prefix: String,
    /// Whether to allow substring matching on hash portion.
    pub allow_substring_match: bool,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            default_prefix: "br".to_string(),
            allow_substring_match: true,
        }
    }
}

impl ResolverConfig {
    /// Create a new resolver config with the given default prefix.
    #[must_use]
    pub fn with_prefix(prefix: impl Into<String>) -> Self {
        Self {
            default_prefix: normalize_prefix(&prefix.into()),
            ..Default::default()
        }
    }
}

/// Resolved ID result from the resolution process.
#[derive(Debug, Clone)]
pub struct ResolvedId {
    /// The full resolved ID.
    pub id: String,
}

/// ID resolver that resolves partial IDs to full IDs.
///
/// Resolution order:
/// 1. Exact ID match
/// 2. Normalize: if missing prefix, prepend `default_prefix-` and retry
/// 3. Substring match on hash portion across all prefixes
/// 4. Ambiguity => error with candidate list
#[derive(Debug, Clone)]
pub struct IdResolver {
    config: ResolverConfig,
}

impl IdResolver {
    /// Create a new ID resolver with the given config.
    #[must_use]
    pub const fn new(config: ResolverConfig) -> Self {
        Self { config }
    }

    /// Get the default prefix.
    #[must_use]
    pub fn default_prefix(&self) -> &str {
        &self.config.default_prefix
    }

    /// Resolve a partial ID while allowing the lookup callbacks to fail.
    ///
    /// This is the same algorithm as [`IdResolver::resolve`], but it preserves
    /// storage/query errors instead of forcing callers to coerce them into
    /// `false` or an empty match set.
    ///
    /// # Errors
    ///
    /// - Propagates any error returned by `exists_fn` or `substring_match_fn`
    /// - `IssueNotFound` if no match is found
    /// - `AmbiguousId` if multiple matches are found
    pub fn resolve_fallible<F, G>(
        &self,
        input: &str,
        exists_fn: F,
        substring_match_fn: G,
    ) -> Result<ResolvedId>
    where
        F: Fn(&str) -> Result<bool>,
        G: Fn(&str) -> Result<Vec<String>>,
    {
        let input = input.trim();

        if input.is_empty() {
            return Err(BeadsError::InvalidId { id: String::new() });
        }

        let normalized = normalize_id(input);

        if exists_fn(&normalized)? {
            return Ok(ResolvedId { id: normalized });
        }

        if !normalized.contains('-') {
            let with_prefix = format!("{}-{}", self.config.default_prefix, normalized);
            if exists_fn(&with_prefix)? {
                return Ok(ResolvedId { id: with_prefix });
            }
        }

        if self.config.allow_substring_match {
            let (prefix, hash_pattern) = split_prefix_remainder(&normalized)
                .map_or((None, normalized.as_str()), |(p, r)| (Some(p), r));

            if !hash_pattern.is_empty() {
                let mut matches = substring_match_fn(hash_pattern)?;

                if let Some(p) = prefix {
                    let expected_prefix = format!("{p}-");
                    matches.retain(|id| id.starts_with(&expected_prefix));
                }

                match matches.len() {
                    0 => {}
                    1 => {
                        return Ok(ResolvedId {
                            id: matches.into_iter().next().unwrap_or_default(),
                        });
                    }
                    _ => {
                        return Err(BeadsError::AmbiguousId {
                            partial: input.to_string(),
                            matches,
                        });
                    }
                }
            }
        }

        Err(BeadsError::IssueNotFound {
            id: input.to_string(),
        })
    }

    /// Resolve multiple IDs while preserving lookup callback errors.
    ///
    /// # Errors
    ///
    /// Returns the first callback or resolution error encountered.
    pub fn resolve_all_fallible<F, G>(
        &self,
        inputs: &[String],
        exists_fn: F,
        substring_match_fn: G,
    ) -> Result<Vec<ResolvedId>>
    where
        F: Fn(&str) -> Result<bool>,
        G: Fn(&str) -> Result<Vec<String>>,
    {
        inputs
            .iter()
            .map(|input| self.resolve_fallible(input, &exists_fn, &substring_match_fn))
            .collect()
    }
}

/// Find all issue IDs that contain the given substring in their hash portion.
///
/// This is a helper function for implementing the `substring_match_fn` parameter
/// of `IdResolver::resolve`. The caller provides the list of all known IDs.
#[must_use]
pub fn find_matching_ids(all_ids: &[String], hash_substring: &str) -> Vec<String> {
    // Split search pattern into base hash and optional child path so that
    // searching for "64up6.4" correctly matches "bd-64up6.4" instead of
    // stripping the child suffix from the candidate and failing.
    let (search_base, search_child) = match hash_substring.split_once('.') {
        Some((base, child)) => (base, Some(child)),
        None => (hash_substring, None),
    };

    all_ids
        .iter()
        .filter(|id| {
            split_prefix_remainder(id).is_some_and(|(_, remainder)| {
                let base_hash = remainder.split('.').next().unwrap_or(remainder);
                if !base_hash.contains(search_base) {
                    return false;
                }
                match search_child {
                    Some(child) => remainder
                        .split_once('.')
                        .is_some_and(|(_, candidate_child)| candidate_child == child),
                    None => true,
                }
            })
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // ID Resolution Tests
    // ========================================================================

    fn mock_db() -> Vec<String> {
        vec![
            "br-abc123".to_string(),
            "br-abd456".to_string(),
            "br-xyz789".to_string(),
            "br-abc123.1".to_string(),  // child
            "other-def111".to_string(), // different prefix
        ]
    }

    // The live resolver takes fallible callbacks so storage errors survive;
    // these mocks cannot fail, hence the allow.
    #[allow(clippy::unnecessary_wraps)]
    fn exists_in_mock(id: &str) -> Result<bool> {
        Ok(mock_db().contains(&id.to_string()))
    }

    #[allow(clippy::unnecessary_wraps)]
    fn substring_in_mock(pattern: &str) -> Result<Vec<String>> {
        Ok(find_matching_ids(&mock_db(), pattern))
    }

    #[test]
    fn test_resolve_exact_match() {
        let resolver = IdResolver::new(ResolverConfig::default());
        let result = resolver
            .resolve_fallible("br-abc123", exists_in_mock, substring_in_mock)
            .unwrap();
        assert_eq!(result.id, "br-abc123");
    }

    #[test]
    fn test_resolve_prefix_normalized() {
        let resolver = IdResolver::new(ResolverConfig::default());
        let result = resolver
            .resolve_fallible("abc123", exists_in_mock, substring_in_mock)
            .unwrap();
        assert_eq!(result.id, "br-abc123");
    }

    #[test]
    fn test_resolve_substring_match() {
        let resolver = IdResolver::new(ResolverConfig::default());
        // "xyz" should uniquely match "br-xyz789"
        let result = resolver
            .resolve_fallible("xyz", exists_in_mock, substring_in_mock)
            .unwrap();
        assert_eq!(result.id, "br-xyz789");
    }

    #[test]
    fn test_resolve_ambiguous() {
        let resolver = IdResolver::new(ResolverConfig::default());
        // "ab" matches both "br-abc123" and "br-abd456"
        let result = resolver.resolve_fallible("ab", exists_in_mock, substring_in_mock);
        assert!(result.is_err());
        if let Err(BeadsError::AmbiguousId { partial, matches }) = result {
            assert_eq!(partial, "ab");
            assert!(matches.contains(&"br-abc123".to_string()));
            assert!(matches.contains(&"br-abd456".to_string()));
        } else {
            unreachable!("Expected AmbiguousId error");
        }
    }

    #[test]
    fn test_resolve_not_found() {
        let resolver = IdResolver::new(ResolverConfig::default());
        let result = resolver.resolve_fallible("nonexistent", exists_in_mock, substring_in_mock);
        assert!(result.is_err());
        if let Err(BeadsError::IssueNotFound { id }) = result {
            assert_eq!(id, "nonexistent");
        } else {
            unreachable!("Expected IssueNotFound error");
        }
    }

    #[test]
    fn test_resolve_child_id() {
        let resolver = IdResolver::new(ResolverConfig::default());
        let result = resolver
            .resolve_fallible("br-abc123.1", exists_in_mock, substring_in_mock)
            .unwrap();
        assert_eq!(result.id, "br-abc123.1");
    }

    #[test]
    fn test_resolve_case_insensitive() {
        let resolver = IdResolver::new(ResolverConfig::default());
        let result = resolver
            .resolve_fallible("BR-ABC123", exists_in_mock, substring_in_mock)
            .unwrap();
        assert_eq!(result.id, "br-abc123");
    }

    #[test]
    fn test_resolve_with_custom_prefix() {
        let custom_db = vec!["proj-aaa111".to_string()];
        let exists = |id: &str| Ok(custom_db.contains(&id.to_string()));
        let substring = |pattern: &str| Ok(find_matching_ids(&custom_db, pattern));

        let resolver = IdResolver::new(ResolverConfig::with_prefix("proj"));
        let result = resolver
            .resolve_fallible("aaa111", exists, substring)
            .unwrap();
        assert_eq!(result.id, "proj-aaa111");
    }

    #[test]
    fn test_resolve_empty_input() {
        let resolver = IdResolver::new(ResolverConfig::default());
        let result = resolver.resolve_fallible("", exists_in_mock, substring_in_mock);
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_whitespace_trimmed() {
        let resolver = IdResolver::new(ResolverConfig::default());
        let result = resolver
            .resolve_fallible("  br-abc123  ", exists_in_mock, substring_in_mock)
            .unwrap();
        assert_eq!(result.id, "br-abc123");
    }

    #[test]
    fn test_resolve_fallible_propagates_lookup_error() {
        let resolver = IdResolver::new(ResolverConfig::default());
        let result = resolver.resolve_fallible(
            "br-abc123",
            |_id| Err(BeadsError::Config("lookup failed".to_string())),
            |_hash| Ok(Vec::new()),
        );
        assert!(matches!(result, Err(BeadsError::Config(message)) if message == "lookup failed"));
    }

    #[test]
    fn test_resolve_all_fallible_propagates_lookup_error() {
        let resolver = IdResolver::new(ResolverConfig::default());
        let inputs = vec!["br-abc123".to_string(), "br-xyz789".to_string()];
        let result = resolver.resolve_all_fallible(
            &inputs,
            |_id| Err(BeadsError::Config("exists lookup failed".to_string())),
            |_hash| Ok(Vec::new()),
        );
        assert!(
            matches!(result, Err(BeadsError::Config(message)) if message == "exists lookup failed")
        );
    }

    #[test]
    fn test_find_matching_ids_substring() {
        let ids = mock_db();
        let matches = find_matching_ids(&ids, "abc");
        assert!(matches.contains(&"br-abc123".to_string()));
        // Note: br-abc123.1 is a child and its base hash contains "abc"
        assert!(matches.contains(&"br-abc123.1".to_string()));
    }

    #[test]
    fn test_find_matching_ids_no_match() {
        let ids = mock_db();
        let matches = find_matching_ids(&ids, "zzz");
        assert!(matches.is_empty());
    }

    // ========================================================================
    // Original Tests
    // ========================================================================

    #[test]
    fn test_base36_encode() {
        assert_eq!(base36_encode(0), "0");
        assert_eq!(base36_encode(10), "a");
        assert_eq!(base36_encode(35), "z");
        assert_eq!(base36_encode(36), "10");
    }

    #[test]
    fn test_compute_id_hash_length() {
        let input = "test input";
        let hash3 = compute_id_hash(input, 3);
        assert_eq!(hash3.len(), 3);

        let hash8 = compute_id_hash(input, 8);
        assert_eq!(hash8.len(), 8);
    }

    #[test]
    fn test_generate_id_seed() {
        let now = Utc::now();
        let seed = generate_id_seed("title", Some("desc"), Some("me"), now, 0);
        assert!(seed.contains("5:title"));
        assert!(seed.contains("4:desc"));
        assert!(seed.contains("2:me"));
        assert!(seed.ends_with("1:0"));
    }

    #[test]
    fn test_parse_id_root() {
        let parsed = parse_id("bd-abc123").unwrap();
        assert_eq!(parsed.prefix, "bd");
        assert_eq!(parsed.hash, "abc123");
        assert!(parsed.child_path.is_empty());
        assert!(parsed.is_root());
        assert_eq!(parsed.child_path.len(), 0);
    }

    #[test]
    fn test_parse_id_hyphenated_prefix() {
        let parsed = parse_id("bead-me-up-3e9").unwrap();
        assert_eq!(parsed.prefix, "bead-me-up");
        assert_eq!(parsed.hash, "3e9");
        assert!(parsed.child_path.is_empty());

        let parsed2 = parse_id("document-intelligence-0sa.2").unwrap();
        assert_eq!(parsed2.prefix, "document-intelligence");
        assert_eq!(parsed2.hash, "0sa");
        assert_eq!(parsed2.child_path, vec![2]);
    }

    #[test]
    fn test_parse_id_hyphenated_prefix_word_like_hash() {
        // "my-proj" is prefix. "abcd" is hash (4 chars, no digits).
        // Previously failed because "abcd" was deemed unlikely hash, causing split at first dash.
        let parsed = parse_id("my-proj-abcd").unwrap();
        assert_eq!(parsed.prefix, "my-proj");
        assert_eq!(parsed.hash, "abcd");
    }

    #[test]
    fn test_parse_id_child() {
        let parsed = parse_id("bd-abc123.1").unwrap();
        assert_eq!(parsed.prefix, "bd");
        assert_eq!(parsed.hash, "abc123");
        assert_eq!(parsed.child_path, vec![1]);
        assert!(!parsed.is_root());
        assert_eq!(parsed.child_path.len(), 1);
    }

    #[test]
    fn test_parse_id_grandchild() {
        let parsed = parse_id("bd-abc123.1.2").unwrap();
        assert_eq!(parsed.child_path, vec![1, 2]);
        assert_eq!(parsed.child_path.len(), 2);
    }

    #[test]
    fn test_parse_id_external_style() {
        let parsed = parse_id("external:jira-123").unwrap();
        assert_eq!(parsed.prefix, "external:jira");
        assert_eq!(parsed.hash, "123");

        let parsed2 = parse_id("ext:github#repo-456").unwrap();
        assert_eq!(parsed2.prefix, "ext:github#repo");
        assert_eq!(parsed2.hash, "456");
    }

    #[test]
    fn test_parse_id_invalid_no_dash() {
        assert!(parse_id("bdabc123").is_err());
    }

    #[test]
    fn test_parse_id_invalid_empty_hash() {
        assert!(parse_id("bd-").is_err());
    }

    #[test]
    fn test_parse_id_invalid_uppercase() {
        assert!(parse_id("bd-ABC123").is_err());
    }

    #[test]
    fn test_parse_id_long_hash() {
        // Fallback generates 12 chars + potential nonce.
        // parse_id used to limit to 8, but now allows up to MAX_ID_HASH_LEN.
        let long_id = "bd-abc123456789";
        let parsed = parse_id(long_id).unwrap();
        assert_eq!(parsed.hash, "abc123456789");
    }

    #[test]
    fn test_parsed_id_parent() {
        let child = parse_id("bd-abc123.1").unwrap();
        assert_eq!(child.parent(), Some("bd-abc123".to_string()));

        let grandchild = parse_id("bd-abc123.1.2").unwrap();
        assert_eq!(grandchild.parent(), Some("bd-abc123.1".to_string()));

        let root = parse_id("bd-abc123").unwrap();
        assert_eq!(root.parent(), None);
    }

    #[test]
    fn test_parsed_id_to_string() {
        let root = parse_id("bd-abc123").unwrap();
        assert_eq!(root.to_id_string(), "bd-abc123");

        let child = parse_id("bd-abc123.1.2").unwrap();
        assert_eq!(child.to_id_string(), "bd-abc123.1.2");
    }

    #[test]
    fn test_normalize_prefix_sanitizes_and_lowercases() {
        assert_eq!(normalize_prefix("  Project-Name_2!  "), "project-name_2");
        assert_eq!(normalize_prefix("!!!"), "br");
    }

    #[test]
    fn test_configured_prefix_rejects_values_without_usable_characters() {
        for prefix in ["", "   ", "!!!", "日本語", "___", "...", "###"] {
            let err = normalize_configured_prefix(prefix)
                .expect_err("configured prefix must not silently fall back");
            assert!(matches!(err, BeadsError::Config(_)));
        }

        assert_eq!(
            normalize_configured_prefix(" Project-Name_2! ").unwrap(),
            "project-name_2"
        );
        assert!(IdConfig::with_prefix("!!!").is_err());
    }

    #[test]
    fn test_abbreviate_prefix_handles_mixed_case_and_underscores() {
        assert_eq!(abbreviate_prefix("My_Project-Name"), "mpn");
        assert_eq!(abbreviate_prefix("superlongname"), "sup");
    }

    #[test]
    fn test_is_valid_id_format() {
        assert!(is_valid_id_format("bd-abc123"));
        assert!(is_valid_id_format("bd-abc123.1.2"));
        assert!(!is_valid_id_format("invalid"));
        assert!(!is_valid_id_format("bd-ABC")); // uppercase
    }

    #[test]
    fn test_id_generator_optimal_length() {
        let id_gen = IdGenerator::new(IdConfig::default());

        // Small DB should use minimum length
        assert_eq!(id_gen.optimal_length(0), 3);
        assert_eq!(id_gen.optimal_length(10), 3);

        // Large DB should need more characters
        let len_1000 = id_gen.optimal_length(1000);
        assert!(len_1000 >= 3);
        assert!(len_1000 <= 8);
    }

    #[test]
    fn test_id_generator_generate() {
        let id_gen = IdGenerator::new(IdConfig::default());
        let now = Utc::now();

        let id = id_gen
            .generate(
                "Test Issue",
                Some("Description"),
                Some("user"),
                now,
                0,
                |_| Ok(false),
            )
            .expect("collision lookup succeeds");

        assert!(id.starts_with("br-"));
        assert!(is_valid_id_format(&id));
    }

    #[test]
    fn test_id_generator_collision_handling() {
        let id_gen = IdGenerator::new(IdConfig::default());
        let now = Utc::now();

        let mut generated = std::collections::HashSet::new();

        // Generate first ID
        let id1 = id_gen
            .generate("Test", None, None, now, 0, |id| Ok(generated.contains(id)))
            .expect("collision lookup succeeds");
        generated.insert(id1.clone());

        // Generate second ID - should get different one due to collision check
        let id2 = id_gen
            .generate("Test", None, None, now, 0, |id| Ok(generated.contains(id)))
            .expect("collision lookup succeeds");

        assert_ne!(id1, id2);
    }

    #[test]
    fn test_id_generator_propagates_collision_lookup_failure() {
        let id_gen = IdGenerator::new(IdConfig::default());
        let expected = "collision lookup unavailable";

        let err = id_gen
            .generate("Test", None, None, Utc::now(), 0, |_| {
                Err(BeadsError::Config(expected.to_string()))
            })
            .expect_err("lookup failure must stop generation immediately");

        assert!(matches!(err, BeadsError::Config(message) if message == expected));
    }

    #[test]
    fn test_id_generator_fails_closed_when_every_candidate_exists() {
        let id_gen = IdGenerator::new(IdConfig {
            prefix: "br".to_string(),
            min_hash_length: 3,
            max_hash_length: 3,
            max_collision_prob: 0.25,
        });
        let calls = std::cell::Cell::new(0_u32);

        let err = id_gen
            .generate("Test", None, None, Utc::now(), 0, |_| {
                calls.set(calls.get() + 1);
                Ok(true)
            })
            .expect_err("a saturated checker must fail closed");

        assert!(matches!(err, BeadsError::IdCollision { .. }));
        assert_eq!(calls.get(), 10 + MAX_FALLBACK_NONCE + 1);
    }
}
