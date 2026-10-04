//! Run diffoscope and manipulate its JSON output.
//!
//! This is the Rust counterpart to `py/janitor/diffoscope.py`.

use patchkit::unified::{iter_hunks, splitlines, HunkLine};
use pyo3::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::{debug, warn};

/// A node in the diffoscope JSON tree.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DiffoscopeOutput {
    #[serde(
        rename = "diffoscope-json-version",
        skip_serializing_if = "Option::is_none",
        default
    )]
    diffoscope_json_version: Option<u8>,
    pub source1: PathBuf,
    pub source2: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<String>,
    #[serde(default)]
    pub unified_diff: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<DiffoscopeOutput>,
}

/// Errors from `run_diffoscope`.
#[derive(Debug, thiserror::Error)]
pub enum DiffoscopeError {
    #[error("diffoscope timed out")]
    Timeout,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    #[error(transparent)]
    Py(#[from] pyo3::PyErr),
    #[error("{0}")]
    Other(String),
}

// Mirrors the Python `_set_limits`: cap virtual memory only. Extra
// limits (RLIMIT_CPU, RLIMIT_NOFILE, ...) would be new behaviour and
// belong in a separate change.
fn set_memory_limit(limit_mb: u64) {
    let bytes = limit_mb * 1024 * 1024;
    let soft = (bytes as f64 * 0.8) as u64;
    if let Err(e) =
        nix::sys::resource::setrlimit(nix::sys::resource::Resource::RLIMIT_AS, soft, bytes)
    {
        warn!("Failed to set RLIMIT_AS: {}", e);
    }
}

async fn run_diffoscope_one(
    old_binary: &str,
    new_binary: &str,
    diffoscope_command: Option<&str>,
    timeout: Option<f64>,
    memory_limit: Option<u64>,
) -> Result<Option<DiffoscopeOutput>, DiffoscopeError> {
    let command = diffoscope_command.unwrap_or("diffoscope");
    let mut args = shlex::split(command)
        .ok_or_else(|| DiffoscopeError::Other(format!("Failed to parse command: {command}")))?;
    args.extend([
        "--json=-".to_string(),
        "--exclude-directory-metadata=yes".to_string(),
        old_binary.to_string(),
        new_binary.to_string(),
    ]);

    let mut cmd = tokio::process::Command::new(&args[0]);
    cmd.args(&args[1..]);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.process_group(0);

    if let Some(mb) = memory_limit {
        unsafe {
            cmd.pre_exec(move || {
                set_memory_limit(mb);
                Ok(())
            });
        }
    }

    let child = cmd.spawn()?;
    let pid = child.id();
    debug!("Spawned diffoscope: pid={:?} args={:?}", pid, args);

    let output = match timeout {
        Some(secs) => {
            let dur = std::time::Duration::from_secs_f64(secs);
            match tokio::time::timeout(dur, child.wait_with_output()).await {
                Ok(res) => res?,
                Err(_) => {
                    if let Some(pid) = pid {
                        use nix::sys::signal::{killpg, Signal};
                        use nix::unistd::Pid;
                        let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
                    }
                    return Err(DiffoscopeError::Timeout);
                }
            }
        }
        None => child.wait_with_output().await?,
    };

    match output.status.code() {
        Some(0) => Ok(None),
        Some(1) => {
            let out = serde_json::from_slice(&output.stdout)?;
            Ok(Some(out))
        }
        Some(code) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(DiffoscopeError::Other(format!(
                "diffoscope exited with code {code}: {stderr}"
            )))
        }
        None => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(DiffoscopeError::Other(format!(
                "diffoscope killed by signal: {stderr}"
            )))
        }
    }
}

/// Diff each `(old_name, old_path)` against its positional counterpart
/// in `new_binaries`, returning a single wrapper `DiffoscopeOutput`.
pub async fn run_diffoscope(
    old_binaries: &[(&str, &str)],
    new_binaries: &[(&str, &str)],
    timeout: Option<f64>,
    memory_limit: Option<u64>,
    diffoscope_command: Option<&str>,
) -> Result<DiffoscopeOutput, DiffoscopeError> {
    let mut ret = DiffoscopeOutput {
        diffoscope_json_version: Some(1),
        source1: "old version".into(),
        source2: "new version".into(),
        comments: vec![],
        unified_diff: None,
        details: vec![],
    };

    for ((old_name, old_path), (new_name, new_path)) in old_binaries.iter().zip(new_binaries.iter())
    {
        if let Some(mut sub) = run_diffoscope_one(
            old_path,
            new_path,
            diffoscope_command,
            timeout,
            memory_limit,
        )
        .await?
        {
            sub.source1 = (*old_name).into();
            sub.source2 = (*new_name).into();
            sub.diffoscope_json_version = None;
            ret.details.push(sub);
        }
    }
    Ok(ret)
}

