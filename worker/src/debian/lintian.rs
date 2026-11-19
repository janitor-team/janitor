use log::debug;
use serde_json;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::str;

#[derive(Debug)]
pub enum Error {
    LintianFailed(std::io::Error),
    LintianOutputInvalid(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match *self {
            Error::LintianFailed(ref e) => write!(f, "Lintian failed: {}", e),
            Error::LintianOutputInvalid(ref e) => write!(f, "Lintian output invalid: {}", e),
        }
    }
}

/// What an item the hint points at — a binary deb's content, the
/// source tarball, or a debian/* file. Lintian 2.135 emits this as
/// `{"index": "<package> (installed)", "name": "<path-in-pkg>"}`.
#[derive(serde::Deserialize, PartialEq, Eq, serde::Serialize, Debug, Default, Clone)]
pub struct LintianPointerItem {
    #[serde(default)]
    pub index: String,
    #[serde(default)]
    pub name: String,
}

/// Where in a file (or which artefact) a hint refers to.
///
/// Older lintian (≤2.116) emitted `pointer` as a plain string —
/// usually a `path:line` rendering. Lintian 2.135 switched to a
/// structured `{"item": …, "line_position": N}` object so the
/// renderer can hyperlink back into the package contents. Both
/// shapes appear in our deployment; the untagged enum lets serde
/// pick the matching variant per record without re-encoding.
///
/// Variants are tried top-to-bottom: the structured form first
/// (it's the only valid match for an object input), `Inline`
/// second (plain string form). `Other` swallows anything else
/// (e.g. a future `[…]` array shape) without aborting the parse —
/// the data is preserved verbatim for the rare consumer that
/// wants it. (`Eq` is intentionally not derived: the `Other`
/// fallback holds a `serde_json::Value`, which only implements
/// `PartialEq`. None of the call sites need stricter equality.)
#[derive(serde::Deserialize, PartialEq, serde::Serialize, Debug, Clone)]
#[serde(untagged)]
pub enum LintianPointer {
    Structured {
        #[serde(default)]
        item: LintianPointerItem,
        #[serde(default)]
        line_position: i64,
    },
    Inline(String),
    Other(serde_json::Value),
}

/// The structured form of one lintian hint — the per-issue record
/// inside an input file's `hints` array. Real
/// `lintian --exp-output=format=json` emits objects like
///   `{"experimental": false, "note": "sid unstable",
///     "tag": "distribution-and-changes-mismatch",
///     "visibility": "warning"}`.
///
/// All fields are `#[serde(default)]` so a future lintian emitting
/// fewer keys doesn't break parsing; extra keys are silently
/// dropped (no `deny_unknown_fields`).
#[derive(serde::Deserialize, PartialEq, serde::Serialize, Debug, Default, Clone)]
pub struct LintianHintObject {
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub visibility: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub experimental: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<LintianPointer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<String>,
}

/// A lintian hint. Older lintian (and some plugins) emitted each hint
/// as a plain string; lintian 2.135 switched to a structured object.
/// The untagged enum accepts both shapes so a single worker build can
/// parse output from either.
///
/// `Eq` is intentionally not derived: `LintianHintObject::pointer`
/// can hold a `serde_json::Value`, which only implements `PartialEq`.
/// No call site needs stricter equality.
#[derive(serde::Deserialize, PartialEq, serde::Serialize, Debug, Clone)]
#[serde(untagged)]
pub enum LintianHint {
    Structured(LintianHintObject),
    Inline(String),
}

#[derive(serde::Deserialize, PartialEq, serde::Serialize, Debug)]
pub struct LintianInputFile {
    pub hints: Vec<LintianHint>,
    pub path: PathBuf,
}

#[derive(serde::Deserialize, PartialEq, serde::Serialize, Debug)]
pub struct LintianGroup {
    pub group_id: String,
    pub input_files: Vec<LintianInputFile>,
    pub source_name: String,
    pub source_version: debversion::Version,
}

#[derive(serde::Deserialize, PartialEq, Default, serde::Serialize, Debug)]
pub struct LintianResult {
    pub groups: Vec<LintianGroup>,
    pub lintian_version: Option<debversion::Version>,
}

impl std::error::Error for Error {}

fn parse_lintian_output(text: &str) -> Result<LintianResult, serde_json::Error> {
    let lines: Vec<&str> = text.trim().split('\n').collect();
    let mut joined_lines: Vec<&str> = Vec::new();
    for line in lines {
        joined_lines.push(line);
        if line == "}" {
            break;
        }
    }

    let joined_str = joined_lines.join("\n");
    let mut result: LintianResult = serde_json::from_str(&joined_str)?;

    // Strip irrelevant directory information
    for group in &mut result.groups {
        for input_file in &mut group.input_files {
            input_file.path = Path::new(input_file.path.file_name().unwrap()).to_path_buf();
        }
    }

    Ok(result)
}

