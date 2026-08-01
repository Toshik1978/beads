//! Context extension trait for adding context to errors.
//!
//! Provides a convenient way to add context information to `Result` types,
//! similar to anyhow's `Context` trait but for `BeadsError`.

use super::BeadsError;

/// Extension trait for adding context to `Result` types.
///
/// This allows adding descriptive context to errors without losing
/// the original error information.
pub trait ResultExt<T> {
    /// Wrap the error with additional context.
    ///
    /// # Errors
    ///
    /// Returns the wrapped error if the result was `Err`.
    fn context(self, ctx: impl Into<String>) -> Result<T, BeadsError>;

    /// Wrap the error with lazily-evaluated context.
    ///
    /// # Errors
    ///
    /// Returns the wrapped error if the result was `Err`.
    fn with_context<F, S>(self, f: F) -> Result<T, BeadsError>
    where
        F: FnOnce() -> S,
        S: Into<String>;
}

impl<T, E> ResultExt<T> for Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn context(self, ctx: impl Into<String>) -> Result<T, BeadsError> {
        self.map_err(|e| BeadsError::WithContext {
            context: ctx.into(),
            source: Box::new(e),
        })
    }

    fn with_context<F, S>(self, f: F) -> Result<T, BeadsError>
    where
        F: FnOnce() -> S,
        S: Into<String>,
    {
        self.map_err(|e| BeadsError::WithContext {
            context: f().into(),
            source: Box::new(e),
        })
    }
}

/// Extension trait for `Option` types.

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, ErrorKind};

    #[test]
    fn test_context_on_io_error() {
        let result: Result<(), io::Error> =
            Err(io::Error::new(ErrorKind::NotFound, "file missing"));
        let with_context = result.context("failed to read config");

        assert!(with_context.is_err());
        let err = with_context.unwrap_err();
        assert!(err.to_string().contains("failed to read config"));
    }

    #[test]
    fn test_with_context_lazy() {
        let path = "/some/path";
        let result: Result<(), io::Error> = Err(io::Error::new(ErrorKind::NotFound, "not found"));
        let with_context = result.with_context(|| format!("failed to open {path}"));

        assert!(with_context.is_err());
        let err = with_context.unwrap_err();
        assert!(err.to_string().contains("/some/path"));
    }
}