/// Reduce source paths to their basename, matching Python's `filter_irrelevant`.
pub fn filter_irrelevant(diff: &mut DiffoscopeOutput) {
    if let Some(name) = diff.source1.file_name() {
        diff.source1 = name.into();
    }
    if let Some(name) = diff.source2.file_name() {
        diff.source2 = name.into();
    }
}

/// Rewrite version strings inside `-`/`+` lines of a unified diff to `display_version`.
pub fn filter_boring_udiff(
    udiff: &str,
    old_version: &str,
    new_version: &str,
    display_version: &str,
) -> std::result::Result<String, patchkit::unified::Error> {
    // patchkit's hunk parser needs lines with their terminators to
    // round-trip: HunkLine::as_bytes() emits `\ No newline at end of
    // file` for any line that doesn't end in `\n`. std's str::lines()
    // strips terminators, so we use patchkit's splitlines instead.
    let mut lines = splitlines(udiff.as_bytes());
    let mut out = String::new();
    for hunk in iter_hunks(&mut lines) {
        let mut hunk = hunk?;
        for line in &mut hunk.lines {
            match line {
                HunkLine::RemoveLine(bytes) => {
                    if let Ok(s) = std::str::from_utf8(bytes) {
                        *bytes = s.replace(old_version, display_version).into_bytes();
                    }
                }
                HunkLine::InsertLine(bytes) => {
                    if let Ok(s) = std::str::from_utf8(bytes) {
                        *bytes = s.replace(new_version, display_version).into_bytes();
                    }
                }
                HunkLine::ContextLine(_) => {}
            }
        }
        out.push_str(&String::from_utf8_lossy(&hunk.as_bytes()));
    }
    Ok(out)
}

fn filter_boring_detail(
    detail: &mut DiffoscopeOutput,
    old_version: &str,
    new_version: &str,
    display_version: &str,
) -> bool {
    if let Some(udiff) = &detail.unified_diff {
        match filter_boring_udiff(udiff, old_version, new_version, display_version) {
            Ok(filtered) => detail.unified_diff = Some(filtered),
            Err(e) => {
                warn!("Error parsing hunk: {}", e);
                detail.unified_diff = None;
            }
        }
    }
    if let Some(s) = detail.source1.to_str() {
        detail.source1 = s.replace(old_version, display_version).into();
    }
    if let Some(s) = detail.source2.to_str() {
        detail.source2 = s.replace(new_version, display_version).into();
    }
    if !detail.details.is_empty() {
        detail.details = std::mem::take(&mut detail.details)
            .into_iter()
            .filter_map(|mut sub| {
                filter_boring_detail(&mut sub, old_version, new_version, display_version)
                    .then_some(sub)
            })
            .collect();
    }
    detail
        .unified_diff
        .as_deref()
        .is_some_and(|s| !s.is_empty())
        || !detail.details.is_empty()
}

/// Drop `Date`/`Distribution`/`Version` changes-file diffs and
/// `.buildinfo` diffs; rewrite version strings in the rest.
///
/// Argument order matches Python: `(old_version, new_version, old_campaign, new_campaign)`.
/// `old_campaign` and `new_campaign` are accepted for API parity but unused.
pub fn filter_boring(
    diff: &mut DiffoscopeOutput,
    old_version: &str,
    new_version: &str,
    _old_campaign: &str,
    _new_campaign: &str,
) {
    const BORING_FIELDS: &[&str] = &["Date", "Distribution", "Version"];
    let display_version = new_version.rsplit_once('~').map_or(new_version, |(v, _)| v);
    diff.details = std::mem::take(&mut diff.details)
        .into_iter()
        .filter_map(|mut detail| {
            let s1 = detail.source1.to_str().unwrap_or("");
            let s2 = detail.source2.to_str().unwrap_or("");
            if BORING_FIELDS.contains(&s1) && BORING_FIELDS.contains(&s2) {
                return None;
            }
            if s1.ends_with(".buildinfo") && s2.ends_with(".buildinfo") {
                return None;
            }
            filter_boring_detail(&mut detail, old_version, new_version, display_version)
                .then_some(detail)
        })
        .collect();
}

