//! Version command implementation.

use crate::cli::VersionArgs;
use crate::error::Result;
use crate::output::{OutputContext, OutputMode};
use rich_rust::prelude::*;
use serde::Serialize;

#[derive(Serialize)]
struct VersionOutput<'a> {
    version: &'a str,
    build: &'a str,
}

/// Execute the version command.
///
/// # Errors
///
/// Returns an error if JSON serialization fails or update check fails.
pub fn execute(args: &VersionArgs, ctx: &OutputContext) -> Result<()> {
    let version = env!("CARGO_PKG_VERSION");

    // Handle --short flag: output only version number
    if args.short {
        if !ctx.is_quiet() {
            println!("{version}");
        }
        return Ok(());
    }

    let build = if cfg!(debug_assertions) {
        "dev"
    } else {
        "release"
    };

    if ctx.is_json() {
        let output = VersionOutput { version, build };
        ctx.json(&output);
        return Ok(());
    }

    if ctx.is_quiet() {
        return Ok(());
    }

    // Rich output mode
    if matches!(ctx.mode(), OutputMode::Rich) {
        render_version_rich(version, build, ctx);
        return Ok(());
    }

    // Plain text output
    println!("br version {version} ({build})");
    Ok(())
}

/// Render version information with rich formatting.
fn render_version_rich(version: &str, build: &str, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    // Version header with styling
    content.append_styled(&format!("br {version}"), theme.emphasis.clone());
    content.append_styled(&format!(" ({build})"), theme.dimmed.clone());
    content.append("\n\n");

    // There is deliberately no "Build Info" section. It used to carry the
    // commit, branch, rustc version and target triple, all stamped by a build
    // script. That script was removed: for a released binary the version and
    // its matching tag already identify the commit exactly, so the hash was
    // redundant precisely where users have it, and informative only in a dev
    // build — where `git rev-parse HEAD` in the tree you are standing in is
    // both easier and always accurate. The build script's copy was not: it
    // emitted no `cargo:rerun-if-changed` for git state, so the stamped SHA
    // went stale after any commit that did not also touch a source file.

    // Wrap in panel
    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("br version", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_json_schema() {
        let output = VersionOutput {
            version: "1.0.0",
            build: "release",
        };

        let json = serde_json::to_value(&output).unwrap();
        assert_eq!(json["version"], "1.0.0");
        assert_eq!(json["build"], "release");
    }

    /// `version --json` carries these two keys and nothing else.
    ///
    /// Asserted as an exact key set rather than as a list of `is_some()`
    /// checks, so that re-adding a field is a deliberate act that updates this
    /// test. The four that used to be here — commit, branch, `rust_version`,
    /// target — came from a build script that no longer exists; see
    /// `render_version_rich` for why it went.
    #[test]
    fn test_version_json_has_no_build_metadata() {
        let output = VersionOutput {
            version: "1.0.0",
            build: "dev",
        };

        let json = serde_json::to_value(&output).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .expect("version --json is an object")
            .keys()
            .map(String::as_str)
            .collect();

        assert_eq!(keys, ["version", "build"]);
    }

    #[test]
    fn test_version_short_format() {
        // The short format should just be the version number
        let version = env!("CARGO_PKG_VERSION");
        // Should match semver pattern
        assert!(
            version.contains('.'),
            "Version should contain dots: {version}"
        );
        assert!(
            version.split('.').count() >= 2,
            "Version should have at least major.minor"
        );
    }
}
