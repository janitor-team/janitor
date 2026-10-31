//! Validated identifiers for codebases, campaigns, roles, runs, change sets
//! and publish attempts.
//!
//! These values end up in filesystem paths, URLs, redis keys and
//! subprocess arguments, so anything coming from outside (HTTP paths,
//! query strings, JSON bodies, command line arguments) is checked on
//! the way in rather than at each use site.

use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;
use std::str::FromStr;

/// Error returned when a string is not a valid identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidIdentifier {
    kind: &'static str,
    value: String,
}

impl fmt::Display for InvalidIdentifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}: {:?}", self.kind, self.value)
    }
}

impl std::error::Error for InvalidIdentifier {}

impl From<InvalidIdentifier> for pyo3::PyErr {
    fn from(e: InvalidIdentifier) -> Self {
        pyo3::exceptions::PyValueError::new_err(e.to_string())
    }
}

/// Same rule as the `codebase_name` and `campaign_name` SQL domains:
/// `[a-z0-9][a-z0-9+.-]+`.
fn is_valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    let mut rest = chars.peekable();
    if rest.peek().is_none() {
        return false;
    }
    rest.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '.' | '-'))
}

/// `[a-z][a-z-]*`, e.g. `main` or `pristine-tar`.
fn is_valid_role(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c == '-')
}

fn is_valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

macro_rules! identifier {
    ($(#[$meta:meta])* $name:ident, $kind:literal, $check:path) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, sqlx::Type,
        )]
        #[serde(try_from = "String", into = "String")]
        #[sqlx(transparent)]
        pub struct $name(String);

        impl $name {
            /// Validate `value` and wrap it.
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidIdentifier> {
                let value = value.into();
                if $check(&value) {
                    Ok(Self(value))
                } else {
                    Err(InvalidIdentifier { kind: $kind, value })
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Deref for $name {
            type Target = str;

            fn deref(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<std::path::Path> for $name {
            fn as_ref(&self) -> &std::path::Path {
                std::path::Path::new(&self.0)
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = InvalidIdentifier;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidIdentifier;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = InvalidIdentifier;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl pyo3::FromPyObject<'_, '_> for $name {
            type Error = pyo3::PyErr;

            fn extract(obj: pyo3::Borrowed<'_, '_, pyo3::PyAny>) -> pyo3::PyResult<Self> {
                Ok(Self::new(obj.extract::<String>()?)?)
            }
        }

        impl<'py> pyo3::IntoPyObject<'py> for $name {
            type Target = pyo3::types::PyString;
            type Output = pyo3::Bound<'py, Self::Target>;
            type Error = std::convert::Infallible;

            fn into_pyobject(self, py: pyo3::Python<'py>) -> Result<Self::Output, Self::Error> {
                Ok(pyo3::types::PyString::new(py, &self.0))
            }
        }

        impl<'py> pyo3::IntoPyObject<'py> for &$name {
            type Target = pyo3::types::PyString;
            type Output = pyo3::Bound<'py, Self::Target>;
            type Error = std::convert::Infallible;

            fn into_pyobject(self, py: pyo3::Python<'py>) -> Result<Self::Output, Self::Error> {
                Ok(pyo3::types::PyString::new(py, &self.0))
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.0
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                &self.0 == other
            }
        }

        impl PartialEq<$name> for str {
            fn eq(&self, other: &$name) -> bool {
                self == other.0
            }
        }

        impl PartialEq<$name> for &str {
            fn eq(&self, other: &$name) -> bool {
                *self == other.0
            }
        }

        impl PartialEq<$name> for String {
            fn eq(&self, other: &$name) -> bool {
                *self == other.0
            }
        }
    };
}

identifier!(
    /// Name of a codebase.
    CodebaseName,
    "codebase name",
    is_valid_name
);

identifier!(
    /// Name of a campaign.
    Campaign,
    "campaign name",
    is_valid_name
);

identifier!(
    /// Role of a result branch, such as `main` or `pristine-tar`.
    Role,
    "role",
    is_valid_role
);

identifier!(
    /// Identifier of a run.
    RunId,
    "run id",
    is_valid_id
);

identifier!(
    /// Identifier of a change set.
    ChangeSetId,
    "change set id",
    is_valid_id
);

identifier!(
    /// Identifier of a publish attempt.
    PublishId,
    "publish id",
    is_valid_id
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codebase_valid() {
        for name in ["foo", "a0", "0ad", "libfoo-perl", "g++", "foo.bar"] {
            assert_eq!(CodebaseName::new(name).unwrap().as_str(), name);
        }
    }

    #[test]
    fn test_codebase_invalid() {
        for name in [
            "", "a", "Foo", "-foo", ".foo", "..", "foo/bar", "foo bar", "foo_bar", "foo,bar",
            "foo\0", "foo\n",
        ] {
            assert_eq!(
                CodebaseName::new(name),
                Err(InvalidIdentifier {
                    kind: "codebase name",
                    value: name.to_string()
                })
            );
        }
    }

    #[test]
    fn test_campaign() {
        assert_eq!(Campaign::new("lintian-fixes").unwrap(), "lintian-fixes");
        assert!(Campaign::new("../etc").is_err());
    }

    #[test]
    fn test_role() {
        for role in [
            "main",
            "pristine-tar",
            "pristine-lfs",
            "patch-queue",
            "upstream",
        ] {
            assert_eq!(Role::new(role).unwrap().as_str(), role);
        }
        for role in ["", ".", "..", "a/b", "a\\b", "a%b", "Main", "-x", "1abc"] {
            assert!(Role::new(role).is_err(), "{:?}", role);
        }
    }

    #[test]
    fn test_run_id() {
        for id in ["abc", "1f2e-3d4c", "run_1", "A"] {
            assert_eq!(RunId::new(id).unwrap().as_str(), id);
        }
        for id in ["", ".", "..", ".hidden", "a/b", "a.b", "a b", "a;b", "a\0"] {
            assert!(RunId::new(id).is_err(), "{:?}", id);
        }
    }

    #[test]
    fn test_change_set_id() {
        assert_eq!(ChangeSetId::new("cs-1").unwrap().as_str(), "cs-1");
        for id in ["", "..", "a/b", "a.b"] {
            assert!(ChangeSetId::new(id).is_err(), "{:?}", id);
        }
    }

    #[test]
    fn test_publish_id() {
        let id = "2f1c7a4e-9b3d-4e5f-8a6b-0c1d2e3f4a5b";
        assert_eq!(PublishId::new(id).unwrap().as_str(), id);
        assert!(PublishId::new("../x").is_err());
    }

    #[test]
    fn test_deserialize_validates() {
        assert_eq!(
            serde_json::from_str::<CodebaseName>("\"foo\"").unwrap(),
            CodebaseName::new("foo").unwrap()
        );
        assert!(serde_json::from_str::<CodebaseName>("\"../foo\"").is_err());
        assert!(serde_json::from_str::<RunId>("\"a/b\"").is_err());
    }

    #[test]
    fn test_serialize() {
        assert_eq!(
            serde_json::to_string(&RunId::new("abc").unwrap()).unwrap(),
            "\"abc\""
        );
    }

    #[test]
    fn test_display() {
        assert_eq!(Campaign::new("foo").unwrap().to_string(), "foo");
    }
}
