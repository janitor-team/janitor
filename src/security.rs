//! Security utilities for input validation and sanitization

use std::path::{Path, PathBuf};

/// Validates a filename to prevent path traversal attacks
pub fn validate_filename(filename: &str) -> Result<(), String> {
    // Check for directory separators
    if filename.contains('/') || filename.contains('\\') {
        return Err("Filename cannot contain directory separators".to_string());
    }

    // Check for path traversal sequences
    if filename.contains("..") {
        return Err("Filename cannot contain parent directory references".to_string());
    }

    // Check for hidden files (starting with .)
    if filename.starts_with('.') {
        return Err("Hidden files are not allowed".to_string());
    }

    // Check for null bytes
    if filename.contains('\0') {
        return Err("Filename cannot contain null bytes".to_string());
    }

    // Check for control characters
    if filename.chars().any(|c| c.is_control()) {
        return Err("Filename cannot contain control characters".to_string());
    }

    // Check for shell metacharacters
    if filename
        .chars()
        .any(|c| matches!(c, ';' | '&' | '|' | '$' | '`' | '<' | '>'))
    {
        return Err("Filename cannot contain shell metacharacters".to_string());
    }

    // Check for reasonable length
    if filename.len() > 255 {
        return Err("Filename too long".to_string());
    }

    if filename.is_empty() {
        return Err("Filename cannot be empty".to_string());
    }

    Ok(())
}

/// Safely joins a base directory with a filename, ensuring the result stays within the base directory.
///
/// The target file is NOT required to exist — only the base directory must. This is so handlers
/// can validate paths before attempting to open, and still return a useful 404 rather than a
/// generic "Cannot resolve path" error when a legitimately-named file doesn't exist yet.
pub fn safe_path_join(base_dir: &Path, filename: &str) -> Result<PathBuf, String> {
    // First validate the filename
    validate_filename(filename)?;

    // Canonicalize the base directory to resolve any symlinks
    let canonical_base = base_dir
        .canonicalize()
        .map_err(|_| "Cannot resolve base directory".to_string())?;

    let path = canonical_base.join(filename);

    // Since filename is validated to not contain separators or traversal components,
    // joining with a canonicalized base is safe. Sanity check anyway.
    if !path.starts_with(&canonical_base) {
        return Err("Path escapes base directory".to_string());
    }

    Ok(path)
}

/// Validates command arguments to prevent command injection
pub fn validate_command_arg(arg: &str) -> Result<(), String> {
    // Check for shell metacharacters
    if arg
        .chars()
        .any(|c| matches!(c, ';' | '&' | '|' | '$' | '`' | '\n' | '\r' | '\0'))
    {
        return Err("Argument contains shell metacharacters".to_string());
    }

    // Check for path traversal
    if arg.contains("..") {
        return Err("Argument contains path traversal".to_string());
    }

    Ok(())
}

/// Sanitizes a string for use in SQL LIKE patterns
pub fn sanitize_sql_like_pattern(input: &str) -> String {
    input
        .replace('\\', "\\\\") // Escape backslashes first
        .replace('%', "\\%") // Escape SQL wildcards
        .replace('_', "\\_") // Escape SQL single character wildcard
}

/// Validates that a string is safe for use in database queries
pub fn validate_db_identifier(identifier: &str) -> Result<(), String> {
    // Check that it only contains alphanumeric characters, underscores, and hyphens
    if !identifier
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return Err("Identifier contains invalid characters".to_string());
    }

    // Check length
    if identifier.is_empty() || identifier.len() > 63 {
        return Err("Identifier length invalid".to_string());
    }

    // Must start with a letter or underscore
    if !identifier.chars().next().unwrap().is_alphabetic() && !identifier.starts_with('_') {
        return Err("Identifier must start with letter or underscore".to_string());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_filename_valid() {
        assert!(validate_filename("test.log").is_ok());
        assert!(validate_filename("build.txt").is_ok());
        assert!(validate_filename("file123.json").is_ok());
    }

    #[test]
    fn test_validate_filename_invalid() {
        assert!(validate_filename("../test.log").is_err());
        assert!(validate_filename("dir/test.log").is_err());
        assert!(validate_filename(".hidden").is_err());
        assert!(validate_filename("file\0name").is_err());
        assert!(validate_filename("file;name").is_err());
        assert!(validate_filename("").is_err());
    }

    #[test]
    fn test_validate_command_arg() {
        assert!(validate_command_arg("normal_arg").is_ok());
        assert!(validate_command_arg("--flag=value").is_ok());

        assert!(validate_command_arg("arg; rm -rf /").is_err());
        assert!(validate_command_arg("arg && echo hi").is_err());
        assert!(validate_command_arg("../../../etc/passwd").is_err());
    }

    #[test]
    fn test_sanitize_sql_like_pattern() {
        assert_eq!(sanitize_sql_like_pattern("test%"), "test\\%");
        assert_eq!(sanitize_sql_like_pattern("test_"), "test\\_");
        assert_eq!(sanitize_sql_like_pattern("test\\"), "test\\\\");
    }

    #[test]
    fn test_validate_db_identifier() {
        assert!(validate_db_identifier("valid_name").is_ok());
        assert!(validate_db_identifier("_private").is_ok());
        assert!(validate_db_identifier("table123").is_ok());

        assert!(validate_db_identifier("123invalid").is_err());
        assert!(validate_db_identifier("invalid-name!").is_err());
        assert!(validate_db_identifier("").is_err());
    }
}
