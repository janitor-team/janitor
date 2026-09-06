//! Contents-<arch> file generation.
//!
//! A `Contents-<arch>` file maps installed file paths to the packages
//! that provide them, letting apt clients answer "which package
//! contains `/usr/bin/foo`?" quickly. Format (see
//! <https://wiki.debian.org/DebianRepository/Format#A.22Contents.22_indices>):
//!
//! ```text
//! FILE                                                    LOCATION
//! bin/uname                                               utils/coreutils
//! usr/bin/hello                                           main/hello
//! usr/share/doc/foo/copyright                             main/foo,main/bar
//! ```
//!
//! Column 1 is the file path (no leading `/`), left-justified to a
//! nominal width. Column 2 is a comma-separated `section/package`
//! list -- a single file can be shipped by multiple packages, and
//! Contents lists them all on one line.
//!
//! We generate (a) an uncompressed `Contents-<arch>` and (b) a
//! gzipped `Contents-<arch>.gz`, and add both to the Release file
//! with size + hashes so apt clients actually pick them up.

use std::collections::BTreeMap;

/// A single package's contribution to a Contents index: the fully-
/// qualified `section/package` identifier plus the list of file paths
/// it installs (paths already stripped of leading `./` -- use
/// [`crate::deb::list_deb_files`] to produce them).
#[derive(Debug, Clone)]
pub struct ContentsEntry {
    /// `section/package-name`, e.g. `main/hello` or
    /// `contrib/firmware-nvidia`. Contents entries use this exact
    /// two-segment form.
    pub qualified_name: String,
    /// Every installed file path this package ships. Order is
    /// preserved for stability across runs but need not be sorted;
    /// the writer sorts by file path across all entries.
    pub files: Vec<String>,
}

/// Format a Contents-<arch> file from a set of per-package entries.
///
/// Produces the header + one line per distinct file path, with all
/// packages that provide the same path merged on that line
/// (comma-separated). Lines are sorted alphabetically by file path
/// for stable, diffable output.
///
/// The header (`FILE  ...  LOCATION`) is optional per the spec but
/// widely present; apt clients ignore it and it makes the file
/// human-readable.
pub fn format_contents(entries: &[ContentsEntry]) -> String {
    // Reverse index: file path -> set of packages. Using BTreeMap
    // + BTreeSet gives sorted-by-path output for free and
    // de-duplicates packages within a line.
    let mut by_path: BTreeMap<&str, std::collections::BTreeSet<&str>> = BTreeMap::new();
    for entry in entries {
        for file in &entry.files {
            by_path
                .entry(file.as_str())
                .or_default()
                .insert(entry.qualified_name.as_str());
        }
    }

    let mut out = String::new();
    // Column widths mirror what dak (Debian's archive-kit) writes:
    // 55 characters for the path column, then packages.
    out.push_str(&format!("{:<55} {}\n", "FILE", "LOCATION"));
    for (path, pkgs) in by_path {
        let joined = pkgs.into_iter().collect::<Vec<_>>().join(",");
        out.push_str(&format!("{:<55} {}\n", path, joined));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(qualified: &str, files: &[&str]) -> ContentsEntry {
        ContentsEntry {
            qualified_name: qualified.to_string(),
            files: files.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// A single package's files render one line per file, sorted
    /// alphabetically, with the header row at the top.
    #[test]
    fn format_contents_single_package_sorts_files() {
        let out = format_contents(&[entry(
            "main/hello",
            &["usr/bin/hello", "usr/share/doc/hello/copyright"],
        )]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0].trim_start(),
            "FILE                                                    LOCATION"
        );
        assert!(lines[1].starts_with("usr/bin/hello"));
        assert!(lines[1].ends_with(" main/hello"));
        assert!(lines[2].starts_with("usr/share/doc/hello/copyright"));
        assert!(lines[2].ends_with(" main/hello"));
    }

    /// When two packages ship the same file (e.g. an alternative or
    /// a common config path), they must be merged onto one line as
    /// `pkg-a,pkg-b`. The package list within a line must be
    /// alphabetically sorted so output is byte-stable.
    #[test]
    fn format_contents_merges_shared_paths() {
        let out = format_contents(&[
            entry("main/foo", &["usr/share/common/doc"]),
            entry("main/bar", &["usr/share/common/doc"]),
        ]);
        // Header + one merged line = 2 lines total.
        assert_eq!(out.lines().count(), 2);
        let data_line = out.lines().nth(1).unwrap();
        assert!(
            data_line.contains("main/bar,main/foo"),
            "expected comma-joined & sorted packages, got: {:?}",
            data_line
        );
    }

    /// Duplicate paths within a single package (rare but possible)
    /// must collapse to a single entry -- the underlying BTreeSet
    /// dedupes automatically.
    #[test]
    fn format_contents_dedupes_within_package() {
        let out = format_contents(&[entry(
            "main/foo",
            &["usr/bin/foo", "usr/bin/foo", "usr/bin/foo"],
        )]);
        assert_eq!(out.lines().count(), 2); // header + 1 file
    }

    /// Empty input yields just the header. This is what should
    /// appear when a suite has no builds yet -- apt clients accept
    /// an empty Contents.
    #[test]
    fn format_contents_empty_is_header_only() {
        let out = format_contents(&[]);
        assert_eq!(out.lines().count(), 1);
        assert!(out.starts_with("FILE"));
    }

    /// Paths must render byte-verbatim: spaces stay, dots stay, dashes
    /// stay. dak-generated Contents files do the same.
    #[test]
    fn format_contents_preserves_special_characters() {
        let out = format_contents(&[entry(
            "main/foo",
            &[
                "usr/share/foo/file with spaces.txt",
                "usr/share/foo/дата.txt",
            ],
        )]);
        assert!(out.contains("usr/share/foo/file with spaces.txt"));
        assert!(out.contains("usr/share/foo/дата.txt"));
    }

    /// Output must be sorted primarily by file path across all
    /// entries -- not by package. Guards against a future refactor
    /// that inserts packages into the map in package-name order.
    #[test]
    fn format_contents_output_is_sorted_by_path() {
        let out = format_contents(&[
            entry("main/z-pkg", &["a/first"]),
            entry("main/a-pkg", &["z/last"]),
        ]);
        let paths: Vec<&str> = out
            .lines()
            .skip(1) // header
            .filter_map(|l| l.split_whitespace().next())
            .collect();
        assert_eq!(paths, vec!["a/first", "z/last"]);
    }
}
