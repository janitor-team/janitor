use buildlog_consultant::common::find_build_failure_description;
use std::io::Read;

pub struct AnalyzedLog {
    pub code: String,
    pub description: String,
    pub phase: Option<String>,
    pub failure_details: Option<serde_json::Value>,
}

pub type AnalyzeLogFn<R> = fn(R) -> AnalyzedLog;

/// Read everything, replacing invalid UTF-8 rather than failing on it.
///
/// Build logs are not reliably UTF-8.
fn read_lossy<R: std::io::Read>(mut logf: R) -> String {
    let mut buf = Vec::new();
    if logf.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn read_lines_lossy<R: std::io::Read>(logf: R) -> Vec<String> {
    read_lossy(logf).lines().map(|l| l.to_string()).collect()
}

pub trait AnalyzeFn<R: std::io::Read> {
    fn analyze(&self, logf: R) -> AnalyzedLog;
}

pub fn process_dist_log<R: std::io::Read>(logf: R) -> AnalyzedLog {
    let lines = read_lines_lossy(logf);
    let problem =
        find_build_failure_description(lines.iter().map(|l| l.as_str()).collect::<Vec<_>>()).1;
    let (new_code, new_description, new_failure_details) = if let Some(problem) = problem {
        let new_code = if problem.is_universal() {
            problem.kind().to_string()
        } else {
            format!("dist-{}", problem.kind())
        };
        let new_description = problem.to_string();
        let new_failure_details = problem.json();
        (new_code, new_description, Some(new_failure_details))
    } else {
        (
            "dist-command-failed".to_string(),
            "Dist command failed".to_string(),
            None,
        )
    };
    let new_phase = None;
    AnalyzedLog {
        code: new_code,
        description: new_description,
        phase: new_phase,
        failure_details: new_failure_details,
    }
}

pub fn process_build_log<R: std::io::Read>(logf: R) -> AnalyzedLog {
    let lines = read_lines_lossy(logf);
    let (r#match, problem) =
        find_build_failure_description(lines.iter().map(|l| l.as_str()).collect::<Vec<_>>());
    let (new_code, new_failure_details) = if let Some(problem) = problem.as_ref() {
        let new_code = problem.kind().to_string();
        let new_failure_details = problem.json();
        (new_code, Some(new_failure_details))
    } else {
        ("build-failed".to_string(), None)
    };

    let new_description = if let Some(r#match) = r#match {
        r#match.line().to_string()
    } else if let Some(problem) = problem.as_ref() {
        problem.to_string()
    } else {
        "Build failed".to_string()
    };

    AnalyzedLog {
        code: new_code,
        description: new_description,
        phase: Some("build".to_owned()),
        failure_details: new_failure_details,
    }
}

pub fn process_sbuild_log<R: std::io::Read>(logf: R) -> AnalyzedLog {
    // FromStr, because read_lossy hands back a String. TryFrom would take
    // the reader and decode it strictly.
    let sbuildlog = match read_lossy(logf).parse::<buildlog_consultant::sbuild::SbuildLog>() {
        Ok(l) => l,
        Err(e) => {
            return AnalyzedLog {
                code: "build-failed".to_string(),
                description: format!("unparseable sbuild log: {}", e),
                phase: None,
                failure_details: None,
            }
        }
    };
    let failure = buildlog_consultant::sbuild::worker_failure_from_sbuild_log(&sbuildlog);

    let (new_code, new_failure_details) = if let Some(error) = failure.error.as_ref() {
        let new_code = if let Some(failure_stage) = failure.stage.as_ref() {
            if !error.is_universal() {
                format!("{}-{}", failure_stage, error.kind())
            } else {
                error.kind().to_string()
            }
        } else {
            error.kind().to_string()
        };

        let new_failure_details = error.json();
        (new_code, Some(new_failure_details))
    } else if let Some(failure_stage) = failure.stage.as_ref() {
        let new_code = format!("build-failed-stage-{}", failure_stage);
        let new_failure_details = None;
        (new_code, new_failure_details)
    } else {
        let new_code = "build-failed".to_string();
        let new_failure_details = None;
        (new_code, new_failure_details)
    };

    let new_description = if let Some(error) = failure.error.as_ref() {
        error.to_string()
    } else {
        "Build failed".to_string()
    };

    let new_phase = failure.stage.map(|s| s.to_string());

    AnalyzedLog {
        code: new_code,
        description: new_description,
        phase: new_phase,
        failure_details: new_failure_details,
    }
}
