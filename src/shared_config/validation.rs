//! Configuration validation and error types

use std::fmt;

/// Configuration error types
#[derive(Debug, Clone)]
pub enum ConfigError {
    /// Required configuration value is missing
    MissingRequired(String),

    /// Failed to parse a configuration value
    ParseError { field: String, message: String },

    /// File I/O error
    IoError { path: String, message: String },

    /// Serialization/deserialization error
    SerdeError { format: String, message: String },

    /// Validation error
    ValidationError(ValidationError),

    /// Multiple errors occurred
    Multiple(Vec<ConfigError>),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::MissingRequired(field) => {
                write!(f, "Required configuration field '{}' is missing", field)
            }
            ConfigError::ParseError { field, message } => {
                write!(f, "Failed to parse field '{}': {}", field, message)
            }
            ConfigError::IoError { path, message } => {
                write!(f, "I/O error reading config file '{}': {}", path, message)
            }
            ConfigError::SerdeError { format, message } => {
                write!(f, "Failed to parse {} configuration: {}", format, message)
            }
            ConfigError::ValidationError(err) => {
                write!(f, "Configuration validation error: {}", err)
            }
            ConfigError::Multiple(errors) => {
                writeln!(f, "Multiple configuration errors:")?;
                for (i, error) in errors.iter().enumerate() {
                    writeln!(f, "  {}: {}", i + 1, error)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<ValidationError> for ConfigError {
    fn from(err: ValidationError) -> Self {
        ConfigError::ValidationError(err)
    }
}

/// Configuration validation error
#[derive(Debug, Clone)]
pub enum ValidationError {
    /// A field has an invalid value
    InvalidValue { field: String, message: String },

    /// A required field is missing
    MissingField { field: String },

    /// Fields have conflicting values
    ConflictingFields {
        fields: Vec<String>,
        message: String,
    },

    /// A constraint is violated
    ConstraintViolation { constraint: String, message: String },

    /// Multiple validation errors
    Multiple(Vec<ValidationError>),
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::InvalidValue { field, message } => {
                write!(f, "Invalid value for field '{}': {}", field, message)
            }
            ValidationError::MissingField { field } => {
                write!(f, "Required field '{}' is missing", field)
            }
            ValidationError::ConflictingFields { fields, message } => {
                write!(f, "Conflicting fields [{}]: {}", fields.join(", "), message)
            }
            ValidationError::ConstraintViolation {
                constraint,
                message,
            } => {
                write!(f, "Constraint '{}' violated: {}", constraint, message)
            }
            ValidationError::Multiple(errors) => {
                writeln!(f, "Multiple validation errors:")?;
                for (i, error) in errors.iter().enumerate() {
                    writeln!(f, "  {}: {}", i + 1, error)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ValidationError {}
