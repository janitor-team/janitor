use buildlog_consultant::common::find_build_failure_description;
use buildlog_consultant::r#match::Error;

/// The result of analyzing a build log.
#[derive(Debug, PartialEq, Eq)]
pub struct AnalyzeResult {
    /// The total number of lines in the build log.
    pub total_lines: usize,

    /// The range of lines to include in the output.
    pub include_lines: (usize, usize),

    /// The line numbers to highlight in the output.
    pub highlight_lines: Option<Vec<usize>>,
}

/// Find the failure in a dist.log file.
pub fn find_dist_log_failure(logf: &str, length: usize) -> Result<AnalyzeResult, Error> {
    let lines = logf.lines().collect::<Vec<&str>>();
    let failure = find_build_failure_description(&lines)?;
    let highlight_lines = failure.map(|f| f.r#match.linenos());

    let include_lines = (lines.len().saturating_sub(length).max(1), lines.len());

    Ok(AnalyzeResult {
        total_lines: lines.len(),
        include_lines,
        highlight_lines,
    })
}

/// Find the build failure in the given build log.
///
/// Returns:
/// - The total number of lines in the build log.
/// - The range of lines to include in the output.
/// - The line numbers to highlight in the output.
pub fn find_build_log_failure<R: std::io::Read>(
    logf: R,
    length: usize,
) -> Result<AnalyzeResult, Error> {
    let bufread = std::io::BufReader::new(logf);
    let sbuildlog = buildlog_consultant::sbuild::SbuildLog::try_from(bufread).unwrap();
    let linecount = sbuildlog.sections().last().map(|s| s.offsets.1).unwrap();
    let failure = buildlog_consultant::sbuild::worker_failure_from_sbuild_log(&sbuildlog)?;

    if failure.r#match.is_some() && failure.section.is_some() {
        let section = failure.section.as_ref().unwrap();
        let r#match = failure.r#match.as_ref().unwrap();
        let abs_offset = section.offsets.0 + r#match.lineno();
        let include_lines = (
            abs_offset.saturating_sub(length / 2).max(1),
            abs_offset + std::cmp::min(length / 2, section.lines.len()),
        );
        let highlight_lines = vec![abs_offset];
        return Ok(AnalyzeResult {
            total_lines: linecount,
            include_lines,
            highlight_lines: Some(highlight_lines),
        });
    }

    if let Some(r#match) = failure.r#match.as_ref() {
        let include_lines = (
            r#match.lineno().saturating_sub(length / 2).max(1),
            r#match.lineno() + std::cmp::min(length / 2, linecount),
        );
        let highlight_lines = vec![r#match.lineno()];
        return Ok(AnalyzeResult {
            total_lines: linecount,
            include_lines,
            highlight_lines: Some(highlight_lines),
        });
    }

    let include_lines = if let Some(section) = failure.section.as_ref() {
        (
            section.offsets.1.saturating_sub(length).max(1),
            Some(section.offsets.1),
        )
    } else if length < linecount {
        (linecount - length, None)
    } else {
        (1, Some(linecount))
    };

    Ok(AnalyzeResult {
        total_lines: linecount,
        include_lines: (include_lines.0, include_lines.1.unwrap_or(linecount)),
        highlight_lines: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_build_log_failure_short_log_with_match() {
        let sep = format!("+{}+", "-".repeat(78));
        let log = format!(
            "{sep}\n| Build |\n{sep}\n\
             sl.c:41:10: fatal error: curses.h: No such file or directory\n\
             make: *** [Makefile:13: sl] Error 1\n\
             {sep}\n| Summary |\n{sep}\nFail-Stage: build\n"
        );
        assert_eq!(
            find_build_log_failure(log.as_bytes(), 15).unwrap(),
            AnalyzeResult {
                total_lines: 9,
                include_lines: (1, 6),
                highlight_lines: Some(vec![4]),
            }
        );
    }

    #[test]
    fn test_find_build_log_failure_short_log_without_match() {
        assert_eq!(
            find_build_log_failure(&b"hello\nworld\n"[..], 15).unwrap(),
            AnalyzeResult {
                total_lines: 2,
                include_lines: (1, 2),
                highlight_lines: None,
            }
        );
    }

    #[test]
    fn test_find_dist_log_failure_short_log() {
        assert_eq!(
            find_dist_log_failure("hello\nworld\n", 15).unwrap(),
            AnalyzeResult {
                total_lines: 2,
                include_lines: (1, 2),
                highlight_lines: None,
            }
        );
    }
}