/// Render a diffoscope tree using the diffoscope Python presenters.
pub fn format_diffoscope(
    diff: &DiffoscopeOutput,
    content_type: &str,
    title: &str,
    css_url: Option<&str>,
) -> Result<String, DiffoscopeError> {
    if content_type == "application/json" {
        return Ok(serde_json::to_string(diff)?);
    }

    Ok(Python::attach(|py| -> PyResult<String> {
        let reader = py
            .import("diffoscope.readers.json")?
            .getattr("JSONReaderV1")?
            .call0()?;
        let json_str = serde_json::to_string(diff).map_err(|e| {
            pyo3::exceptions::PyValueError::new_err(format!("JSON serialization failed: {e}"))
        })?;
        let py_dict = py.import("json")?.call_method1("loads", (json_str,))?;
        let root = reader.call_method1("load_rec", (py_dict,))?;

        match content_type {
            "text/html" => {
                let presenter = py
                    .import("diffoscope.presenters.html")?
                    .getattr("HTMLPresenter")?
                    .call0()?;
                let sys = py.import("sys")?;
                let old_stdout = sys.getattr("stdout")?;
                let buf = py.import("io")?.getattr("StringIO")?.call0()?;
                sys.setattr("stdout", buf.clone())?;
                let old_argv = sys.getattr("argv")?;
                sys.setattr(
                    "argv",
                    title.split(' ').map(String::from).collect::<Vec<_>>(),
                )?;
                let kwargs = pyo3::types::PyDict::new(py);
                kwargs.set_item("css_url", css_url)?;
                let result = presenter.call_method("output_html", ("-", root), Some(&kwargs));
                sys.setattr("stdout", old_stdout)?;
                sys.setattr("argv", old_argv)?;
                result?;
                Ok(buf.call_method0("getvalue")?.extract::<String>()?)
            }
            "text/markdown" => {
                let out = std::sync::Arc::new(pyo3::types::PyList::empty(py).unbind());
                let sink = out.clone();
                let printfn = move |args: &Bound<pyo3::types::PyTuple>,
                                    _kw: Option<&Bound<pyo3::types::PyDict>>|
                      -> PyResult<()> {
                    let s = if args.len() == 1 {
                        args.get_item(0)?.extract::<String>()?
                    } else {
                        String::new()
                    };
                    Python::attach(|py| sink.call_method1(py, "append", (s + "\n",)))?;
                    Ok(())
                };
                let cb = pyo3::types::PyCFunction::new_closure(py, None, None, printfn)?;
                let presenter = py
                    .import("diffoscope.presenters.markdown")?
                    .getattr("MarkdownTextPresenter")?
                    .call1((cb,))?;
                presenter.call_method1("start", (root,))?;
                Ok(out.extract::<Vec<String>>(py)?.concat())
            }
            "text/plain" => {
                let out = pyo3::types::PyList::empty(py);
                let presenter = py
                    .import("diffoscope.presenters.text")?
                    .getattr("TextPresenter")?
                    .call1((out.getattr("append")?, false))?;
                presenter.call_method1("start", (root,))?;
                Ok(out
                    .extract::<Vec<String>>()?
                    .into_iter()
                    .map(|s| s + "\n")
                    .collect::<String>())
            }
            _ => Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unknown content type {content_type:?}"
            ))),
        }
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(source: &str) -> DiffoscopeOutput {
        DiffoscopeOutput {
            diffoscope_json_version: None,
            source1: source.into(),
            source2: source.into(),
            comments: vec![],
            unified_diff: None,
            details: vec![DiffoscopeOutput {
                diffoscope_json_version: None,
                source1: "leaf".into(),
                source2: "leaf".into(),
                comments: vec![],
                unified_diff: Some("@@ -1,1 +1,1 @@\n-a\n+b\n".to_string()),
                details: vec![],
            }],
        }
    }

    #[tokio::test]
    async fn run_diffoscope_produces_expected_tree() {
        let td = tempfile::tempdir().unwrap();
        let old = td.path().join("old.json");
        let new = td.path().join("new.json");
        std::fs::write(&old, r#"{"foo": "bar"}"#).unwrap();
        std::fs::write(&new, r#"{"foo": "baz"}"#).unwrap();

        let diff = run_diffoscope(
            &[("old", old.to_str().unwrap())],
            &[("new", new.to_str().unwrap())],
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            diff,
            DiffoscopeOutput {
                diffoscope_json_version: Some(1),
                source1: "old version".into(),
                source2: "new version".into(),
                comments: vec![],
                unified_diff: None,
                details: vec![DiffoscopeOutput {
                    diffoscope_json_version: None,
                    source1: "old".into(),
                    source2: "new".into(),
                    comments: vec![],
                    unified_diff: None,
                    details: vec![DiffoscopeOutput {
                        diffoscope_json_version: None,
                        source1: "Pretty-printed".into(),
                        source2: "Pretty-printed".into(),
                        comments: vec![
                            "Similarity: 0.5%".to_string(),
                            "Differences: {\"'foo'\": \"'baz'\"}".to_string(),
                        ],
                        unified_diff: Some(
                            "@@ -1,3 +1,3 @@\n {\n-    \"foo\": \"bar\"\n+    \"foo\": \"baz\"\n }\n"
                                .to_string()
                        ),
                        details: vec![],
                    }]
                }]
            }
        );
    }

    fn sample_diff() -> DiffoscopeOutput {
        DiffoscopeOutput {
            diffoscope_json_version: Some(1),
            source1: "old version".into(),
            source2: "new version".into(),
            comments: vec![],
            unified_diff: None,
            details: vec![DiffoscopeOutput {
                diffoscope_json_version: None,
                source1: "old".into(),
                source2: "new".into(),
                comments: vec![],
                unified_diff: Some(
                    "@@ -1,3 +1,3 @@\n {\n-    \"foo\": \"bar\"\n+    \"foo\": \"baz\"\n }\n"
                        .to_string(),
                ),
                details: vec![DiffoscopeOutput {
                    diffoscope_json_version: None,
                    source1: "Pretty-printed".into(),
                    source2: "Pretty-printed".into(),
                    comments: vec![
                        "Similarity: 0.5%".to_string(),
                        "Differences: {\"'foo'\": \"'baz'\"}".to_string(),
                    ],
                    unified_diff: Some(
                        "@@ -1,3 +1,3 @@\n {\n-    \"foo\": \"bar\"\n+    \"foo\": \"baz\"\n }\n"
                            .to_string(),
                    ),
                    details: vec![],
                }],
            }],
        }
    }

    #[test]
    fn format_markdown_matches_snapshot() {
        let md = format_diffoscope(&sample_diff(), "text/markdown", "title", None).unwrap();
        assert_eq!(
            md,
            "# Comparing `old version` & `new version`\n\n## Comparing `old` & `new`\n\n```diff\n@@ -1,3 +1,3 @@\n {\n-    \"foo\": \"bar\"\n+    \"foo\": \"baz\"\n }\n```\n\n### Pretty-printed\n\n * *Similarity: 0.5%*\n\n * *Differences: {\"'foo'\": \"'baz'\"}*\n\n```diff\n@@ -1,3 +1,3 @@\n {\n-    \"foo\": \"bar\"\n+    \"foo\": \"baz\"\n }\n```\n\n"
        );
    }

    #[test]
    fn format_html_starts_with_doctype() {
        let html = format_diffoscope(&sample_diff(), "text/html", "title", None).unwrap();
        assert!(html.starts_with("<!DOCTYPE html>"));
    }

    #[test]
    fn format_json_is_json() {
        let json = format_diffoscope(&sample_diff(), "application/json", "title", None).unwrap();
        assert_eq!(
            json,
            "{\"diffoscope-json-version\":1,\"source1\":\"old version\",\"source2\":\"new version\",\"unified_diff\":null,\"details\":[{\"source1\":\"old\",\"source2\":\"new\",\"unified_diff\":\"@@ -1,3 +1,3 @@\\n {\\n-    \\\"foo\\\": \\\"bar\\\"\\n+    \\\"foo\\\": \\\"baz\\\"\\n }\\n\",\"details\":[{\"source1\":\"Pretty-printed\",\"source2\":\"Pretty-printed\",\"comments\":[\"Similarity: 0.5%\",\"Differences: {\\\"'foo'\\\": \\\"'baz'\\\"}\"],\"unified_diff\":\"@@ -1,3 +1,3 @@\\n {\\n-    \\\"foo\\\": \\\"bar\\\"\\n+    \\\"foo\\\": \\\"baz\\\"\\n }\\n\"}]}]}"
        );
    }

    #[test]
    fn format_text_matches_snapshot() {
        let text = format_diffoscope(&sample_diff(), "text/plain", "title", None).unwrap();
        assert_eq!(
            text,
            "--- old version\n+++ new version\n│   --- old\n├── +++ new\n│ @@ -1,3 +1,3 @@\n│  {\n│ -    \"foo\": \"bar\"\n│ +    \"foo\": \"baz\"\n│  }\n│ ├── Pretty-printed\n│ │┄ Similarity: 0.5%\n│ │┄ Differences: {\"'foo'\": \"'baz'\"}\n│ │ @@ -1,3 +1,3 @@\n│ │  {\n│ │ -    \"foo\": \"bar\"\n│ │ +    \"foo\": \"baz\"\n│ │  }\n"
        );
    }

    #[test]
    fn filter_irrelevant_strips_directory() {
        let mut diff = DiffoscopeOutput {
            diffoscope_json_version: Some(1),
            source1: "/tmp/old/build_1.0.deb".into(),
            source2: "/tmp/new/build_1.1.deb".into(),
            comments: vec![],
            unified_diff: None,
            details: vec![],
        };
        filter_irrelevant(&mut diff);
        assert_eq!(diff.source1, PathBuf::from("build_1.0.deb"));
        assert_eq!(diff.source2, PathBuf::from("build_1.1.deb"));
    }

    #[test]
    fn filter_boring_udiff_rewrites_versions() {
        let udiff = "@@ -1,2 +1,2 @@\n context\n-pkg_1.0.0.deb\n+pkg_1.1.0.deb\n";
        let out = filter_boring_udiff(udiff, "1.0.0", "1.1.0", "X.Y.Z").unwrap();
        assert_eq!(
            out,
            "@@ -1,2 +1,2 @@\n context\n-pkg_X.Y.Z.deb\n+pkg_X.Y.Z.deb\n"
        );
    }

    #[test]
    fn filter_boring_udiff_leaves_unrelated_lines() {
        let udiff = "@@ -1,3 +1,3 @@\n line a\n-foo\n+bar\n line b\n";
        let out = filter_boring_udiff(udiff, "1.0.0", "1.1.0", "X.Y.Z").unwrap();
        assert_eq!(out, udiff);
    }

    #[test]
    fn filter_boring_drops_date_version_distribution() {
        let mut diff = DiffoscopeOutput {
            diffoscope_json_version: Some(1),
            source1: "a.changes".into(),
            source2: "b.changes".into(),
            comments: vec![],
            unified_diff: None,
            details: vec![
                leaf("Date"),
                leaf("Version"),
                leaf("Distribution"),
                leaf("Description"),
            ],
        };
        filter_boring(&mut diff, "1.0", "1.1", "old", "new");
        assert_eq!(diff.details.len(), 1);
        assert_eq!(diff.details[0].source1, PathBuf::from("Description"));
    }

    #[test]
    fn filter_boring_drops_buildinfo() {
        let buildinfo = DiffoscopeOutput {
            diffoscope_json_version: None,
            source1: "pkg_1.0_amd64.buildinfo".into(),
            source2: "pkg_1.1_amd64.buildinfo".into(),
            comments: vec![],
            unified_diff: None,
            details: vec![DiffoscopeOutput {
                diffoscope_json_version: None,
                source1: "stub".into(),
                source2: "stub".into(),
                comments: vec![],
                unified_diff: Some("@@ -1,1 +1,1 @@\n-a\n+b\n".to_string()),
                details: vec![],
            }],
        };
        let deb = DiffoscopeOutput {
            source1: "pkg_1.0_amd64.deb".into(),
            source2: "pkg_1.1_amd64.deb".into(),
            ..buildinfo.clone()
        };
        let mut diff = DiffoscopeOutput {
            diffoscope_json_version: Some(1),
            source1: "a.changes".into(),
            source2: "b.changes".into(),
            comments: vec![],
            unified_diff: None,
            details: vec![buildinfo, deb],
        };
        filter_boring(&mut diff, "1.0", "1.1", "_", "_");
        assert_eq!(diff.details.len(), 1);
        assert_eq!(diff.details[0].source1, PathBuf::from("pkg_1.1_amd64.deb"));
        assert_eq!(diff.details[0].source2, PathBuf::from("pkg_1.1_amd64.deb"));
    }

    #[test]
    fn filter_boring_strips_tilde_suffix_for_display_version() {
        let mut diff = DiffoscopeOutput {
            diffoscope_json_version: Some(1),
            source1: "a.changes".into(),
            source2: "b.changes".into(),
            comments: vec![],
            unified_diff: None,
            details: vec![DiffoscopeOutput {
                diffoscope_json_version: None,
                source1: "pkg_1.0.0_all.deb".into(),
                source2: "pkg_1.1.0~bpo12+1_all.deb".into(),
                comments: vec![],
                unified_diff: Some("@@ -1,1 +1,1 @@\n-1.0.0\n+1.1.0~bpo12+1\n".to_string()),
                details: vec![],
            }],
        };
        filter_boring(&mut diff, "1.0.0", "1.1.0~bpo12+1", "_", "_");
        assert_eq!(diff.details.len(), 1);
        assert_eq!(diff.details[0].source1, PathBuf::from("pkg_1.1.0_all.deb"));
        assert_eq!(diff.details[0].source2, PathBuf::from("pkg_1.1.0_all.deb"));
    }
}
