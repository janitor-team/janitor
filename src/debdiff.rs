/// Parse a debdiff
///
/// # Arguments
/// * `text` - The debdiff
///
/// # Returns
/// An iterator over the sections of the debdiff
/// Each section is a tuple of the title of the section and the lines of the section
/// The title is None if the section is a paragraph
/// The lines are a Vec of strings
pub fn iter_sections(text: &str) -> impl Iterator<Item = (Option<&str>, Vec<&str>)> {
    let lines = text.split_terminator('\n').collect::<Vec<_>>();
    let mut title = None;
    let mut paragraph = Vec::new();
    let mut i = 0;
    let mut ret = vec![];
    while i < lines.len() {
        let line = lines[i];
        if i + 1 < lines.len() && lines[i + 1] == ("-".repeat(line.len())) {
            if title.is_some() || !paragraph.is_empty() {
                ret.push((title, paragraph));
            }
            title = Some(line);
            paragraph = Vec::new();
            i += 1;
        } else if line.trim_end().is_empty() {
            if title.is_some() || !paragraph.is_empty() {
                ret.push((title, paragraph));
            }
            title = None;
            paragraph = Vec::new();
        } else {
            paragraph.push(line);
        }
        i += 1;
    }
    if title.is_some() || !paragraph.is_empty() {
        ret.push((title, paragraph));
    }
    ret.into_iter()
}

/// Filter boring lines out of a wdiff
///
/// # Arguments
/// * `lines` - The lines of the wdiff
/// * `old_version` - The old version
/// * `new_version` - The new version
///
/// # Returns
/// A new Vec of lines with the boring lines removed
pub fn filter_boring_wdiff<'a>(
    lines: Vec<&'a str>,
    old_version: &'a str,
    new_version: &'a str,
) -> Vec<String> {
    if lines.is_empty() {
        return Vec::new();
    }
    let (field, _changes) = match lines[0].split_once(':') {
        Some((field, changes)) => (field, changes),
        None => {
            return lines
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
        }
    };
    if field == "Installed-Size" {
        return Vec::new();
    }
    if field == "Version" {
        return Vec::new();
    }
    let regex = fancy_regex::Regex::new(&format!(
        r"\[-{}(.*?)-\] \{{\+{}\1\+\}}",
        regex::escape(old_version),
        regex::escape(new_version),
    ))
    .unwrap();

    let lines = lines
        .iter()
        .map(|line| regex.replace_all(line, r"$1").to_string())
        .collect::<Vec<_>>();
    let block = lines.join("\n");

    if lazy_regex::regex_find!(r"\[-.*?-\]", &block).is_none()
        && lazy_regex::regex_find!(r"\{\+.*?\+\}", &block).is_none()
    {
        return Vec::new();
    }
    lines
}

fn iter_fields<'a>(
    lines: impl Iterator<Item = &'a str> + 'a,
) -> impl Iterator<Item = Vec<&'a str>> + 'a {
    let mut cl = Vec::new();
    let mut ret = Vec::new();
    for line in lines {
        if !cl.is_empty() && line.starts_with(" ") {
            cl.push(line);
        } else {
            if !cl.is_empty() {
                ret.push(cl);
            }
            cl = vec![line];
        }
    }
    if !cl.is_empty() {
        ret.push(cl);
    }
    ret.into_iter()
}

