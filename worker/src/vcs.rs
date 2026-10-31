use breezyshim::branch::Branch;
use breezyshim::error::Error as BrzError;
use breezyshim::repository::Repository;
use breezyshim::transport::Transport;
use breezyshim::RevisionId;
use janitor::vcs::VcsType;
use janitor::{Campaign, RunId};
use std::collections::HashMap;
use url::Url;

/// Push a branch to a new location.
pub fn push_branch(
    source_branch: &dyn breezyshim::branch::PyBranch,
    url: &Url,
    vcs_type: Option<VcsType>,
    overwrite: bool,
    stop_revision: Option<RevisionId>,
    tag_selector: Option<Box<dyn Fn(String) -> bool>>,
    possible_transports: &mut Option<Vec<Transport>>,
) -> Result<(), BrzError> {
    let (url, params) = breezyshim::urlutils::split_segment_parameters(url);
    let branch_name = params.get("branch").map(|s| {
        percent_encoding::percent_decode(s.as_bytes())
            .decode_utf8()
            .unwrap()
            .to_string()
    });
    let vcs_type = vcs_type.map_or_else(
        || source_branch.controldir().cloning_metadir(),
        |t| {
            breezyshim::controldir::FORMAT_REGISTRY
                .make_controldir(t.to_string().as_str())
                .unwrap()
        },
    );
    // A boxed selector is used up by one push; share it so that each try gets its own.
    let tag_selector = tag_selector.map(std::rc::Rc::from);
    retry_push(push_retryable, || {
        let target = match breezyshim::controldir::open(&url, possible_transports.as_mut()) {
            Ok(cd) => cd,
            Err(BrzError::NotBranchError(..)) => {
                breezyshim::controldir::create(&url, &vcs_type, possible_transports.as_mut())?
            }
            Err(e) => {
                return Err(e);
            }
        };

        let tag_selector = tag_selector
            .clone()
            .map(|f: std::rc::Rc<dyn Fn(String) -> bool>| {
                Box::new(move |tag| f(tag)) as Box<dyn Fn(String) -> bool>
            });
        target.push_branch(
            source_branch,
            branch_name.as_deref(),
            stop_revision.as_ref(),
            Some(overwrite),
            tag_selector,
        )?;

        Ok(())
    })
}

/// Tries for one push, as in the Python worker (`max_tries=10`).
const PUSH_MAX_TRIES: u32 = 10;

/// Errors after which `import_branches_*` is tried again, as in the Python worker.
fn import_retryable(e: &BrzError) -> bool {
    matches!(
        e,
        BrzError::InvalidHttpResponse(..)
            | BrzError::IncompleteRead(..)
            | BrzError::ConnectionError(..)
    )
}

/// Errors after which `push_branch` is tried again: also any unexpected HTTP status.
fn push_retryable(e: &BrzError) -> bool {
    matches!(e, BrzError::UnexpectedHttpStatus { .. }) || import_retryable(e)
}

fn push_branch_retries() -> &'static prometheus::IntCounter {
    static M: std::sync::OnceLock<prometheus::IntCounter> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        prometheus::register_int_counter!("push_branch_retries", "Number of branch push retries.")
            .expect("register push_branch_retries")
    })
}

/// Run a push, and run it again with a growing wait while it fails with a `retryable` error.
fn retry_push<T>(
    retryable: fn(&BrzError) -> bool,
    op: impl FnMut() -> Result<T, BrzError>,
) -> Result<T, BrzError> {
    retry_push_with(
        retryable,
        op,
        push_waits(),
        std::thread::sleep,
        push_branch_retries(),
    )
}

/// Waits of 0.5 s, 1 s, 2 s, ... each with jitter.
fn push_waits() -> backoff::ExponentialBackoff {
    backoff::ExponentialBackoff {
        multiplier: 2.0,
        max_interval: std::time::Duration::from_secs(128),
        max_elapsed_time: None,
        ..Default::default()
    }
}