pub fn run_lintian(
    output_directory: &Path,
    changes_names: Vec<&Path>,
    profile: Option<&str>,
    suppress_tags: Option<Vec<&str>>,
) -> Result<LintianResult, Error> {
    let mut args: Vec<String> = vec![
        "--exp-output=format=json".to_owned(),
        "--allow-root".to_owned(),
    ];
    if let Some(tags) = suppress_tags {
        args.push(format!("--suppress-tags={}", tags.join(",")));
    }
    if let Some(profile_str) = profile {
        args.push(format!("--profile={}", profile_str));
    }
    let mut cmd = Command::new("lintian");
    cmd.args(args);
    cmd.args(changes_names);
    cmd.current_dir(output_directory);
    debug!("Running lintian: {:?}", cmd);

    let lintian_output: Output = match cmd.output() {
        Ok(output) => output,
        Err(e) => {
            return Err(Error::LintianFailed(e));
        }
    };

    let output_str = match str::from_utf8(&lintian_output.stdout) {
        Ok(s) => s,
        Err(e) => {
            return Err(Error::LintianOutputInvalid(e.to_string()));
        }
    };

    parse_lintian_output(output_str).map_err(|e| Error::LintianOutputInvalid(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_lintian_output() {
        let output_str = r#"{
   "groups" : [
      {
         "group_id" : "lintian-brush_0.148",
         "input_files" : [
            {
               "hints" : [],
               "path" : "/tmp/lintian-brush_0.148.dsc"
            },
            {
               "hints" : [],
               "path" : "/tmp/lintian-brush_0.148_source.buildinfo"
            },
            {
               "hints" : [],
               "path" : "/tmp/lintian-brush_0.148_source.changes"
            }
         ],
         "source_name" : "lintian-brush",
         "source_version" : "0.148"
      }
   ],
   "lintian_version" : "2.116.3"
}
OTHER BOGUS DATA
"#;
        let result = parse_lintian_output(output_str).unwrap();
        assert_eq!(
            result,
            LintianResult {
                groups: vec![LintianGroup {
                    group_id: "lintian-brush_0.148".to_owned(),
                    input_files: vec![
                        LintianInputFile {
                            hints: vec![],
                            path: PathBuf::from("lintian-brush_0.148.dsc")
                        },
                        LintianInputFile {
                            hints: vec![],
                            path: PathBuf::from("lintian-brush_0.148_source.buildinfo")
                        },
                        LintianInputFile {
                            hints: vec![],
                            path: PathBuf::from("lintian-brush_0.148_source.changes")
                        },
                    ],
                    source_name: "lintian-brush".to_owned(),
                    source_version: "0.148".parse().unwrap(),
                }],
                lintian_version: Some("2.116.3".parse().unwrap())
            }
        );
    }

    /// Regression for the worker-side
    /// `Lintian output invalid: invalid type: map, expected a string`
    /// failure: real lintian-output objects in the `hints` array were
    /// being deserialised into `Vec<String>`. Verbatim sample taken
    /// from `lintian --exp-output=format=json hello_*_amd64.changes`.
    #[test]
    fn test_parse_lintian_output_with_real_hint_objects() {
        let output_str = r#"{
   "groups" : [
      {
         "group_id" : "hello_2.10-5",
         "input_files" : [
            {
               "hints" : [
                  {
                     "experimental" : false,
                     "note" : "sid unstable",
                     "tag" : "distribution-and-changes-mismatch",
                     "visibility" : "warning"
                  }
               ],
               "path" : "./hello_2.10-5_amd64.changes"
            }
         ],
         "source_name" : "hello",
         "source_version" : "2.10-5"
      }
   ],
   "lintian_version" : "2.135.0"
}
"#;
        let result = parse_lintian_output(output_str).expect("real hints must parse");
        let hints = &result.groups[0].input_files[0].hints;
        assert_eq!(hints.len(), 1);
        match &hints[0] {
            LintianHint::Structured(hint) => {
                assert_eq!(hint.tag, "distribution-and-changes-mismatch");
                assert_eq!(hint.visibility, "warning");
                assert_eq!(hint.note, "sid unstable");
                assert!(!hint.experimental);
            }
            other => panic!("expected Structured hint, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_lintian_output_with_2_135_pointer_object() {
        let output_str = r#"{
   "groups" : [
      {
         "group_id" : "okio_1.16.0-3~jan+lint1",
         "input_files" : [
            {
               "hints" : [
                  {
                     "experimental" : false,
                     "note" : "lintian-fixes != sid",
                     "pointer" : {
                        "item" : {
                           "index" : "libokio-java-doc_1.16.0-3~jan+lint1_all.deb (installed)",
                           "name" : "usr/share/doc/libokio-java-doc/changelog.Debian.gz"
                        },
                        "line_position" : 1
                     },
                     "tag" : "changelog-distribution-does-not-match-changes-file",
                     "visibility" : "warning"
                  }
               ],
               "path" : "./okio_1.16.0-3~jan+lint1_amd64.changes"
            }
         ],
         "source_name" : "okio",
         "source_version" : "1.16.0-3~jan+lint1"
      }
   ],
   "lintian_version" : "2.135.0"
}
"#;
        let result =
            parse_lintian_output(output_str).expect("2.135 pointer-object hints must parse");
        let hints = &result.groups[0].input_files[0].hints;
        assert_eq!(hints.len(), 1);
        let hint = match &hints[0] {
            LintianHint::Structured(h) => h,
            other => panic!("expected Structured hint, got {:?}", other),
        };
        assert_eq!(
            hint.tag,
            "changelog-distribution-does-not-match-changes-file"
        );
        let pointer = hint.pointer.as_ref().expect("pointer should be retained");
        match pointer {
            LintianPointer::Structured {
                item,
                line_position,
            } => {
                assert_eq!(*line_position, 1);
                assert_eq!(
                    item.name,
                    "usr/share/doc/libokio-java-doc/changelog.Debian.gz"
                );
                assert!(item.index.starts_with("libokio-java-doc"));
            }
            other => panic!("expected Structured pointer, got {:?}", other),
        }
    }

    /// Older lintian (and some plugins) still emit `pointer` as a
    /// plain string. Make sure the untagged enum picks up that
    /// shape too — otherwise a partial regression here would
    /// break parsing on the non-2.135 path.
    #[test]
    fn test_parse_lintian_output_with_string_pointer() {
        let output_str = r#"{
   "groups" : [
      {
         "group_id" : "x_1",
         "input_files" : [
            {
               "hints" : [
                  {
                     "tag" : "trailing-whitespace",
                     "visibility" : "info",
                     "note" : "",
                     "experimental" : false,
                     "pointer" : "debian/changelog:42"
                  }
               ],
               "path" : "x.dsc"
            }
         ],
         "source_name" : "x",
         "source_version" : "1"
      }
   ],
   "lintian_version" : "2.116.3"
}
"#;
        let result =
            parse_lintian_output(output_str).expect("string-pointer hints must still parse");
        let hint = match &result.groups[0].input_files[0].hints[0] {
            LintianHint::Structured(h) => h,
            other => panic!("expected Structured hint, got {:?}", other),
        };
        let pointer = hint.pointer.as_ref().expect("pointer should be retained");
        match pointer {
            LintianPointer::Inline(s) => assert_eq!(s, "debian/changelog:42"),
            other => panic!("expected Inline pointer, got {:?}", other),
        }
    }

    /// Future lintian releases may add new fields to a hint; we accept
    /// and silently drop those rather than refusing to parse the whole
    /// run.
    /// Older lintian emitted each hint as a plain string rather than an object.
    #[test]
    fn test_parse_lintian_output_with_string_hints() {
        let output_str = r#"{
   "groups" : [
      {
         "group_id" : "x_1",
         "input_files" : [
            {
               "hints" : [
                  "debian-changelog-line-too-long line 1",
                  "no-copyright-file"
               ],
               "path" : "x.dsc"
            }
         ],
         "source_name" : "x",
         "source_version" : "1"
      }
   ],
   "lintian_version" : "2.100.0"
}
"#;
        let result = parse_lintian_output(output_str).expect("string hints must parse");
        let hints = &result.groups[0].input_files[0].hints;
        assert_eq!(hints.len(), 2);
        assert_eq!(
            hints[0],
            LintianHint::Inline("debian-changelog-line-too-long line 1".to_owned())
        );
        assert_eq!(
            hints[1],
            LintianHint::Inline("no-copyright-file".to_owned())
        );
    }

    #[test]
    fn test_parse_lintian_output_tolerates_extra_hint_fields() {
        let output_str = r#"{
   "groups" : [
      {
         "group_id" : "x_1",
         "input_files" : [
            {
               "hints" : [
                  {
                     "tag" : "some-tag",
                     "visibility" : "info",
                     "note" : "",
                     "experimental" : false,
                     "future_field" : "ignored"
                  }
               ],
               "path" : "x.dsc"
            }
         ],
         "source_name" : "x",
         "source_version" : "1"
      }
   ],
   "lintian_version" : "2.135.0"
}
"#;
        let result = parse_lintian_output(output_str).unwrap();
        let hint = match &result.groups[0].input_files[0].hints[0] {
            LintianHint::Structured(h) => h,
            other => panic!("expected Structured hint, got {:?}", other),
        };
        assert_eq!(hint.tag, "some-tag");
    }
}
