//! Clean up owned repositories that are no longer needed for merge proposals.
//!
//! This is necessary in particular because some hosting sites
//! (e.g. default GitLab) have restrictions on the number of repositories
//! that a single user can own (in the case of GitLab, 1000).

use clap::Parser;
use pyo3::prelude::*;
use std::collections::HashSet;
use std::process::ExitCode;

#[derive(Parser)]
struct Args {
    #[clap(long)]
    /// Only report what would be deleted.
    dry_run: bool,

    #[clap(flatten)]
    logging: janitor::logging::LoggingArgs,
}

/// The ways a forge can fail that this tool tells apart.
#[derive(Debug, PartialEq, Eq)]
enum Error {
    /// The forge does not support the operation.
    Unsupported(String),
    /// The forge has no credentials, or a dependency of it is missing.
    Unavailable(String),
    /// The project does not exist.
    NoSuchProject(String),
    /// Anything else.
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoSuchProject(project) => write!(f, "No such project: {}", project),
            Error::Unsupported(msg) | Error::Unavailable(msg) | Error::Other(msg) => {
                write!(f, "{}", msg)
            }
        }
    }
}

/// Access to the projects and merge proposals of one forge instance.
trait ForgeProjects {
    /// Name of the forge instance, for messages.
    fn name(&self) -> String;

    /// Name of the logged in user, if any.
    fn current_user(&self) -> Result<Option<String>, Error>;

    /// Source project of every proposal that is neither closed nor merged.
    fn open_proposal_sources(&self) -> Result<Vec<Result<Option<String>, Error>>, Error>;

    /// Forks owned by the current user.
    fn my_forks(&self) -> Result<Vec<String>, Error>;

    /// Delete a project.
    fn delete_project(&self, project: &str) -> Result<(), Error>;
}

/// What happened on one forge instance.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The forge was processed; `removed` lists what was (or would be) deleted.
    Cleaned {
        removed: Vec<String>,
        failed: Vec<String>,
    },
    /// The forge does not support this or is not available; that is not a failure.
    Skipped,
    /// The projects in use could not be determined, so nothing was deleted.
    Failed,
}

impl Outcome {
    fn is_failure(&self) -> bool {
        match self {
            Outcome::Cleaned { failed, .. } => !failed.is_empty(),
            Outcome::Skipped => false,
            Outcome::Failed => true,
        }
    }
}

fn is_unsupported(e: &Error) -> bool {
    matches!(e, Error::Unsupported(_))
}

fn is_skippable(e: &Error) -> bool {
    matches!(e, Error::Unsupported(_) | Error::Unavailable(_))
}