fn retry_push_with<T>(
    retryable: fn(&BrzError) -> bool,
    mut op: impl FnMut() -> Result<T, BrzError>,
    mut waits: backoff::ExponentialBackoff,
    mut sleep: impl FnMut(std::time::Duration),
    retries: &prometheus::IntCounter,
) -> Result<T, BrzError> {
    use backoff::backoff::Backoff;
    let mut tries = 1;
    loop {
        let e = match op() {
            Ok(ret) => return Ok(ret),
            Err(e) => e,
        };
        if !retryable(&e) || tries >= PUSH_MAX_TRIES {
            return Err(e);
        }
        let wait = waits.next_backoff().unwrap_or(waits.max_interval);
        tries += 1;
        retries.inc();
        // The error text can hold the URL with the password, so it is not logged.
        log::warn!(
            "Push failed; try {} of {} in {:?}",
            tries,
            PUSH_MAX_TRIES,
            wait
        );
        sleep(wait);
    }
}

/// Import branches into a new location.
///
/// This function will create a new branch for each branch in the branches
/// vector, and push the revisions from the local branch to the new branch.
/// The tags vector is used to set tags on the new branches.
///
/// The directory structure of the new branches will be:
///
/// <url>/campaign>
///
/// The tags will be set on the new branches as follows:
/// - The tag `log_id` will be set to the revision in the branches vector.
/// - The tag `log_id/<tag_name>` will be set to the revision in the tags vector
///   for each tag in the tags vector.
/// - The tag `<tag_name>` will be set to the revision in the tags vector for
///   each tag in the tags vector if `update_current` is true.
///
/// # Arguments
/// * `repo_url` - The URL of the repository to import the branches into.
/// * `local_branch` - The local branch to import the branches from.
/// * `campaign` - The campaign
/// * `log_id` - The log_id
/// * `branches` - A vector of tuples containing the branch name, the directory
///    name, the revision to push, and the revision to push to.
///
/// # Returns
/// * `Ok(())` if the branches were successfully imported.
fn import_branches_bzr(
    repo_url: &Url,
    local_branch: &dyn breezyshim::branch::PyBranch,
    campaign: &Campaign,
    log_id: &RunId,
    branches: &Vec<(String, String, Option<RevisionId>, Option<RevisionId>)>,
    tags: Vec<(String, Option<RevisionId>)>,
    update_current: bool,
) -> Result<(), BrzError> {
    let format = breezyshim::controldir::FORMAT_REGISTRY
        .make_controldir("bzr")
        .ok_or(BrzError::UnknownFormat("bzr".to_string()))?;
    for (f, _n, _br, r) in branches.iter() {
        let rootcd = match breezyshim::controldir::open(repo_url, None) {
            Ok(cd) => cd,
            Err(BrzError::NotBranchError(..)) => {
                breezyshim::controldir::create(repo_url, &format, None)?
            }
            Err(e) => {
                return Err(e);
            }
        };
        match rootcd.find_repository() {
            Err(BrzError::NoRepositoryPresent) => {
                rootcd.create_repository(Some(true))?;
            }
            Ok(_) => {}
            Err(e) => {
                return Err(e);
            }
        }
        let transport = rootcd.user_transport().clone(campaign)?;
        let name = if f != "main" {
            f.to_string()
        } else {
            "".to_string()
        };
        if !transport.has(".")? {
            match transport.ensure_base() {
                Ok(_) => {}
                Err(BrzError::NoSuchFile(..)) => {
                    transport.create_prefix()?;
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
        let branchcd = match format.initialize_on_transport(&transport) {
            Ok(cd) => cd,
            Err(BrzError::AlreadyControlDir(..)) => {
                breezyshim::controldir::open_from_transport(&transport, None)?
            }
            Err(e) => {
                return Err(e);
            }
        };

        assert!(branchcd.find_repository().is_ok());

        let target_branch = match branchcd.open_branch(Some(&name)) {
            Ok(b) => b,
            Err(BrzError::NotBranchError(..)) => branchcd.create_branch(Some(&name))?,
            Err(e) => {
                return Err(e);
            }
        };
        if update_current {
            local_branch.push(&*target_branch, true, r.as_ref(), None)?;
        } else {
            target_branch
                .repository()
                .fetch(&local_branch.repository(), r.as_ref())?;
        }

        target_branch.tags()?.set_tag(log_id, &r.clone().unwrap())?;

        let graph = target_branch.repository().get_graph();
        for (name, revision) in tags.iter() {
            if let Some(revision) = revision {
                // Only set tags on those branches where the revisions exist
                if graph
                    .is_ancestor(revision, &target_branch.last_revision())
                    .unwrap_or(false)
                {
                    target_branch
                        .tags()?
                        .set_tag(&format!("{}/{}", log_id, name), revision)?;
                    if update_current {
                        target_branch.tags()?.set_tag(name, revision)?;
                    }
                }
            } else if update_current {
                match target_branch.tags()?.delete_tag(name) {
                    Ok(_) => (),
                    Err(BrzError::NoSuchTag(..)) => (),
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
        }
    }

    Ok(())
}

pub trait Vcs {
    fn import_branches(
        &self,
        repo_url: &Url,
        local_branch: &dyn breezyshim::branch::PyBranch,
        campaign: &Campaign,
        log_id: &RunId,
        branches: &Vec<(String, String, Option<RevisionId>, Option<RevisionId>)>,
        tags: Vec<(String, Option<RevisionId>)>,
        update_current: bool,
    ) -> Result<(), BrzError>;
}

pub struct BzrVcs;

impl Vcs for BzrVcs {
    fn import_branches(
        &self,
        repo_url: &Url,
        local_branch: &dyn breezyshim::branch::PyBranch,
        campaign: &Campaign,
        log_id: &RunId,
        branches: &Vec<(String, String, Option<RevisionId>, Option<RevisionId>)>,
        tags: Vec<(String, Option<RevisionId>)>,
        update_current: bool,
    ) -> Result<(), BrzError> {
        retry_push(import_retryable, || {
            import_branches_bzr(
                repo_url,
                local_branch,
                campaign,
                log_id,
                branches,
                tags.clone(),
                update_current,
            )
        })
    }
}

fn import_branches_git(
    repo_url: &Url,
    local_branch: &dyn breezyshim::branch::PyBranch,
    campaign: &Campaign,
    log_id: &RunId,
    branches: &Vec<(String, String, Option<RevisionId>, Option<RevisionId>)>,
    tags: Vec<(String, Option<RevisionId>)>,
    update_current: bool,
) -> Result<(), BrzError> {
    let vcs_result_controldir = match breezyshim::controldir::open(repo_url, None) {
        Err(BrzError::NotBranchError(..)) => {
            let transport = breezyshim::transport::get_transport(repo_url, None).unwrap();
            if !transport.has(".")? {
                match transport.ensure_base() {
                    Err(BrzError::NoSuchFile(..)) => {
                        transport.create_prefix()?;
                    }
                    Ok(_) => (),
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
            // The server is expected to have repositories ready for us, unless we're working
            // locally.
            let format = breezyshim::controldir::FORMAT_REGISTRY
                .make_controldir("git-bare")
                .unwrap();
            format.initialize(repo_url)?
        }
        Ok(cd) => cd,
        Err(e) => {
            return Err(e);
        }
    };

    let repo = vcs_result_controldir.open_repository().unwrap();
    let source_repo = local_branch.repository();

    // Clone for the sake of the closure
    let log_id_ = log_id.clone();
    let campaign_ = campaign.clone();
    let source_repo_for_closure = source_repo.clone();
    let branches = branches.clone();

    let get_changed_refs = move |_refs: &HashMap<Vec<u8>, (Vec<u8>, Option<RevisionId>)>| -> HashMap<Vec<u8>, (Vec<u8>, Option<RevisionId>)> {
        let mut changed_refs = HashMap::new();
        for (f, _n, _br, r) in branches.iter() {
            let tagname = format!("refs/tags/run/{}/{}", log_id_, f);
            changed_refs.insert(
                tagname.as_bytes().to_vec(),
                if let Some(r) = r {
                    (source_repo_for_closure.lookup_bzr_revision_id(r).unwrap().0, Some(r.clone()))
                } else {
                    (breezyshim::git::ZERO_SHA.to_vec(), r.clone())
                },
            );
            if update_current {
                let branchname = format!("refs/heads/{}/{}", campaign_, f);
                // TODO(jelmer): Ideally this would be a symref:
                changed_refs.insert(branchname.as_bytes().to_vec(), changed_refs.get(tagname.as_bytes()).unwrap().clone());
            }
        }
        for (n, r) in tags.iter() {
            let tagname = format!("refs/tags/{}/{}", log_id_, n);
            changed_refs.insert(
                tagname.as_bytes().to_vec(),
                (
                    source_repo_for_closure.lookup_bzr_revision_id(r.as_ref().unwrap()).unwrap().0,
                    r.clone(),
                ),
            );
            if update_current {
                let tagname = format!("refs/tags/{}", n);
                changed_refs.insert(
                    tagname.as_bytes().to_vec(),
                    (
                        source_repo_for_closure.lookup_bzr_revision_id(r.as_ref().unwrap()).unwrap().0,
                        r.clone(),
                    ),
                );
            }
        }
        changed_refs
    };

    let inter = breezyshim::interrepository::get(&source_repo, &repo).unwrap();
    inter.fetch_refs(
        std::sync::Mutex::new(Box::new(get_changed_refs)),
        false,
        true,
    )?;
    Ok(())
}

pub struct GitVcs;

impl Vcs for GitVcs {
    fn import_branches(
        &self,
        repo_url: &Url,
        local_branch: &dyn breezyshim::branch::PyBranch,
        campaign: &Campaign,
        log_id: &RunId,
        branches: &Vec<(String, String, Option<RevisionId>, Option<RevisionId>)>,
        tags: Vec<(String, Option<RevisionId>)>,
        update_current: bool,
    ) -> Result<(), BrzError> {
        retry_push(import_retryable, || {
            import_branches_git(
                repo_url,
                local_branch,
                campaign,
                log_id,
                branches,
                tags.clone(),
                update_current,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use breezyshim::tree::WorkingTree;

    #[test]
    fn test_push_branch() {
        let td = tempfile::tempdir().unwrap();
        let source_tree = breezyshim::controldir::create_standalone_workingtree(
            &td.path().join("source"),
            &breezyshim::controldir::ControlDirFormat::default(),
        )
        .unwrap();
        let target_url = Url::from_directory_path(td.path().join("target")).unwrap();
        let revid1 = source_tree
            .build_commit()
            .message("Initial commit")
            .commit()
            .unwrap();
        let target = breezyshim::controldir::create(
            &target_url,
            &breezyshim::controldir::FORMAT_REGISTRY
                .make_controldir("bzr")
                .unwrap(),
            None,
        )
        .unwrap();
        target.create_repository(None).unwrap();
        super::push_branch(
            &source_tree.branch(),
            &url::Url::parse(&format!("{},branch=foo", target_url)).unwrap(),
            None,
            false,
            None,
            None,
            &mut None,
        )
        .unwrap();
        assert_eq!(
            target.open_branch(Some("foo")).unwrap().last_revision(),
            revid1
        );
    }

    #[test]
    fn test_import_branches_bzr() {
        let td = tempfile::tempdir().unwrap();
        let source_tree = breezyshim::controldir::create_standalone_workingtree(
            &td.path().join("source"),
            &breezyshim::controldir::ControlDirFormat::default(),
        )
        .unwrap();
        let target_path = td.path().join("target");
        let target_url = Url::from_directory_path(&target_path).unwrap();
        let revid1 = source_tree
            .build_commit()
            .message("Initial commit")
            .commit()
            .unwrap();
        let target = breezyshim::controldir::create(
            &target_url,
            &breezyshim::controldir::FORMAT_REGISTRY
                .make_controldir("bzr")
                .unwrap(),
            None,
        )
        .unwrap();
        target.create_repository(Some(true)).unwrap();
        BzrVcs
            .import_branches(
                &target_url,
                &source_tree.branch(),
                &Campaign::new("campaign").unwrap(),
                &RunId::new("log_id").unwrap(),
                &vec![(
                    "main".to_string(),
                    "foo".to_string(),
                    None,
                    Some(revid1.clone()),
                )],
                vec![("tag".to_string(), Some(revid1.clone()))],
                false,
            )
            .unwrap();
        let target_branch_foo = breezyshim::branch::open(
            &Url::from_directory_path(target_path.join("campaign")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            &target_branch_foo.last_revision(),
            &breezyshim::RevisionId::null()
        );
        assert_eq!(
            &target_branch_foo.tags().unwrap().get_tag_dict().unwrap(),
            &maplit::hashmap! {
                "log_id".to_string() => revid1.clone(),
            }
        );
    }

    #[test]
    fn test_import_branches_git() {
        let td = tempfile::tempdir().unwrap();
        let source_tree = breezyshim::controldir::create_standalone_workingtree(
            &td.path().join("source"),
            &breezyshim::controldir::FORMAT_REGISTRY
                .make_controldir("git")
                .unwrap(),
        )
        .unwrap();
        let target_path = td.path().join("target");
        let target_url = Url::from_directory_path(&target_path).unwrap();
        let revid1 = source_tree
            .build_commit()
            .message("Initial commit")
            .commit()
            .unwrap();
        let target = breezyshim::controldir::create(
            &target_url,
            &breezyshim::controldir::FORMAT_REGISTRY
                .make_controldir("git-bare")
                .unwrap(),
            None,
        )
        .unwrap();
        GitVcs
            .import_branches(
                &target_url,
                &source_tree.branch(),
                &Campaign::new("campaign").unwrap(),
                &RunId::new("log_id").unwrap(),
                &vec![(
                    "main".to_string(),
                    "foo".to_string(),
                    None,
                    Some(revid1.clone()),
                )],
                vec![("tag".to_string(), Some(revid1.clone()))],
                true,
            )
            .unwrap();
        let target_branch_foo = target.open_branch(Some("campaign/main")).unwrap();
        assert_eq!(&target_branch_foo.last_revision(), &revid1);
        assert_eq!(
            &target_branch_foo.tags().unwrap().get_tag_dict().unwrap(),
            &maplit::hashmap! {
                "run/log_id/main".to_string() => revid1.clone(),
                "log_id/tag".to_string() => revid1.clone(),
                "tag".to_string() => revid1.clone(),
            }
        );
        std::mem::drop(td);
    }

    fn retry(
        retryable: fn(&BrzError) -> bool,
        errors: Vec<BrzError>,
    ) -> (Result<(), BrzError>, Vec<std::time::Duration>, u64) {
        let mut errors = errors.into_iter();
        let mut waits = vec![];
        let retries = prometheus::IntCounter::new("retries", "retries").unwrap();
        let waits_without_jitter = backoff::ExponentialBackoff {
            randomization_factor: 0.0,
            ..push_waits()
        };
        let ret = retry_push_with(
            retryable,
            || errors.next().map_or(Ok(()), Err),
            waits_without_jitter,
            |wait| waits.push(wait),
            &retries,
        );
        (ret, waits, retries.get())
    }

    fn connection_error() -> BrzError {
        BrzError::ConnectionError("while sending POST /codebase/git-receive-pack:".to_string())
    }

    #[test]
    fn test_retry_push_repeats_a_retryable_error() {
        let (ret, waits, retries) = retry(
            import_retryable,
            (0..3).map(|_| connection_error()).collect(),
        );
        assert!(ret.is_ok());
        assert_eq!(
            waits,
            vec![
                std::time::Duration::from_millis(500),
                std::time::Duration::from_secs(1),
                std::time::Duration::from_secs(2),
            ]
        );
        assert_eq!(retries, 3);
    }

    #[test]
    fn test_retry_push_gives_up() {
        let (ret, waits, retries) = retry(
            push_retryable,
            (0..20).map(|_| connection_error()).collect(),
        );
        assert!(matches!(ret, Err(BrzError::ConnectionError(_))));
        assert_eq!((waits.len(), retries), (9, 9));

        let (ret, waits, retries) = retry(push_retryable, vec![BrzError::DivergedBranches]);
        assert!(matches!(ret, Err(BrzError::DivergedBranches)));
        assert_eq!((waits.len(), retries), (0, 0));
    }

    #[test]
    fn test_retryable_errors() {
        let status = BrzError::UnexpectedHttpStatus {
            url: Url::parse("http://example.com/codebase").unwrap(),
            code: 403,
            extra: None,
            headers: HashMap::new(),
        };
        let errors = [
            connection_error(),
            BrzError::IncompleteRead(vec![], Some(10)),
            BrzError::InvalidHttpResponse(
                "http://example.com/codebase".to_string(),
                "Bad status line received".to_string(),
                None,
                HashMap::new(),
            ),
            status,
            BrzError::TransportError("No space left on device".to_string()),
            BrzError::DivergedBranches,
        ];
        let kinds: Vec<_> = errors
            .iter()
            .map(|e| (push_retryable(e), import_retryable(e)))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (true, true),
                (true, true),
                (true, true),
                (true, false),
                (false, false),
                (false, false),
            ]
        );
    }
}