/// Filter boring lines out of a debdiff
///
/// # Arguments
/// * `debdiff` - The debdiff
/// * `old_version` - The old version
/// * `new_version` - The new version
///
/// # Returns
/// A new debdiff with the boring lines removed
pub fn filter_boring(debdiff: &str, old_version: &str, new_version: &str) -> String {
    let mut ret = Vec::new();
    for (title, paragraph) in iter_sections(debdiff) {
        let title = if let Some(title) = title {
            title
        } else {
            ret.push((
                title,
                paragraph
                    .iter()
                    .map(|line| line.to_string())
                    .collect::<Vec<_>>(),
            ));
            continue;
        };
        let (package, wdiff) = if let Some((_, package)) = lazy_regex::regex_captures!(
            r"Control files of package (.*): lines which differ \(wdiff format\)",
            title
        ) {
            (Some(package), true)
        } else if title == "Control files: lines which differ (wdiff format)" {
            (None, true)
        } else {
            (None, false)
        };
        if wdiff {
            let mut paragraph_unfiltered = Vec::new();
            for lines in iter_fields(paragraph.into_iter()) {
                let newlines = filter_boring_wdiff(lines, old_version, new_version);
                paragraph_unfiltered.extend(newlines);
            }
            let paragraph = paragraph_unfiltered;
            if paragraph.iter().any(|line| !line.trim().is_empty()) {
                ret.push((Some(title), paragraph));
            } else if let Some(package) = package {
                ret.push((
                    None,
                    vec![format!(
                        "No differences were encountered between the control files of package {}",
                        package
                    )],
                ));
            } else {
                ret.push((
                    None,
                    vec!["No differences were encountered in the control files".to_string()],
                ));
            }
        } else {
            ret.push((
                Some(title),
                paragraph
                    .iter()
                    .map(|line| line.to_string())
                    .collect::<Vec<_>>(),
            ));
        }
    }

    let mut lines = vec![];
    for (title, paragraph) in ret {
        if let Some(title) = title {
            lines.push(title.to_string());
            lines.push("-".repeat(title.len()));
        }
        lines.extend(paragraph);
        lines.push("".to_string());
    }
    lines.join("\n")
}

#[derive(Debug)]
pub struct DebdiffError {
    message: String,
}

impl DebdiffError {
    /// The raw error message, without the `DebdiffError:` `Display` prefix.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<tokio::io::Error> for DebdiffError {
    fn from(e: tokio::io::Error) -> Self {
        DebdiffError {
            message: e.to_string(),
        }
    }
}

impl std::fmt::Display for DebdiffError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "DebdiffError: {}", self.message)
    }
}

impl std::error::Error for DebdiffError {}

