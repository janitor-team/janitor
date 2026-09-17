//! Extract file listings from `.deb` binary packages.
//!
//! Used by the Contents-<arch> generator. A `.deb` file is an `ar`
//! archive with three members:
//!
//!   1. `debian-binary` -- the format version marker
//!   2. `control.tar.*` -- package metadata
//!   3. `data.tar.*` -- the installed file tree
//!
//! `data.tar.*` may be uncompressed or compressed with gzip, bzip2,
//! xz, or zstd. We identify by the member's filename extension.
//!
//! Parsing in-process is cheaper than shelling out to `dpkg-deb -c`
//! per package (a few thousand packages per publish adds up), works
//! on minimal container images, and keeps errors typed rather than
//! parsed from stderr.

use crate::error::{ArchiveError, ArchiveResult};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

/// List every regular-file path installed by a `.deb` archive.
///
/// Paths are returned *without* the leading `./` that `tar` stores
/// them under, matching what apt clients expect in a `Contents-<arch>`
/// entry (e.g. `usr/bin/hello`, not `./usr/bin/hello`). Directory
/// entries and hard/symlinks-pointing-elsewhere are skipped;
/// symlinks are included as regular file entries because apt's
/// Contents file lists all files a package installs regardless of
/// their filesystem type.
pub fn list_deb_files(deb_path: &Path) -> ArchiveResult<Vec<String>> {
    let file = File::open(deb_path).map_err(ArchiveError::Io)?;
    let mut ar = ar::Archive::new(BufReader::new(file));

    // Walk the ar members until we find the data.tar.*.
    while let Some(entry) = ar.next_entry() {
        let entry = entry.map_err(|e| {
            ArchiveError::PackageScanning(format!(
                "reading ar entry in {}: {}",
                deb_path.display(),
                e
            ))
        })?;
        let name = String::from_utf8_lossy(entry.header().identifier()).into_owned();
        if !name.starts_with("data.tar") {
            continue;
        }
        return read_data_tar(&name, entry, deb_path);
    }

    Err(ArchiveError::PackageScanning(format!(
        "no data.tar.* member in {}",
        deb_path.display()
    )))
}

