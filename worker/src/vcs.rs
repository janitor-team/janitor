use breezyshim::branch::Branch;
use breezyshim::error::Error as BrzError;
use breezyshim::repository::Repository;
use breezyshim::transport::Transport;
use breezyshim::RevisionId;
use janitor::vcs::VcsType;
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
    retry_push(|| {
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

/// What Breezy says when the connection to the store is lost.
const CONNECTION_LOST_MESSAGES: &[&str] = &[
    "while sending ",
    "Connection lost while sending request",
    "Connection closed early",
];

/// Why a failed push is worth another try: the connection was lost or the store was briefly away.
fn connection_lost(e: &BrzError) -> Option<&'static str> {
    match e {
        BrzError::InvalidHttpResponse(..) => Some("invalid HTTP response"),
        BrzError::IncompleteRead(..) => Some("incomplete read"),
        BrzError::ConnectionError(..) => Some("connection error"),
        BrzError::UnexpectedHttpStatus {
            code: 502..=504, ..
        } => Some("gateway error"),
        // Breezy's ConnectionError and ConnectionReset can arrive as a plain TransportError.
        BrzError::TransportError(msg)
            if CONNECTION_LOST_MESSAGES.iter().any(|m| msg.contains(m)) =>
        {
            Some("connection lost")
        }
        _ => None,
    }
}

/// Run a push, and run it again with a growing wait while the connection keeps being lost.
fn retry_push<T>(op: impl FnMut() -> Result<T, BrzError>) -> Result<T, BrzError> {
    retry_push_with(op, std::thread::sleep)
}

fn retry_push_with<T>(
    mut op: impl FnMut() -> Result<T, BrzError>,
    mut sleep: impl FnMut(std::time::Duration),
) -> Result<T, BrzError> {
    use backoff::backoff::Backoff;
    // Waits of 0.5 s, 1 s, 2 s, ... each with jitter.
    let mut waits = backoff::ExponentialBackoff {
        multiplier: 2.0,
        max_interval: std::time::Duration::from_secs(128),
        max_elapsed_time: None,
        ..Default::default()
    };
    let mut tries = 1;
    loop {
        let e = match op() {
            Ok(ret) => return Ok(ret),
            Err(e) => e,
        };
        let Some(why) = connection_lost(&e).filter(|_| tries < PUSH_MAX_TRIES) else {
            return Err(e);
        };
        let wait = waits.next_backoff().unwrap_or(waits.max_interval);
        tries += 1;
        // The error text can hold the URL with the password, so only its kind is logged.
        log::warn!(
            "Push failed ({}); try {} of {} in {:?}",
            why,
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
    campaign: &str,
    log_id: &str,
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
        campaign: &str,
        log_id: &str,
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
        campaign: &str,
        log_id: &str,
        branches: &Vec<(String, String, Option<RevisionId>, Option<RevisionId>)>,
        tags: Vec<(String, Option<RevisionId>)>,
        update_current: bool,
    ) -> Result<(), BrzError> {
        retry_push(|| {
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
    campaign: &str,
    log_id: &str,
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
    let log_id_ = log_id.to_string();
    let campaign_ = campaign.to_string();
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
        campaign: &str,
        log_id: &str,
        branches: &Vec<(String, String, Option<RevisionId>, Option<RevisionId>)>,
        tags: Vec<(String, Option<RevisionId>)>,
        update_current: bool,
    ) -> Result<(), BrzError> {
        retry_push(|| {
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
                "campaign",
                "log_id",
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
                "campaign",
                "log_id",
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

    fn lost_connection() -> BrzError {
        BrzError::TransportError("while sending POST /codebase/git-receive-pack:".to_string())
    }

    fn http_status(code: u16) -> BrzError {
        BrzError::UnexpectedHttpStatus {
            url: Url::parse("http://example.com/codebase").unwrap(),
            code,
            extra: None,
            headers: HashMap::new(),
        }
    }

    #[test]
    fn test_retry_push_repeats_a_lost_connection() {
        let mut calls = 0;
        let mut waits = vec![];
        let ret = retry_push_with(
            || {
                calls += 1;
                if calls < 4 {
                    Err(lost_connection())
                } else {
                    Ok(calls)
                }
            },
            |wait| waits.push(wait),
        );
        assert_eq!(ret.unwrap(), 4);
        assert_eq!(waits.len(), 3);
        assert!(waits[2] > waits[0], "waits do not grow: {:?}", waits);
    }

    #[test]
    fn test_retry_push_gives_up_after_max_tries() {
        let mut calls = 0;
        let mut waits = 0;
        let ret: Result<(), BrzError> = retry_push_with(
            || {
                calls += 1;
                Err(lost_connection())
            },
            |_| waits += 1,
        );
        assert!(matches!(ret, Err(BrzError::TransportError(..))));
        assert_eq!(calls, PUSH_MAX_TRIES);
        assert_eq!(waits, PUSH_MAX_TRIES - 1);
    }

    #[test]
    fn test_retry_push_does_not_repeat_other_errors() {
        for e in [
            BrzError::TransportError("No space left on device".to_string()),
            BrzError::RemoteGitError("pre-receive hook declined".to_string()),
            BrzError::DivergedBranches,
            http_status(403),
        ] {
            let mut calls = 0;
            let mut e = Some(e);
            let ret: Result<(), BrzError> = retry_push_with(
                || {
                    calls += 1;
                    Err(e.take().unwrap())
                },
                |_| panic!("waited after an error that is not a lost connection"),
            );
            assert!(ret.is_err());
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn test_connection_lost() {
        for e in [
            lost_connection(),
            BrzError::TransportError("Connection lost while sending request.".to_string()),
            BrzError::TransportError("Connection closed early".to_string()),
            BrzError::ConnectionError("[Errno 32] Broken pipe".to_string()),
            BrzError::IncompleteRead(vec![], Some(10)),
            BrzError::InvalidHttpResponse(
                "http://example.com/codebase".to_string(),
                "Bad status line received".to_string(),
                None,
                HashMap::new(),
            ),
            http_status(502),
            http_status(503),
            http_status(504),
        ] {
            assert!(connection_lost(&e).is_some(), "not retried: {:?}", e);
        }
        for e in [
            BrzError::TransportError("No space left on device".to_string()),
            BrzError::TransportError("Too many open files".to_string()),
            BrzError::RemoteGitError("missing necessary objects".to_string()),
            http_status(401),
            http_status(404),
            http_status(500),
        ] {
            assert!(connection_lost(&e).is_none(), "retried: {:?}", e);
        }
    }
}
