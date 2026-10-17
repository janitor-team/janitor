//! Codebase names.

use std::fmt;
use std::path::{Component, Path};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The name of a codebase.
///
/// Always one plain path component without NUL, so it can be joined
/// onto a store directory without leaving it. Use it as an axum `Path`
/// extractor to reject other names before a handler runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CodebaseName(String);

/// A string that is not a valid [`CodebaseName`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidCodebaseName(String);

impl fmt::Display for InvalidCodebaseName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid codebase name: {:?}", self.0)
    }
}

impl std::error::Error for InvalidCodebaseName {}

impl CodebaseName {
    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CodebaseName {
    type Error = InvalidCodebaseName;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        let mut components = Path::new(&name).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(c)), None) if c == name.as_str() && !name.contains('\0') => {
                Ok(Self(name))
            }
            _ => Err(InvalidCodebaseName(name)),
        }
    }
}

impl TryFrom<&str> for CodebaseName {
    type Error = InvalidCodebaseName;

    fn try_from(name: &str) -> Result<Self, Self::Error> {
        Self::try_from(name.to_string())
    }
}

impl FromStr for CodebaseName {
    type Err = InvalidCodebaseName;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::try_from(name)
    }
}

impl From<CodebaseName> for String {
    fn from(name: CodebaseName) -> Self {
        name.0
    }
}

impl AsRef<str> for CodebaseName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<Path> for CodebaseName {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl fmt::Display for CodebaseName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &[&str] = &["codebase", "a.b", "a,b", "a b", ".hidden", "..x", "x.."];
    const INVALID: &[&str] = &[
        "", ".", "..", "/", "/etc", "a/b", "../a", "a/..", "a/", "./a", "a/.", "a\0b",
    ];

    #[test]
    fn test_valid_names() {
        for name in VALID {
            let parsed: CodebaseName = name.parse().unwrap();
            assert_eq!(parsed.as_str(), *name);
            assert_eq!(parsed.to_string(), *name);
        }
    }

    #[test]
    fn test_invalid_names() {
        for name in INVALID {
            assert_eq!(
                CodebaseName::try_from(*name),
                Err(InvalidCodebaseName(name.to_string())),
                "{:?}",
                name
            );
        }
    }

    #[test]
    fn test_join_stays_below_base() {
        let base = Path::new("/srv/store");
        for name in VALID {
            let name: CodebaseName = name.parse().unwrap();
            assert_eq!(base.join(&name).parent(), Some(base));
        }
    }

    #[test]
    fn test_serde() {
        let name: CodebaseName = serde_json::from_str("\"codebase\"").unwrap();
        assert_eq!(name.as_str(), "codebase");
        assert_eq!(serde_json::to_string(&name).unwrap(), "\"codebase\"");
        assert!(serde_json::from_str::<CodebaseName>("\"../etc\"").is_err());
    }
}