/// Wrap the given `data.tar.*` reader in the right decompressor
/// based on the ar member's filename, then walk the tar entries and
/// collect regular-file paths.
fn read_data_tar<R: Read>(name: &str, reader: R, deb_path: &Path) -> ArchiveResult<Vec<String>> {
    let boxed: Box<dyn Read> = match name {
        "data.tar" => Box::new(reader),
        "data.tar.gz" => Box::new(flate2::read::GzDecoder::new(reader)),
        "data.tar.bz2" => Box::new(bzip2::read::BzDecoder::new(reader)),
        "data.tar.xz" => Box::new(xz2::read::XzDecoder::new(reader)),
        "data.tar.zst" | "data.tar.zstd" => Box::new(
            zstd::stream::Decoder::new(reader)
                .map_err(|e| ArchiveError::PackageScanning(format!("zstd init: {}", e)))?,
        ),
        other => {
            return Err(ArchiveError::PackageScanning(format!(
                "unknown data.tar compression '{}' in {}",
                other,
                deb_path.display()
            )));
        }
    };

    let mut tar = tar::Archive::new(boxed);
    let mut out = Vec::new();
    for entry in tar
        .entries()
        .map_err(|e| ArchiveError::PackageScanning(format!("tar entries: {}", e)))?
    {
        let entry =
            entry.map_err(|e| ArchiveError::PackageScanning(format!("tar entry: {}", e)))?;
        // Skip directory entries; Contents-<arch> lists files, not
        // dirs. Include symlinks and hardlinks -- apt clients treat
        // them as owned by the package.
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|e| ArchiveError::PackageScanning(format!("tar path: {}", e)))?;
        let s = path.to_string_lossy();
        // tar stores paths as `./usr/bin/foo`; strip the leading
        // `./` (or `/`) so the output matches `dpkg-deb -c | awk
        // '{print $NF}' | sed 's|^\./||'`.
        let cleaned = s
            .strip_prefix("./")
            .or_else(|| s.strip_prefix('/'))
            .unwrap_or(&s);
        if cleaned.is_empty() {
            continue;
        }
        out.push(cleaned.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a minimal in-memory `.deb` file at `path` whose
    /// `data.tar.gz` member contains the listed paths as
    /// zero-length regular files. Directory entries under
    /// `paths_dirs` are added as dir entries so we can test that
    /// they get filtered out.
    fn write_minimal_deb(
        path: &Path,
        paths_files: &[&str],
        paths_dirs: &[&str],
        compression: &str,
    ) {
        // First build the inner data.tar
        let mut tar_bytes: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for dir in paths_dirs {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Directory);
                header.set_path(dir).unwrap();
                header.set_size(0);
                header.set_mode(0o755);
                header.set_cksum();
                builder.append(&header, std::io::empty()).unwrap();
            }
            for p in paths_files {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Regular);
                header.set_path(p).unwrap();
                header.set_size(0);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append(&header, std::io::empty()).unwrap();
            }
            builder.finish().unwrap();
        }

        // Compress it if requested
        let (data_name, data_bytes): (&str, Vec<u8>) = match compression {
            "gz" => {
                let mut w =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                w.write_all(&tar_bytes).unwrap();
                ("data.tar.gz", w.finish().unwrap())
            }
            "xz" => {
                let mut w = xz2::write::XzEncoder::new(Vec::new(), 6);
                w.write_all(&tar_bytes).unwrap();
                ("data.tar.xz", w.finish().unwrap())
            }
            "zst" => {
                let mut buf = Vec::new();
                zstd::stream::copy_encode(std::io::Cursor::new(&tar_bytes), &mut buf, 3).unwrap();
                ("data.tar.zst", buf)
            }
            "" => ("data.tar", tar_bytes),
            other => panic!("unsupported compression {}", other),
        };

        // Wrap the members in an ar archive.
        let file = File::create(path).unwrap();
        let mut ar_builder = ar::Builder::new(file);
        ar_builder
            .append(
                &ar::Header::new(b"debian-binary".to_vec(), 4),
                b"2.0\n" as &[u8],
            )
            .unwrap();
        ar_builder
            .append(
                &ar::Header::new(b"control.tar.gz".to_vec(), 20),
                &b"placeholder-control!"[..],
            )
            .unwrap();
        ar_builder
            .append(
                &ar::Header::new(data_name.as_bytes().to_vec(), data_bytes.len() as u64),
                data_bytes.as_slice(),
            )
            .unwrap();
    }

    #[test]
    fn list_deb_files_extracts_gz_data_tar() {
        let tmp = tempfile::tempdir().unwrap();
        let deb = tmp.path().join("hello.deb");
        write_minimal_deb(
            &deb,
            &["./usr/bin/hello", "./usr/share/doc/hello/copyright"],
            &["./usr/", "./usr/bin/"],
            "gz",
        );
        let mut files = list_deb_files(&deb).unwrap();
        files.sort();
        assert_eq!(
            files,
            vec![
                "usr/bin/hello".to_string(),
                "usr/share/doc/hello/copyright".to_string(),
            ]
        );
    }

    #[test]
    fn list_deb_files_extracts_xz_data_tar() {
        let tmp = tempfile::tempdir().unwrap();
        let deb = tmp.path().join("hello.deb");
        write_minimal_deb(&deb, &["./usr/bin/hello"], &["./usr/"], "xz");
        let files = list_deb_files(&deb).unwrap();
        assert_eq!(files, vec!["usr/bin/hello".to_string()]);
    }

    #[test]
    fn list_deb_files_extracts_zst_data_tar() {
        let tmp = tempfile::tempdir().unwrap();
        let deb = tmp.path().join("hello.deb");
        write_minimal_deb(&deb, &["./usr/bin/hello"], &["./usr/"], "zst");
        let files = list_deb_files(&deb).unwrap();
        assert_eq!(files, vec!["usr/bin/hello".to_string()]);
    }

    #[test]
    fn list_deb_files_extracts_uncompressed_data_tar() {
        let tmp = tempfile::tempdir().unwrap();
        let deb = tmp.path().join("hello.deb");
        write_minimal_deb(&deb, &["./usr/bin/hello"], &["./usr/"], "");
        let files = list_deb_files(&deb).unwrap();
        assert_eq!(files, vec!["usr/bin/hello".to_string()]);
    }

    /// Directory entries must be filtered out -- Contents-<arch>
    /// only lists regular files. Guards against a future refactor
    /// that includes `is_dir()` entries and clutters Contents with
    /// spurious rows.
    #[test]
    fn list_deb_files_filters_directory_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let deb = tmp.path().join("hello.deb");
        write_minimal_deb(
            &deb,
            &["./usr/bin/hello"],
            &["./usr/", "./usr/bin/", "./usr/share/", "./usr/share/doc/"],
            "gz",
        );
        let files = list_deb_files(&deb).unwrap();
        assert_eq!(files, vec!["usr/bin/hello".to_string()]);
    }

    /// Filenames containing spaces must survive intact. tar stores
    /// them verbatim; parse_dpkg_deb_line in the old code took the
    /// last whitespace-separated token and truncated them.
    #[test]
    fn list_deb_files_preserves_spaces_in_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let deb = tmp.path().join("hello.deb");
        write_minimal_deb(
            &deb,
            &["./usr/share/hello/file with spaces.txt"],
            &["./usr/", "./usr/share/", "./usr/share/hello/"],
            "gz",
        );
        let files = list_deb_files(&deb).unwrap();
        assert_eq!(
            files,
            vec!["usr/share/hello/file with spaces.txt".to_string()]
        );
    }

    /// Missing `data.tar.*` member -> typed error, not panic. Some
    /// .deb variants (e.g. udebs, malformed packages) may have
    /// unusual structure; the caller must be able to skip them
    /// gracefully.
    #[test]
    fn list_deb_files_missing_data_tar_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let deb = tmp.path().join("bogus.deb");
        // Write an ar archive that has debian-binary + control.tar but
        // no data.tar.*
        let file = File::create(&deb).unwrap();
        let mut ar_builder = ar::Builder::new(file);
        ar_builder
            .append(
                &ar::Header::new(b"debian-binary".to_vec(), 4),
                b"2.0\n" as &[u8],
            )
            .unwrap();
        ar_builder
            .append(
                &ar::Header::new(b"control.tar.gz".to_vec(), 4),
                b"junk" as &[u8],
            )
            .unwrap();
        drop(ar_builder);
        let err = list_deb_files(&deb).unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("no data.tar"), "got: {}", msg);
    }
}
