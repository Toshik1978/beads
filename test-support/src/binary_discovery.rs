//! Binary discovery and version pinning for benchmark runs.
//!
//! Locates the `br` binary a benchmark should measure and records its version
//! metadata so a results file says which build produced the numbers.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Binary version metadata captured from `--version --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryVersion {
    pub binary: String,
    pub path: PathBuf,
    pub version: String,
    pub commit: Option<String>,
    pub build_date: Option<String>,
    #[serde(default)]
    pub raw_output: String,
}

impl BinaryVersion {
    /// Serialize to JSON for inclusion in benchmark logs.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "binary": self.binary,
            "path": self.path.display().to_string(),
            "version": self.version,
            "commit": self.commit,
            "build_date": self.build_date,
        })
    }
}

/// Result of binary discovery.
#[derive(Debug, Clone)]
pub struct DiscoveredBinaries {
    pub br: BinaryVersion,
}

impl DiscoveredBinaries {
    /// Serialize for inclusion in a benchmark summary.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "br": self.br.to_json(),
        })
    }
}

/// Discover br binary (from cargo build).
fn discover_br() -> Result<BinaryVersion, String> {
    // First check if BR_BINARY env var is set
    if let Ok(br_path) = std::env::var("BR_BINARY") {
        let path = PathBuf::from(&br_path);
        if path.exists() {
            return probe_binary("br", &path);
        }
        return Err(format!("BR_BINARY={br_path} does not exist"));
    }

    // Try cargo-built binary
    if let Some(cargo_bin) = crate::cargo_built_binary("br") {
        return probe_binary("br", &cargo_bin);
    }

    // Try release binary
    let manifest_dir = crate::repo_root();
    let release_bin = manifest_dir.join("target/release/br");
    if release_bin.exists() {
        return probe_binary("br", &release_bin);
    }

    // Try PATH
    if let Some(path) = which("br") {
        return probe_binary("br", &path);
    }

    Err("br binary not found. Build with `cargo build` first.".to_string())
}

/// Probe a binary to extract version information.
fn probe_binary(name: &str, path: &Path) -> Result<BinaryVersion, String> {
    // Try `--version --json` first
    let json_output = run_version_command(path, &["version", "--json"]);
    if let Some(output) = json_output
        && let Ok(parsed) = parse_json_version(&output)
    {
        return Ok(BinaryVersion {
            binary: name.to_string(),
            path: path.to_path_buf(),
            version: parsed.version,
            commit: parsed.commit,
            build_date: parsed.build_date,
            raw_output: output,
        });
    }

    // Fallback to plain `--version`
    let plain_output = run_version_command(path, &["--version"]);
    if let Some(output) = plain_output {
        let version = parse_plain_version(&output);
        return Ok(BinaryVersion {
            binary: name.to_string(),
            path: path.to_path_buf(),
            version,
            commit: None,
            build_date: None,
            raw_output: output,
        });
    }

    // Last resort: just verify it runs
    let check_output = run_version_command(path, &["--help"]);
    if check_output.is_some() {
        return Ok(BinaryVersion {
            binary: name.to_string(),
            path: path.to_path_buf(),
            version: "unknown".to_string(),
            commit: None,
            build_date: None,
            raw_output: check_output.unwrap_or_default(),
        });
    }

    Err(format!(
        "Binary at {} does not respond to version commands",
        path.display()
    ))
}

/// Run a version command and capture output.
fn run_version_command(binary: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new(binary)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}

/// Parsed JSON version response.
#[derive(Debug, Deserialize)]
struct JsonVersion {
    version: String,
    commit: Option<String>,
    build_date: Option<String>,
}

/// Parse JSON version output.
fn parse_json_version(output: &str) -> Result<JsonVersion, serde_json::Error> {
    // Handle potential prefix text before JSON
    let json_start = output.find('{').unwrap_or(0);
    serde_json::from_str(&output[json_start..])
}

/// Parse plain text version output (e.g., "br 0.1.0").
fn parse_plain_version(output: &str) -> String {
    let output = output.trim();

    // Try to extract version number
    for word in output.split_whitespace() {
        if word.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            // Include digits, dots, hyphens, and alphanumeric suffixes (e.g., "0.1.0-dev")
            let version: String = word
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
                .collect();
            if !version.is_empty() {
                return version;
            }
        }
    }

    "unknown".to_string()
}

/// Find binary in PATH.
fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let path = dir.join(name);
            if path.exists() && path.is_file() {
                Some(path)
            } else {
                None
            }
        })
    })
}

/// Discover the binaries a benchmark run needs.
///
/// # Errors
///
/// Returns an error if the `br` binary cannot be located.
pub fn discover_binaries() -> Result<DiscoveredBinaries, String> {
    Ok(DiscoveredBinaries { br: discover_br()? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discover_br() {
        let result = discover_br();
        assert!(result.is_ok(), "br should be discoverable: {result:?}");

        let version = result.unwrap();
        assert_eq!(version.binary, "br");
        assert!(version.path.exists());
    }

    #[test]
    fn test_discover_binaries() {
        let result = discover_binaries();
        assert!(result.is_ok(), "Binary discovery failed: {result:?}");

        let binaries = result.unwrap();
        assert_eq!(binaries.br.binary, "br");
    }

    #[test]
    fn test_parse_plain_version() {
        assert_eq!(parse_plain_version("br 0.1.0"), "0.1.0");
        assert_eq!(parse_plain_version("beads 0.5.2"), "0.5.2");
        assert_eq!(parse_plain_version("0.1.0-dev"), "0.1.0-dev");
        assert_eq!(parse_plain_version("no version"), "unknown");
    }

    #[test]
    fn test_discovered_binaries_json() {
        let binaries = discover_binaries().expect("discovery failed");
        let json = binaries.to_json();

        assert!(json.get("br").is_some());
    }
}