/// Projects that back a live proposal; a source project that is gone protects nothing.
fn in_use_projects(sources: Vec<Result<Option<String>, Error>>) -> Result<HashSet<String>, Error> {
    let mut in_use = HashSet::new();
    for source in sources {
        match source {
            Ok(Some(project)) => {
                in_use.insert(project);
            }
            Ok(None) | Err(Error::NoSuchProject(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(in_use)
}

/// Own forks that no live proposal uses, in the order the forge lists them.
fn projects_to_remove(forge: &dyn ForgeProjects) -> Result<Vec<String>, Error> {
    let user = forge
        .current_user()?
        .ok_or_else(|| Error::Unavailable("not logged in".to_string()))?;
    let sources = forge.open_proposal_sources()?;
    let live = sources.len();
    let in_use = in_use_projects(sources)?;
    let mut seen = HashSet::new();
    let candidates: Vec<String> = forge
        .my_forks()?
        .into_iter()
        .filter(|project| {
            project
                .split_once('/')
                .is_some_and(|(owner, _)| owner == user)
                && !in_use.contains(project)
                && seen.insert(project.clone())
        })
        .collect();
    if live > 0 && in_use.is_empty() && !candidates.is_empty() {
        return Err(Error::Other(format!(
            "none of the {} open proposals has a source project",
            live
        )));
    }
    Ok(candidates)
}

/// Delete (or with `dry_run` only report) the unused forks on one forge.
fn cleanup_forge(forge: &dyn ForgeProjects, dry_run: bool) -> Outcome {
    let name = forge.name();
    let candidates = match projects_to_remove(forge) {
        Ok(candidates) => candidates,
        Err(e) if is_skippable(&e) => {
            log::warn!("Skipping {}: {}", name, e);
            return Outcome::Skipped;
        }
        Err(e) => {
            log::error!(
                "Unable to determine the projects in use on {}, not deleting anything: {}",
                name,
                e
            );
            return Outcome::Failed;
        }
    };

    let mut removed = Vec::new();
    let mut failed = Vec::new();
    for project in candidates {
        if dry_run {
            log::info!("Would delete {} from {}", project, name);
            removed.push(project);
            continue;
        }
        log::info!("Deleting {} from {}", project, name);
        match forge.delete_project(&project) {
            Ok(()) => removed.push(project),
            Err(e) if is_unsupported(&e) => {
                log::warn!("Skipping {} that can not delete: {}", name, e);
                if failed.is_empty() {
                    return Outcome::Skipped;
                }
                break;
            }
            Err(e) => {
                log::error!("Failed to delete {} from {}: {}", project, name, e);
                failed.push(project);
            }
        }
    }

    log::info!(
        "{}: {} {}, {} failed",
        name,
        removed.len(),
        if dry_run {
            "would be deleted"
        } else {
            "deleted"
        },
        failed.len()
    );
    Outcome::Cleaned { removed, failed }
}

/// Clean up every instance; returns whether anything failed.
fn cleanup_all<F: ForgeProjects>(
    instances: Vec<Result<F, (String, Error)>>,
    dry_run: bool,
) -> bool {
    if instances.is_empty() {
        log::warn!("No forge instances found");
    }
    let mut failed = false;
    for instance in instances {
        match instance {
            Ok(forge) => failed |= cleanup_forge(&forge, dry_run).is_failure(),
            Err((kind, e)) if is_skippable(&e) => {
                log::warn!("Skipping the {} forges: {}", kind, e);
            }
            Err((kind, e)) => {
                log::error!("Unable to list the {} forges: {}", kind, e);
                failed = true;
            }
        }
    }
    failed
}

// Forge access through Python; this can move to breezyshim's own bindings once released.
pyo3::import_exception!(breezy.forge, NoSuchProject);
pyo3::import_exception!(breezy.forge, UnsupportedForge);
pyo3::import_exception!(breezy.errors, DependencyNotPresent);

struct PyForge(Py<PyAny>);

fn py_instances_of(forges: &Bound<'_, PyAny>, kind: &str) -> PyResult<Vec<PyForge>> {
    forges
        .call_method1("get", (kind,))?
        .call_method0("iter_instances")?
        .try_iter()?
        .map(|instance| Ok(PyForge(instance?.unbind())))
        .collect()
}

/// Instances of every kind of forge; a kind that can not be listed does not hide the others.
fn py_registered_instances(forges: &Bound<'_, PyAny>) -> Vec<Result<PyForge, (String, Error)>> {
    let py = forges.py();
    let kinds = forges.call_method0("keys").and_then(|keys| {
        keys.try_iter()?
            .map(|kind| kind?.extract::<String>())
            .collect::<PyResult<Vec<_>>>()
    });
    let kinds = match kinds {
        Ok(kinds) => kinds,
        Err(e) => return vec![Err(("registered".to_string(), py_error(py, e)))],
    };
    let mut instances = Vec::new();
    for kind in kinds {
        match py_instances_of(forges, &kind) {
            Ok(found) => instances.extend(found.into_iter().map(Ok)),
            Err(e) => instances.push(Err((kind, py_error(py, e)))),
        }
    }
    instances
}

fn py_forge_instances() -> Vec<Result<PyForge, (String, Error)>> {
    Python::attach(
        |py| match py.import("breezy.forge").and_then(|m| m.getattr("forges")) {
            Ok(forges) => py_registered_instances(&forges),
            Err(e) => vec![Err(("registered".to_string(), py_error(py, e)))],
        },
    )
}

/// Classify a Python exception without relying on the shape of its attributes.
fn py_error(py: Python<'_>, e: PyErr) -> Error {
    if e.is_instance_of::<NoSuchProject>(py) {
        let project = e
            .value(py)
            .getattr("project")
            .and_then(|p| p.str())
            .map(|p| p.to_string())
            .unwrap_or_default();
        Error::NoSuchProject(project)
    } else if e.is_instance_of::<UnsupportedForge>(py)
        || e.is_instance_of::<pyo3::exceptions::PyNotImplementedError>(py)
    {
        Error::Unsupported(e.to_string())
    } else if e.is_instance_of::<DependencyNotPresent>(py) {
        Error::Unavailable(e.to_string())
    } else {
        Error::Other(e.to_string())
    }
}

fn py_is_live(mp: &Bound<'_, PyAny>) -> PyResult<bool> {
    Ok(
        !mp.call_method0("is_closed")?.is_truthy()?
            && !mp.call_method0("is_merged")?.is_truthy()?,
    )
}

fn py_source_project(mp: &Bound<'_, PyAny>) -> PyResult<Option<String>> {
    mp.call_method0("get_source_project")?.extract()
}

impl ForgeProjects for PyForge {
    fn name(&self) -> String {
        Python::attach(|py| {
            let forge = self.0.bind(py);
            match forge.repr() {
                Ok(repr) => repr.to_string(),
                Err(_) => forge.get_type().to_string(),
            }
        })
    }

    fn current_user(&self) -> Result<Option<String>, Error> {
        Python::attach(|py| {
            self.0
                .bind(py)
                .call_method0("get_current_user")
                .and_then(|user| user.extract())
                .map_err(|e| py_error(py, e))
        })
    }

    fn open_proposal_sources(&self) -> Result<Vec<Result<Option<String>, Error>>, Error> {
        Python::attach(|py| {
            let mut sources = Vec::new();
            let proposals = self
                .0
                .bind(py)
                .call_method0("iter_my_proposals")
                .and_then(|proposals| proposals.try_iter())
                .map_err(|e| py_error(py, e))?;
            for mp in proposals {
                let mp = mp.map_err(|e| py_error(py, e))?;
                if !py_is_live(&mp).map_err(|e| py_error(py, e))? {
                    continue;
                }
                sources.push(py_source_project(&mp).map_err(|e| py_error(py, e)));
            }
            Ok(sources)
        })
    }

    fn my_forks(&self) -> Result<Vec<String>, Error> {
        Python::attach(|py| {
            let mut forks = Vec::new();
            let iter = self
                .0
                .bind(py)
                .call_method0("iter_my_forks")
                .and_then(|forks| forks.try_iter())
                .map_err(|e| py_error(py, e))?;
            for project in iter {
                let project = project
                    .and_then(|project| project.extract::<String>())
                    .map_err(|e| py_error(py, e))?;
                forks.push(project);
            }
            Ok(forks)
        })
    }

    fn delete_project(&self, project: &str) -> Result<(), Error> {
        Python::attach(|py| {
            self.0
                .bind(py)
                .call_method1("delete_project", (project,))
                .map_err(|e| py_error(py, e))?;
            Ok(())
        })
    }
}

fn main() -> ExitCode {
    let args = Args::parse();

    args.logging.init();

    breezyshim::init();
    let _ = breezyshim::plugin::load_plugins();

    if cleanup_all(py_forge_instances(), args.dry_run) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::types::PyModule;
    use std::cell::RefCell;

    /// The errors a fake forge can raise.
    #[derive(Clone, Copy)]
    enum Fail {
        Server,
        Unsupported,
        Unavailable,
        NoSuchProject,
    }

    impl Fail {
        fn error(self) -> Error {
            match self {
                Fail::Server => Error::Other("server error".to_string()),
                Fail::Unavailable => Error::Unavailable("no credentials".to_string()),
                Fail::Unsupported => Error::Unsupported("no such forge".to_string()),
                Fail::NoSuchProject => Error::NoSuchProject("gone".to_string()),
            }
        }
    }

    #[derive(Default)]
    struct FakeForge {
        user: Option<&'static str>,
        user_error: Option<Fail>,
        sources: Vec<Result<Option<&'static str>, Fail>>,
        proposals_error: Option<Fail>,
        forks: Vec<&'static str>,
        forks_error: Option<Fail>,
        delete_errors: Vec<(&'static str, Fail)>,
        deleted: RefCell<Vec<String>>,
    }

    impl ForgeProjects for FakeForge {
        fn name(&self) -> String {
            "fake".to_string()
        }

        fn current_user(&self) -> Result<Option<String>, Error> {
            match self.user_error {
                Some(fail) => Err(fail.error()),
                None => Ok(self.user.map(|user| user.to_string())),
            }
        }

        fn open_proposal_sources(&self) -> Result<Vec<Result<Option<String>, Error>>, Error> {
            if let Some(fail) = self.proposals_error {
                return Err(fail.error());
            }
            Ok(self
                .sources
                .iter()
                .map(|s| match s {
                    Ok(p) => Ok(p.map(|p| p.to_string())),
                    Err(fail) => Err(fail.error()),
                })
                .collect())
        }

        fn my_forks(&self) -> Result<Vec<String>, Error> {
            if let Some(fail) = self.forks_error {
                return Err(fail.error());
            }
            Ok(self.forks.iter().map(|p| p.to_string()).collect())
        }

        fn delete_project(&self, project: &str) -> Result<(), Error> {
            self.deleted.borrow_mut().push(project.to_string());
            match self.delete_errors.iter().find(|(p, _)| *p == project) {
                Some((_, fail)) => Err(fail.error()),
                None => Ok(()),
            }
        }
    }

    /// A fake forge on which the user "me" is logged in.
    fn fake() -> FakeForge {
        FakeForge {
            user: Some("me"),
            ..Default::default()
        }
    }

    fn cleaned(removed: &[&str], failed: &[&str]) -> Outcome {
        Outcome::Cleaned {
            removed: removed.iter().map(|p| p.to_string()).collect(),
            failed: failed.iter().map(|p| p.to_string()).collect(),
        }
    }

    #[test]
    fn test_deletes_unused_and_keeps_in_use() {
        let forge = FakeForge {
            sources: vec![Ok(Some("me/used")), Ok(Some("other/project"))],
            forks: vec!["me/unused", "me/used", "me/stale"],
            ..fake()
        };
        let outcome = cleanup_forge(&forge, false);
        assert_eq!(outcome, cleaned(&["me/unused", "me/stale"], &[]));
        assert!(!outcome.is_failure());
        assert_eq!(*forge.deleted.borrow(), vec!["me/unused", "me/stale"]);
    }

    #[test]
    fn test_projects_are_compared_exactly() {
        let forge = FakeForge {
            sources: vec![Ok(Some("me/used"))],
            forks: vec!["me/Used", "me/used.git", "me/used"],
            ..fake()
        };
        assert_eq!(
            cleanup_forge(&forge, false),
            cleaned(&["me/Used", "me/used.git"], &[])
        );
    }

    #[test]
    fn test_dry_run_deletes_nothing() {
        let forge = FakeForge {
            sources: vec![Ok(Some("me/used"))],
            forks: vec!["me/unused", "me/used", "me/stale"],
            ..fake()
        };
        let outcome = cleanup_forge(&forge, true);
        assert_eq!(outcome, cleaned(&["me/unused", "me/stale"], &[]));
        assert!(!outcome.is_failure());
        assert!(forge.deleted.borrow().is_empty());
    }

    #[test]
    fn test_proposal_listing_error_deletes_nothing() {
        let forge = FakeForge {
            proposals_error: Some(Fail::Server),
            forks: vec!["me/unused"],
            ..fake()
        };
        let outcome = cleanup_forge(&forge, false);
        assert_eq!(outcome, Outcome::Failed);
        assert!(outcome.is_failure());
        assert!(forge.deleted.borrow().is_empty());
    }

    #[test]
    fn test_source_project_error_deletes_nothing() {
        let forge = FakeForge {
            sources: vec![Ok(Some("me/used")), Err(Fail::Server)],
            forks: vec!["me/unused", "me/used"],
            ..fake()
        };
        assert_eq!(cleanup_forge(&forge, false), Outcome::Failed);
        assert!(forge.deleted.borrow().is_empty());
    }

    #[test]
    fn test_fork_listing_error_deletes_nothing() {
        let forge = FakeForge {
            forks_error: Some(Fail::Server),
            ..fake()
        };
        assert_eq!(cleanup_forge(&forge, false), Outcome::Failed);
        assert!(forge.deleted.borrow().is_empty());
    }

    #[test]
    fn test_missing_source_project_neither_blocks_nor_protects() {
        let forge = FakeForge {
            sources: vec![Ok(None), Err(Fail::NoSuchProject), Ok(Some("me/used"))],
            forks: vec!["me/gone", "me/unused", "me/used"],
            ..fake()
        };
        assert_eq!(
            cleanup_forge(&forge, false),
            cleaned(&["me/gone", "me/unused"], &[])
        );
    }

    #[test]
    fn test_unsupported_is_skipped() {
        let forge = FakeForge {
            proposals_error: Some(Fail::Unsupported),
            forks: vec!["me/unused"],
            ..fake()
        };
        let outcome = cleanup_forge(&forge, false);
        assert_eq!(outcome, Outcome::Skipped);
        assert!(!outcome.is_failure());
        assert!(forge.deleted.borrow().is_empty());

        let forge = FakeForge {
            forks_error: Some(Fail::Unsupported),
            ..fake()
        };
        assert_eq!(cleanup_forge(&forge, false), Outcome::Skipped);
    }

    #[test]
    fn test_delete_unsupported_is_skipped() {
        let forge = FakeForge {
            forks: vec!["me/one", "me/two"],
            delete_errors: vec![("me/one", Fail::Unsupported)],
            ..fake()
        };
        let outcome = cleanup_forge(&forge, false);
        assert_eq!(outcome, Outcome::Skipped);
        assert!(!outcome.is_failure());
        assert_eq!(*forge.deleted.borrow(), vec!["me/one"]);
    }

    #[test]
    fn test_delete_unsupported_keeps_earlier_failures() {
        let forge = FakeForge {
            forks: vec!["me/one", "me/two", "me/three"],
            delete_errors: vec![("me/one", Fail::Server), ("me/two", Fail::Unsupported)],
            ..fake()
        };
        let outcome = cleanup_forge(&forge, false);
        assert_eq!(outcome, cleaned(&[], &["me/one"]));
        assert!(outcome.is_failure());
        assert_eq!(*forge.deleted.borrow(), vec!["me/one", "me/two"]);
    }

    #[test]
    fn test_delete_unavailable_is_a_failure() {
        let forge = FakeForge {
            forks: vec!["me/one", "me/two"],
            delete_errors: vec![("me/one", Fail::Unavailable)],
            ..fake()
        };
        let outcome = cleanup_forge(&forge, false);
        assert_eq!(outcome, cleaned(&["me/two"], &["me/one"]));
        assert!(outcome.is_failure());
    }

    #[test]
    fn test_forks_in_other_namespaces_are_kept() {
        let forge = FakeForge {
            forks: vec![
                "org/unused",
                "me/unused",
                "Me/unused",
                "me-too/unused",
                "org/me/unused",
                "unused",
                "me/sub/unused",
            ],
            ..fake()
        };
        assert_eq!(
            cleanup_forge(&forge, false),
            cleaned(&["me/unused", "me/sub/unused"], &[])
        );
        assert_eq!(*forge.deleted.borrow(), vec!["me/unused", "me/sub/unused"]);
    }

    #[test]
    fn test_not_logged_in_is_skipped() {
        let forge = FakeForge {
            forks: vec!["me/unused"],
            ..Default::default()
        };
        let outcome = cleanup_forge(&forge, false);
        assert_eq!(outcome, Outcome::Skipped);
        assert!(!outcome.is_failure());
        assert!(forge.deleted.borrow().is_empty());
    }

    #[test]
    fn test_current_user_errors() {
        let forge = FakeForge {
            user_error: Some(Fail::Unavailable),
            forks: vec!["me/unused"],
            ..fake()
        };
        assert_eq!(cleanup_forge(&forge, false), Outcome::Skipped);
        let forge = FakeForge {
            user_error: Some(Fail::Server),
            forks: vec!["me/unused"],
            ..fake()
        };
        assert_eq!(cleanup_forge(&forge, false), Outcome::Failed);
        assert!(forge.deleted.borrow().is_empty());
    }

    #[test]
    fn test_no_attributable_proposal_deletes_nothing() {
        let forge = FakeForge {
            sources: vec![Ok(None), Err(Fail::NoSuchProject)],
            forks: vec!["me/unused"],
            ..fake()
        };
        let outcome = cleanup_forge(&forge, false);
        assert_eq!(outcome, Outcome::Failed);
        assert!(outcome.is_failure());
        assert!(forge.deleted.borrow().is_empty());

        let forge = FakeForge {
            sources: vec![Ok(None), Err(Fail::NoSuchProject)],
            forks: vec!["org/unused"],
            ..fake()
        };
        let outcome = cleanup_forge(&forge, false);
        assert_eq!(outcome, cleaned(&[], &[]));
        assert!(!outcome.is_failure());
    }

    #[test]
    fn test_cleanup_all() {
        assert!(!cleanup_all(Vec::<Result<FakeForge, _>>::new(), false));
        let unused = || FakeForge {
            forks: vec!["me/unused"],
            ..fake()
        };
        assert!(!cleanup_all(
            vec![
                Err(("missing".to_string(), Fail::Unavailable.error())),
                Err(("other".to_string(), Fail::Unsupported.error())),
                Ok(unused()),
                Ok(FakeForge::default()),
            ],
            false
        ));
        assert!(cleanup_all(
            vec![
                Err(("broken".to_string(), Fail::Server.error())),
                Ok(unused())
            ],
            false
        ));
        assert!(cleanup_all(
            vec![Ok(FakeForge {
                proposals_error: Some(Fail::Server),
                ..fake()
            })],
            false
        ));
    }

    #[test]
    fn test_failing_delete_does_not_stop_the_others() {
        let forge = FakeForge {
            forks: vec!["me/one", "me/two", "me/three"],
            delete_errors: vec![("me/two", Fail::Server)],
            ..fake()
        };
        let outcome = cleanup_forge(&forge, false);
        assert_eq!(outcome, cleaned(&["me/one", "me/three"], &["me/two"]));
        assert!(outcome.is_failure());
        assert_eq!(
            *forge.deleted.borrow(),
            vec!["me/one", "me/two", "me/three"]
        );
    }

    #[test]
    fn test_no_forks() {
        let forge = FakeForge {
            sources: vec![Ok(Some("me/used"))],
            ..fake()
        };
        let outcome = cleanup_forge(&forge, false);
        assert_eq!(outcome, cleaned(&[], &[]));
        assert!(!outcome.is_failure());
    }

    #[test]
    fn test_duplicate_forks_are_deleted_once() {
        let forge = FakeForge {
            forks: vec!["me/one", "me/one"],
            ..fake()
        };
        assert_eq!(cleanup_forge(&forge, false), cleaned(&["me/one"], &[]));
    }

    #[test]
    fn test_args() {
        let args = Args::try_parse_from(["janitor-cleanup-repositories"]).unwrap();
        assert!(!args.dry_run);
        assert!(!args.logging.debug);
        let args =
            Args::try_parse_from(["janitor-cleanup-repositories", "--dry-run", "--debug"]).unwrap();
        assert!(args.dry_run);
        assert!(args.logging.debug);
        assert!(Args::try_parse_from(["janitor-cleanup-repositories", "extra"]).is_err());
    }

    const FAKES: &std::ffi::CStr = cr#"
from breezy.errors import DependencyNotPresent
from breezy.forge import ForgeLoginRequired, NoSuchProject, UnsupportedForge


class FakeProposal:
    def __init__(self, source, closed=False, merged=False):
        self.source = source
        self.closed = closed
        self.merged = merged

    def is_closed(self):
        return self.closed

    def is_merged(self):
        return self.merged

    def get_source_project(self):
        if isinstance(self.source, Exception):
            raise self.source
        return self.source


class FakeForge:
    def __init__(self):
        self.deleted = []
        self.proposals = [
            FakeProposal("me/used"),
            FakeProposal("me/closed", closed=True),
            FakeProposal("me/merged", merged=True),
            FakeProposal(None),
            FakeProposal(NoSuchProject(42)),
            FakeProposal(NoSuchProject(None)),
        ]

    def __repr__(self):
        return "<FakeForge>"

    def get_current_user(self):
        return "me"

    def iter_my_proposals(self, status="open", author=None):
        yield from self.proposals

    def iter_my_forks(self, owner=None):
        yield from ["me/used", "me/closed", "org/unused", "me/merged", "me/missing"]

    def delete_project(self, project):
        if project == "me/missing":
            raise NoSuchProject(project)
        self.deleted.append(project)


class BrokenSourceForge(FakeForge):
    def __init__(self):
        super().__init__()
        self.proposals.append(FakeProposal(RuntimeError("server error")))


class BrokenListingForge(FakeForge):
    def iter_my_proposals(self, status="open", author=None):
        yield FakeProposal("me/used")
        raise RuntimeError("server error")


class UnsupportedFakeForge(FakeForge):
    def iter_my_proposals(self, status="open", author=None):
        raise UnsupportedForge("https://example.com/")


class NoForksForge(FakeForge):
    def iter_my_forks(self, owner=None):
        raise NotImplementedError(self.iter_my_forks)


class AnonymousForge(FakeForge):
    def get_current_user(self):
        return None


class MissingDependencyForge(FakeForge):
    def get_current_user(self):
        raise DependencyNotPresent("x", ImportError())


class LoginRequiredForge(FakeForge):
    def iter_my_proposals(self, status="open", author=None):
        raise ForgeLoginRequired("https://example.com/")


class RejectedUserForge(FakeForge):
    def get_current_user(self):
        raise ForgeLoginRequired("https://example.com/")


class DeleteLoginRequiredForge(FakeForge):
    def delete_project(self, project):
        raise ForgeLoginRequired("https://example.com/")


class NonStringUserForge(FakeForge):
    def get_current_user(self):
        return 42


class NonStringSourceForge(FakeForge):
    def __init__(self):
        super().__init__()
        self.proposals.append(FakeProposal(42))


class NonStringForkForge(FakeForge):
    def iter_my_forks(self, owner=None):
        yield "me/closed"
        yield 42
"#;

    fn py_fake(name: &str) -> PyForge {
        Python::attach(|py| {
            let m = PyModule::from_code(
                py,
                FAKES,
                c"janitor_cleanup_fakes.py",
                c"janitor_cleanup_fakes",
            )
            .unwrap();
            PyForge(m.getattr(name).unwrap().call0().unwrap().unbind())
        })
    }

    fn py_deleted(forge: &PyForge) -> Vec<String> {
        Python::attach(|py| forge.0.getattr(py, "deleted").unwrap().extract(py).unwrap())
    }

    #[test]
    fn test_py_forge() {
        let forge = py_fake("FakeForge");
        assert_eq!(forge.name(), "<FakeForge>");
        let sources = forge.open_proposal_sources().unwrap();
        assert_eq!(sources.len(), 4);
        assert!(matches!(&sources[0], Ok(Some(p)) if p == "me/used"));
        assert!(matches!(&sources[1], Ok(None)));
        assert!(matches!(&sources[2], Err(Error::NoSuchProject(p)) if p == "42"));
        assert!(matches!(&sources[3], Err(Error::NoSuchProject(p)) if p == "None"));

        let outcome = cleanup_forge(&forge, true);
        assert_eq!(
            outcome,
            cleaned(&["me/closed", "me/merged", "me/missing"], &[])
        );
        assert!(py_deleted(&forge).is_empty());

        let outcome = cleanup_forge(&forge, false);
        assert_eq!(
            outcome,
            cleaned(&["me/closed", "me/merged"], &["me/missing"])
        );
        assert_eq!(py_deleted(&forge), vec!["me/closed", "me/merged"]);
    }

    #[test]
    fn test_py_forge_errors_delete_nothing() {
        for name in [
            "BrokenSourceForge",
            "BrokenListingForge",
            "LoginRequiredForge",
            "RejectedUserForge",
            "NonStringUserForge",
            "NonStringSourceForge",
            "NonStringForkForge",
        ] {
            let forge = py_fake(name);
            assert_eq!(cleanup_forge(&forge, false), Outcome::Failed, "{}", name);
            assert!(py_deleted(&forge).is_empty());
        }
    }

    #[test]
    fn test_py_forge_unsupported_is_skipped() {
        for name in [
            "UnsupportedFakeForge",
            "NoForksForge",
            "AnonymousForge",
            "MissingDependencyForge",
        ] {
            let forge = py_fake(name);
            assert_eq!(cleanup_forge(&forge, false), Outcome::Skipped, "{}", name);
            assert!(py_deleted(&forge).is_empty());
        }
    }

    #[test]
    fn test_py_forge_login_required_on_delete_is_a_failure() {
        let forge = py_fake("DeleteLoginRequiredForge");
        assert_eq!(forge.current_user(), Ok(Some("me".to_string())));
        let outcome = cleanup_forge(&forge, false);
        assert_eq!(
            outcome,
            cleaned(&[], &["me/closed", "me/merged", "me/missing"])
        );
        assert!(outcome.is_failure());
    }

    #[test]
    fn test_py_error() {
        Python::attach(|py| {
            let m = PyModule::from_code(py, FAKES, c"janitor_cleanup_fakes.py", c"fakes").unwrap();
            let raise = |name: &str| {
                let forge = m.getattr(name).unwrap().call0().unwrap();
                py_error(py, forge.call_method0("get_current_user").err().unwrap())
            };
            assert!(matches!(
                raise("MissingDependencyForge"),
                Error::Unavailable(_)
            ));
            assert!(matches!(raise("RejectedUserForge"), Error::Other(_)));
        });
    }

    #[test]
    fn test_py_instances_of() {
        Python::attach(|py| {
            let registry = PyModule::from_code(
                py,
                cr#"
class Listed:
    @classmethod
    def iter_instances(cls):
        yield cls()
        yield cls()


class Broken:
    @classmethod
    def iter_instances(cls):
        yield cls()
        raise RuntimeError("no credentials store")


class Missing:
    @classmethod
    def iter_instances(cls):
        from breezy.errors import DependencyNotPresent
        raise DependencyNotPresent("x", ImportError())


forges = {"listed": Listed, "broken": Broken, "missing": Missing}
empty = {}
"#,
                c"janitor_cleanup_registry.py",
                c"janitor_cleanup_registry",
            )
            .unwrap();
            let empty = registry.getattr("empty").unwrap();
            assert!(py_registered_instances(&empty).is_empty());
            let registry = registry.getattr("forges").unwrap();
            let instances = py_registered_instances(&registry);
            assert_eq!(instances.len(), 4);
            assert!(instances[0].is_ok() && instances[1].is_ok());
            assert!(matches!(&instances[2], Err((kind, Error::Other(_))) if kind == "broken"));
            assert!(
                matches!(&instances[3], Err((kind, Error::Unavailable(_))) if kind == "missing")
            );
            assert!(cleanup_all(instances, true));
            assert_eq!(py_instances_of(&registry, "listed").unwrap().len(), 2);
            let err = py_instances_of(&registry, "broken").err().unwrap();
            assert_eq!(
                py_error(py, err),
                Error::Other("RuntimeError: no credentials store".to_string())
            );
        });
    }
}