pub async fn run_debdiff(
    old_binaries: Vec<&str>,
    new_binaries: Vec<&str>,
) -> Result<Vec<u8>, DebdiffError> {
    // Validate all file paths to prevent command injection
    for path in old_binaries.iter().chain(new_binaries.iter()) {
        // Check for shell metacharacters and path traversal
        if path.contains("..")
            || path
                .chars()
                .any(|c| matches!(c, ';' | '&' | '|' | '$' | '`' | '\n' | '\r' | '\0'))
        {
            return Err(DebdiffError {
                message: format!("Invalid file path: {}", path),
            });
        }

        // Verify the path exists and is a regular file
        let path_obj = std::path::Path::new(path);
        if !path_obj.exists() {
            return Err(DebdiffError {
                message: format!("File not found: {}", path),
            });
        }
        if !path_obj.is_file() {
            return Err(DebdiffError {
                message: format!("Not a regular file: {}", path),
            });
        }
    }

    let mut cmd = tokio::process::Command::new("debdiff");
    cmd.arg("--from");
    for path in old_binaries {
        cmd.arg(path);
    }
    cmd.arg("--to");
    for path in new_binaries {
        cmd.arg(path);
    }

    let output = cmd.output().await?;
    // debdiff(1) exit codes: 0 = packages identical, 1 = differences
    // found (the typical, expected outcome). Anything else is a real
    // error (missing file, malformed package, etc). The earlier code
    // treated exit 1 as an error and returned an empty `DebdiffError:
    // ` (because debdiff prints the diff to stdout, not stderr) -
    // every successful debdiff call hit the 503 path.
    let code = output.status.code();
    if code != Some(0) && code != Some(1) {
        return Err(DebdiffError {
            message: format!(
                "debdiff exited with status {:?}: {}",
                code,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(output.stdout)
}

pub fn debdiff_is_empty(debdiff: &str) -> bool {
    !iter_sections(debdiff).any(|(title, _paragraph)| title.is_some())
}

pub fn section_is_wdiff(title: &str) -> (bool, Option<&str>) {
    if let Some((_, package)) = lazy_regex::regex_captures!(
        r"Control files of package (.*): lines which differ \(wdiff format\)",
        title
    ) {
        return (true, Some(package));
    }
    if title == "Control files: lines which differ (wdiff format)" {
        return (true, None);
    }
    (false, None)
}

pub fn markdownify_debdiff(debdiff: &str) -> String {
    let fix_wdiff_md = |line: &str| {
        // GitLab markdown will render links but then not show the
        // delta highlighting. This fools it into not autolinking:
        line.replace("://", "&#8203;://")
    };

    let mut ret = vec![];
    for (title, lines) in iter_sections(debdiff) {
        if let Some(title) = title {
            ret.push(format!("### {}", title));
            let (wdiff, _package) = section_is_wdiff(title);
            if wdiff {
                ret.extend(lines.iter().filter_map(|line| {
                    if line.trim().is_empty() {
                        None
                    } else {
                        Some(format!("* {}", fix_wdiff_md(line)))
                    }
                }));
            } else {
                for line in lines {
                    ret.push(format!("    {}", line));
                }
            }
        } else {
            ret.push("".to_owned());
            for line in lines {
                if !line.trim().is_empty() {
                    let line = lazy_regex::regex_replace!(
                        "^(No differences were encountered between the control files of package) (.*)$",
                        line,
                        r"$1 \*\*$2\*\*",
                    );
                    ret.push(line.to_string());
                } else {
                    ret.push("".to_owned());
                }
            }
            if ret.last() == Some(&String::new()) {
                ret.pop();
            }
        }
    }
    ret.join("\n")
}

pub fn htmlize_debdiff(debdiff: &str) -> String {
    let highlight_wdiff = |line: &str| {
        let line = lazy_regex::regex_replace!(
            r"\[-(.*?)-\]",
            line,
            r#"<span style="color:red;font-weight:bold">$1</span>"#,
        );
        let line = lazy_regex::regex_replace!(
            r"\{\+(.*?)\+\}",
            &*line,
            r#"<span style="color:green;font-weight:bold">$1</span>"#,
        );
        line.into_owned()
    };

    let mut ret = vec![];
    for (title, lines) in iter_sections(debdiff) {
        if let Some(title) = title {
            ret.push(format!("<h4>{}</h4>", title));
            let wdiff = if lazy_regex::regex_is_match!(
                r"Control files of package .*: lines which differ \(wdiff format\)",
                title,
            ) {
                true
            } else {
                title == "Control files: lines which differ (wdiff format)"
            };
            if wdiff {
                ret.push("<ul>".to_owned());
                for mlines in iter_fields(lines.into_iter()) {
                    if mlines.is_empty() {
                        continue;
                    }
                    ret.push(format!(
                        "<li><pre>{}</pre></li>",
                        highlight_wdiff(&mlines.join("\n"))
                    ));
                }
                ret.push("</ul>".to_owned());
            } else {
                ret.push("<pre>".to_owned());
                ret.extend(
                    lines
                        .iter()
                        .map(|line| line.to_string())
                        .collect::<Vec<_>>(),
                );
                ret.push("</pre>".to_owned());
            }
        } else {
            ret.push("<p>".to_owned());
            for line in lines {
                if !line.trim().is_empty() {
                    let line = lazy_regex::regex_replace!(
                        "^(No differences were encountered between the control files of package) (.*)$",
                        line,
                        r"$1 <b>$2</b>",
                    ).to_string();
                    ret.push(line);
                } else {
                    ret.push("</p>".to_owned());
                    ret.push("<p>".to_owned());
                }
            }
            if ret.last().unwrap() == "<p>" {
                ret.pop();
            }
        }
    }
    ret.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_nothing() {
        assert_eq!(
            iter_sections("foo\n").collect::<Vec<_>>(),
            vec![(None, vec!["foo"])]
        );
    }

    #[test]
    fn test_simple() {
        assert_eq!(
        vec![
            (
                None,
                vec![
                    "[The following lists of changes regard files as different if they have",
                    "different names, permissions or owners.]",
                ],
            ),
            (
                Some("Files in second .changes but not in first"),
                vec!["-rw-r--r--  root/root   /usr/lib/debug/.build-id/e4/3520e0f1e.debug"],
            ),
            (
                Some("Files in first .changes but not in second"),
                vec!["-rw-r--r--  root/root   /usr/lib/debug/.build-id/28/0303571bd.debug"],
            ),
            (
                Some("Control files of package xserver-blah: lines which differ (wdiff format)"),
                vec![
                    "Installed-Size: [-174-] {+170+}",
                    "Version: [-1:1.7.9-2~jan+unchanged1-] {+1:1.7.9-3~jan+lint1+}",
                ],
            ),
            (
                Some("Control files of package xserver-dbgsym: lines which differ (wdiff format)"),
                vec![
                    "Build-Ids: [-280303571bd7f8-] {+e43520e0f1eb+}",
                    "Depends: xserver-blah (= [-1:1.7.9-2~jan+unchanged1)-] {+1:1.7.9-3~jan+lint1)+}",
                    "Installed-Size: [-515-] {+204+}",
                    "Version: [-1:1.7.9-2~jan+unchanged1-] {+1:1.7.9-3~jan+lint1+}",
                ],
            ),
        ], iter_sections(r#"[The following lists of changes regard files as different if they have
different names, permissions or owners.]

Files in second .changes but not in first
-----------------------------------------
-rw-r--r--  root/root   /usr/lib/debug/.build-id/e4/3520e0f1e.debug

Files in first .changes but not in second
-----------------------------------------
-rw-r--r--  root/root   /usr/lib/debug/.build-id/28/0303571bd.debug

Control files of package xserver-blah: lines which differ (wdiff format)
------------------------------------------------------------------------
Installed-Size: [-174-] {+170+}
Version: [-1:1.7.9-2~jan+unchanged1-] {+1:1.7.9-3~jan+lint1+}

Control files of package xserver-dbgsym: lines which differ (wdiff format)
--------------------------------------------------------------------------
Build-Ids: [-280303571bd7f8-] {+e43520e0f1eb+}
Depends: xserver-blah (= [-1:1.7.9-2~jan+unchanged1)-] {+1:1.7.9-3~jan+lint1)+}
Installed-Size: [-515-] {+204+}
Version: [-1:1.7.9-2~jan+unchanged1-] {+1:1.7.9-3~jan+lint1+}
"#).collect::<Vec<_>>());
    }

    #[test]
    fn test_just_versions() {
        let debdiff = r#"File lists identical (after any substitutions)

Control files of package acpi-fakekey: lines which differ (wdiff format)
------------------------------------------------------------------------
Version: [-0.143-4~jan+unchanged1-] {+0.143-5~jan+lint1+}

Control files of package acpi-fakekey-dbgsym: lines which differ (wdiff format)
-------------------------------------------------------------------------------
Depends: acpi-fakekey (= [-0.143-4~jan+unchanged1)-] {+0.143-5~jan+lint1)+}
Version: [-0.143-4~jan+unchanged1-] {+0.143-5~jan+lint1+}

Control files of package acpi-support: lines which differ (wdiff format)
------------------------------------------------------------------------
Version: [-0.143-4~jan+unchanged1-] {+0.143-5~jan+lint1+}

Control files of package acpi-support-base: lines which differ (wdiff format)
-----------------------------------------------------------------------------
Version: [-0.143-4~jan+unchanged1-] {+0.143-5~jan+lint1+}
"#;
        let newdebdiff =
            super::filter_boring(debdiff, "0.143-4~jan+unchanged1", "0.143-5~jan+lint1");
        assert_eq!(
            newdebdiff,
            r#"File lists identical (after any substitutions)

No differences were encountered between the control files of package acpi-fakekey

No differences were encountered between the control files of package acpi-fakekey-dbgsym

No differences were encountered between the control files of package acpi-support

No differences were encountered between the control files of package acpi-support-base
"#
        );
    }

    #[test]
    fn test_iter_sections() {
        let debdiff = r#"[The following lists of changes regard files as different if they have
different names, permissions or owners.]

Files in second set of .debs but not in first
---------------------------------------------
-rw-r--r--  root/root   /usr/lib/debug/.build-id/6b/b4fbad299b8e4159451a97bbbb3a3367c6084f.debug
-rw-r--r--  root/root   /usr/lib/debug/.build-id/c3/83853da545bcc080058e063998c4c887b8a54d.debug

Files in first set of .debs but not in second
---------------------------------------------
-rw-r--r--  root/root   /usr/lib/debug/.build-id/b6/c575e21b54becc213ef29458a11cdb7f12cbd6.debug
-rw-r--r--  root/root   /usr/lib/debug/.build-id/f4/749f468a9f6a13812364388d0091ea6a55e951.debug

Control files of package plasma-thunderbolt: lines which differ (wdiff format)
------------------------------------------------------------------------------
Version: [-5.25.4-1~jan+unchanged1-] {+5.25.4-2~jan+lint1+}

Control files of package plasma-thunderbolt-dbgsym: lines which differ (wdiff format)
-------------------------------------------------------------------------------------
Build-Ids: 3b45f929fe8e39e2c1dcf5832eca81b2c2dd3cf3 [-b6c575e21b54becc213ef29458a11cdb7f12cbd6 f4749f468a9f6a13812364388d0091ea6a55e951-] {+6bb4fbad299b8e4159451a97bbbb3a3367c6084f c383853da545bcc080058e063998c4c887b8a54d+}
Depends: plasma-thunderbolt (= [-5.25.4-1~jan+unchanged1)-] {+5.25.4-2~jan+lint1)+}
Version: [-5.25.4-1~jan+unchanged1-] {+5.25.4-2~jan+lint1+}
"#;
        assert_eq!(iter_sections(debdiff).collect::<Vec<_>>(), vec![
            (
                None,
                vec![
                    "[The following lists of changes regard files as different if they have",
                    "different names, permissions or owners.]",
                ],
            ),
            (
                Some("Files in second set of .debs but not in first"),
                vec![
                    "-rw-r--r--  root/root   /usr/lib/debug/.build-id/6b/b4fbad299b8e4159451a97bbbb3a3367c6084f.debug",
                    "-rw-r--r--  root/root   /usr/lib/debug/.build-id/c3/83853da545bcc080058e063998c4c887b8a54d.debug",
                ],
            ),
            (
                Some("Files in first set of .debs but not in second"),
                vec![
                    "-rw-r--r--  root/root   /usr/lib/debug/.build-id/b6/c575e21b54becc213ef29458a11cdb7f12cbd6.debug",
                    "-rw-r--r--  root/root   /usr/lib/debug/.build-id/f4/749f468a9f6a13812364388d0091ea6a55e951.debug",
                ],
            ),
            (
                Some("Control files of package plasma-thunderbolt: lines which differ (wdiff format)"),
                vec![
                    "Version: [-5.25.4-1~jan+unchanged1-] {+5.25.4-2~jan+lint1+}",
                ],
            ),
            (
                Some("Control files of package plasma-thunderbolt-dbgsym: lines which differ (wdiff format)"),
                vec![
                    "Build-Ids: 3b45f929fe8e39e2c1dcf5832eca81b2c2dd3cf3 [-b6c575e21b54becc213ef29458a11cdb7f12cbd6 f4749f468a9f6a13812364388d0091ea6a55e951-] {+6bb4fbad299b8e4159451a97bbbb3a3367c6084f c383853da545bcc080058e063998c4c887b8a54d+}",
                    "Depends: plasma-thunderbolt (= [-5.25.4-1~jan+unchanged1)-] {+5.25.4-2~jan+lint1)+}",
                    "Version: [-5.25.4-1~jan+unchanged1-] {+5.25.4-2~jan+lint1+}",
                ],
            ),
            ]);
    }

    #[test]
    fn test_filter_wdiff_boring() {
        assert_eq!(
            filter_boring_wdiff(
                vec!["Installed-Size: [-174-] {+170+}"],
                "1:1.7.9-2~jan+unchanged1",
                "1:1.7.9-3~jan+lint1"
            ),
            Vec::<String>::new()
        );

        assert_eq!(
            filter_boring_wdiff(
                vec!["Version: [-1:1.7.9-2~jan+unchanged1-] {+1:1.7.9-3~jan+lint1+}"],
                "1:1.7.9-2~jan+unchanged1",
                "1:1.7.9-3~jan+lint1"
            ),
            Vec::<String>::new()
        );

        assert_eq!(
            filter_boring_wdiff(vec!["Depends: xserver-blah (= [-1:1.7.9-2~jan+unchanged1)-] {+1:1.7.9-3~jan+lint1)+}"], "1:1.7.9-2~jan+unchanged1", "1:1.7.9-3~jan+lint1"),
            Vec::<String>::new()
        );
        assert_eq!(
            filter_boring_wdiff(
                vec!["Build-Ids: [-280303571bd7f8-] {+e43520e0f1eb+}"],
                "1.7.9-2~jan+unchanged1",
                "1.7.9-3~jan+lint1"
            ),
            vec!["Build-Ids: [-280303571bd7f8-] {+e43520e0f1eb+}"]
        );
    }

    #[test]
    fn test_iter_fields() {
        assert_eq!(
            iter_fields(
                vec![
                    "foo", " bar", " baz", "qux", " quux", " corge", "grault", " garply", " waldo",
                    "thud",
                ]
                .into_iter()
            )
            .collect::<Vec<_>>(),
            vec![
                vec!["foo", " bar", " baz"],
                vec!["qux", " quux", " corge"],
                vec!["grault", " garply", " waldo"],
                vec!["thud"],
            ]
        );
    }

    #[test]
    fn test_debdiff_is_empty_no_sections() {
        assert_eq!(debdiff_is_empty("just some text\n"), true);
    }

    #[test]
    fn test_debdiff_is_empty_with_sections() {
        let debdiff = "Some Title\n----------\nsome content\n";
        assert_eq!(debdiff_is_empty(debdiff), false);
    }

    #[test]
    fn test_debdiff_is_empty_blank() {
        assert_eq!(debdiff_is_empty(""), true);
    }

    #[test]
    fn test_section_is_wdiff_package() {
        assert_eq!(
            section_is_wdiff(
                "Control files of package mypackage: lines which differ (wdiff format)",
            ),
            (true, Some("mypackage"))
        );
    }

    #[test]
    fn test_section_is_wdiff_no_package() {
        assert_eq!(
            section_is_wdiff("Control files: lines which differ (wdiff format)"),
            (true, None)
        );
    }

    #[test]
    fn test_section_is_wdiff_not_wdiff() {
        assert_eq!(
            section_is_wdiff("Files in second .changes but not in first"),
            (false, None)
        );
    }

    #[test]
    fn test_markdownify_debdiff_basic() {
        let title = "Files in second .changes but not in first";
        let dashes = "-".repeat(title.len());
        let debdiff = format!("{title}\n{dashes}\n-rw-r--r--  root/root   /usr/lib/somefile\n");
        assert_eq!(
            markdownify_debdiff(&debdiff),
            "### Files in second .changes but not in first\n\
             \x20\x20\x20\x20-rw-r--r--  root/root   /usr/lib/somefile"
        );
    }

    #[test]
    fn test_markdownify_debdiff_wdiff_section() {
        let title = "Control files of package foo: lines which differ (wdiff format)";
        let dashes = "-".repeat(title.len());
        let debdiff = format!("{title}\n{dashes}\nVersion: [-1.0-1-] {{+1.0-2+}}\n");
        assert_eq!(
            markdownify_debdiff(&debdiff),
            "### Control files of package foo: lines which differ (wdiff format)\n\
             * Version: [-1.0-1-] {+1.0-2+}"
        );
    }

    #[test]
    fn test_markdownify_debdiff_empty() {
        assert_eq!(markdownify_debdiff(""), "");
    }

    #[test]
    fn test_markdownify_debdiff_plain_paragraph() {
        assert_eq!(
            markdownify_debdiff("File lists identical (after any substitutions)\n"),
            "\nFile lists identical (after any substitutions)"
        );
    }

    #[test]
    fn test_htmlize_debdiff_basic() {
        let title = "Files in second .changes but not in first";
        let dashes = "-".repeat(title.len());
        let debdiff = format!("{title}\n{dashes}\n-rw-r--r--  root/root   /usr/lib/somefile\n");
        assert_eq!(
            htmlize_debdiff(&debdiff),
            "<h4>Files in second .changes but not in first</h4>\n\
             <pre>\n\
             -rw-r--r--  root/root   /usr/lib/somefile\n\
             </pre>"
        );
    }

    #[test]
    fn test_htmlize_debdiff_wdiff_highlighting() {
        let title = "Control files: lines which differ (wdiff format)";
        let dashes = "-".repeat(title.len());
        let debdiff = format!("{title}\n{dashes}\nVersion: [-1.0-] {{+2.0+}}\n");
        assert_eq!(
            htmlize_debdiff(&debdiff),
            "<h4>Control files: lines which differ (wdiff format)</h4>\n\
             <ul>\n\
             <li><pre>Version: <span style=\"color:red;font-weight:bold\">1.0</span> <span style=\"color:green;font-weight:bold\">2.0</span></pre></li>\n\
             </ul>"
        );
    }

    #[test]
    fn test_htmlize_debdiff_empty() {
        assert_eq!(htmlize_debdiff(""), "");
    }

    #[test]
    fn test_filter_boring_no_wdiff() {
        let title = "Files in second .changes but not in first";
        let dashes = "-".repeat(title.len());
        let debdiff = format!("{title}\n{dashes}\n-rw-r--r--  root/root   /usr/lib/somefile\n");
        assert_eq!(
            filter_boring(&debdiff, "1.0-1", "1.0-2"),
            "Files in second .changes but not in first\n\
             -----------------------------------------\n\
             -rw-r--r--  root/root   /usr/lib/somefile\n"
        );
    }

    #[test]
    fn test_filter_boring_wdiff_empty_input() {
        assert_eq!(
            filter_boring_wdiff(vec![], "1.0", "2.0"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_filter_boring_wdiff_no_field_separator() {
        assert_eq!(
            filter_boring_wdiff(vec!["no colon here"], "1.0", "2.0"),
            vec!["no colon here"]
        );
    }

    #[test]
    fn test_iter_sections_empty_string() {
        assert_eq!(
            iter_sections("").collect::<Vec<_>>(),
            Vec::<(Option<&str>, Vec<&str>)>::new()
        );
    }

    #[test]
    fn test_iter_sections_multiple_blank_lines() {
        // An empty line ("") between blank lines is treated as a title candidate
        // because the next blank line has zero length matching "".repeat(0)
        assert_eq!(
            iter_sections("first\n\n\nsecond\n").collect::<Vec<_>>(),
            vec![(None, vec!["first"]), (Some(""), vec!["second"]),]
        );
    }

    #[test]
    fn test_filter_boring_all_boring_control_files() {
        let title = "Control files: lines which differ (wdiff format)";
        let dashes = "-".repeat(title.len());
        let debdiff = format!("{title}\n{dashes}\nVersion: [-1.0-1-] {{+1.0-2+}}\n");
        assert_eq!(
            filter_boring(&debdiff, "1.0-1", "1.0-2"),
            "No differences were encountered in the control files\n"
        );
    }

    #[test]
    fn test_markdownify_debdiff_url_escaping() {
        let title = "Control files: lines which differ (wdiff format)";
        let dashes = "-".repeat(title.len());
        let debdiff = format!("{title}\n{dashes}\nHomepage: [-http://old.example.com-] {{+http://new.example.com+}}\n");
        let md = markdownify_debdiff(&debdiff);
        assert_eq!(
            md,
            "### Control files: lines which differ (wdiff format)\n\
             * Homepage: [-http&#8203;://old.example.com-] {+http&#8203;://new.example.com+}"
        );
    }
}
