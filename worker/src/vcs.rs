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
    let branch_name = janitor::vcs::segment_branch_name(url);
    let (url, _params) = breezyshim::urlutils::split_segment_parameters(url);
    let vcs_type = vcs_type.map_or_else(
        || source_branch.controldir().cloning_metadir(),
        |t| {
            breezyshim::controldir::FORMAT_REGISTRY
                .make_controldir(t.to_string().as_str())
                .unwrap()
        },
    );
    let target = match breezyshim::controldir::open(&url, possible_transports.as_mut()) {
        Ok(cd) => cd,
        Err(BrzError::NotBranchError(..)) => {
            breezyshim::controldir::create(&url, &vcs_type, possible_transports.as_mut())?
        }
        Err(e) => {
            return Err(e);
        }
    };

    target.push_branch(
        source_branch,
        branch_name.as_deref(),
        stop_revision.as_ref(),
        Some(overwrite),
        tag_selector,
    )?;

    Ok(())
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
        import_branches_bzr(
            repo_url,
            local_branch,
            campaign,
            log_id,
            branches,
            tags,
            update_current,
        )
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
        import_branches_git(
            repo_url,
            local_branch,
            campaign,
            log_id,
            branches,
            tags,
            update_current,
        )
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
}

#[cfg(test)]
mod segment_branch_name_tests {
    use super::*;

    #[test]
    fn test_decodes_a_slash_in_the_name() {
        // cache_branch_name returns "<vendor>/latest", and get_branch_url
        // escapes it.
        let url = Url::parse("http://git-store:9923/mypkg,branch=debian%2Flatest").unwrap();
        assert_eq!(
            janitor::vcs::segment_branch_name(&url).as_deref(),
            Some("debian/latest")
        );
    }

    #[test]
    fn test_round_trips_what_get_branch_url_writes() {
        // The pair that matters: the writer escapes, the reader gets it back.
        // Fails on upstream/main, where get_branch_url does not escape.
        use janitor::vcs::VcsManager;
        let mgr =
            janitor::vcs::RemoteGitVcsManager::new(Url::parse("http://git-store:9923/").unwrap());
        let url = mgr.get_branch_url("mypkg", "debian/latest");
        assert_eq!(
            janitor::vcs::segment_branch_name(&url).as_deref(),
            Some("debian/latest")
        );
    }

    #[test]
    fn test_none_for_a_path_based_url() {
        // The bzr managers build a path instead of a segment parameter, so
        // there is nothing to read and the caller keeps its old behaviour.
        let url = Url::parse("http://bzr-store:9929/mypkg/lintian-fixes/main").unwrap();
        assert_eq!(janitor::vcs::segment_branch_name(&url), None);
    }
}
