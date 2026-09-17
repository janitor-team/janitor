//! Publisher for a single branch.
//!
//! This is the worker module for the publish service. For each branch that needs
//! to be published, this module gets invoked. It accepts some JSON on stdin with a
//! request, and writes results to standard out as JSON.

use crate::Mode;
use crate::PublishError;
use breezyshim::branch::{Branch, GenericBranch};
use breezyshim::error::Error as BrzError;
use breezyshim::forge::{determine_title, Forge, MergeProposal};
use breezyshim::repository::Repository;
use breezyshim::transport::Transport;
use breezyshim::RevisionId;
use minijinja::Environment;
use silver_platter::publish::{publish_changes, DescriptionFormat, Error as SvpPublishError};
use silver_platter::utils::merge_conflicts;
use silver_platter::vcs::{full_branch_url, open_branch, BranchOpenError};
use std::collections::HashMap;

fn drop_env(args: &mut Vec<String>) {
    while !args.is_empty() && args[0].contains('=') {
        args.remove(0);
    }
}

fn is_remote_git_branch(branch: &dyn Branch) -> bool {
    let url = branch.get_user_url();
    let vcs_type = branch.repository().vcs_type();
    vcs_type == breezyshim::foreign::VcsType::Git && url.scheme() != "file"
}

/// Publish a single branch based on a request.
///
/// This handles opening the source and target branches and calling the publish function.
pub fn publish_one(
    template_env: Environment,
    request: &crate::PublishOneRequest,
    possible_transports: &mut Option<Vec<Transport>>,
) -> Result<(PublishOneResult, String), PublishError> {
    let mut args = shlex::split(&request.command).unwrap();
    drop_env(&mut args);

    let mut source_branch = match open_branch(
        &request.source_branch_url,
        possible_transports.as_mut(),
        None,
        None,
    ) {
        Ok(branch) => branch,
        Err(BranchOpenError::RateLimited { description, .. }) => {
            panic!("Local branch rate limited: {}", description);
        }
        Err(BranchOpenError::TemporarilyUnavailable { description, .. }) => {
            return Err(PublishError::Failure {
                description: format!("Local branch temporarily unavailable: {}", description),
                code: "local-branch-temporarily-unavailable".to_string(),
            });
        }
        Err(BranchOpenError::Unavailable { description, .. }) => {
            return Err(PublishError::Failure {
                description: format!("Local branch unavailable: {}", description),
                code: "local-branch-unavailable".to_string(),
            });
        }
        Err(BranchOpenError::Missing { description, .. }) => {
            return Err(PublishError::Failure {
                description: format!("Local branch missing: {}", description),
                code: "local-branch-missing".to_string(),
            });
        }
        Err(e) => {
            return Err(PublishError::Failure {
                description: format!("Local branch error: {}", e),
                code: "local-branch-error".to_string(),
            });
        }
    };

    let temp_sprout = if is_remote_git_branch(&source_branch) {
        let sprout = silver_platter::utils::TempSprout::new(&source_branch, None).unwrap();
        source_branch = sprout.tree().branch();
        Some(sprout)
    } else {
        None
    };

    let target_branch = match open_branch(
        &request.target_branch_url,
        possible_transports.as_mut(),
        None,
        None,
    ) {
        Ok(branch) => branch,
        Err(BranchOpenError::RateLimited { description, .. }) => {
            return Err(PublishError::Failure {
                description: format!("Target branch rate limited: {}", description),
                code: "branch-rate-limited".to_string(),
            });
        }
        Err(BranchOpenError::TemporarilyUnavailable { description, .. }) => {
            return Err(PublishError::Failure {
                description: format!("Target branch temporarily unavailable: {}", description),
                code: "branch-temporarily-unavailable".to_string(),
            });
        }
        Err(BranchOpenError::Unavailable { description, .. }) => {
            return Err(PublishError::Failure {
                description: format!("Target branch unavailable: {}", description),
                code: "branch-unavailable".to_string(),
            });
        }
        Err(BranchOpenError::Missing { description, .. }) => {
            return Err(PublishError::Failure {
                description: format!("Target branch missing: {}", description),
                code: "branch-missing".to_string(),
            });
        }
        Err(BranchOpenError::Unsupported { description, .. }) => {
            return Err(PublishError::Failure {
                description: format!("Target branch unsupported: {}", description),
                code: "branch-unsupported".to_string(),
            });
        }
        Err(e) => {
            return Err(PublishError::Failure {
                description: format!("Target branch error: {}", e),
                code: "branch-error".to_string(),
            });
        }
    };

    assert_ne!(request.mode, Mode::Bts);

    let forge: Option<Forge> = match breezyshim::forge::get_forge(&target_branch) {
        Err(e @ BrzError::UnsupportedForge(..)) => {
            if ![Mode::Push, Mode::BuildOnly].contains(&request.mode) {
                let url = target_branch.get_user_url();
                let netloc = url.host_str().unwrap();
                return Err(PublishError::Failure {
                    description: format!("Forge unsupported: {}.", netloc),
                    code: "hoster-unsupported".to_string(),
                });
            }
            // We can't figure out what branch to resume from when there's no forge that can tell us.
            if request.mode == Mode::Push {
                log::warn!(
                    "Unsupported forge ({}), will attempt to push to {}",
                    e,
                    full_branch_url(&target_branch),
                )
            }
            None
        }
        Err(e @ BrzError::ForgeLoginRequired) => {
            if ![Mode::Push, Mode::BuildOnly].contains(&request.mode) {
                let url = target_branch.get_user_url();
                let netloc = url.host_str().unwrap();
                return Err(PublishError::Failure {
                    description: format!("Forge {} supported but no login known.", netloc),
                    code: "hoster-no-login".to_string(),
                });
            }
            // We can't figure out what branch to resume from when there's no forge that can tell us.
            if request.mode == Mode::Push {
                log::warn!(
                    "No login for forge ({}), will attempt to push to {}",
                    e,
                    full_branch_url(&target_branch),
                );
            }
            None
        }
        Err(BrzError::UnexpectedHttpStatus { code, extra, .. }) => match code {
            502 => {
                return Err(PublishError::Failure {
                    description: if let Some(extra) = extra {
                        format!("Bad gateway: {}", extra)
                    } else {
                        "Bad gateway.".to_string()
                    },
                    code: "bad-gateway".to_string(),
                });
            }
            429 => {
                return Err(PublishError::Failure {
                    description: if let Some(extra) = extra {
                        format!("Too many requests: {}", extra)
                    } else {
                        "Too many requests.".to_string()
                    },
                    code: "too-many-requests".to_string(),
                });
            }
            _ => {
                return Err(PublishError::Failure {
                    description: format!("HTTP error: {}", code),
                    code: format!("http-{}", code),
                });
            }
        },
        Err(_) => {
            return Err(PublishError::Failure {
                description: "Unexpected error.".to_string(),
                code: "unexpected-error".to_string(),
            });
        }
        Ok(forge) => Some(forge),
    };

    let (resume_branch, _overwrite, existing_proposal) = if let Some(forge) = forge.as_ref() {
        if let Some(existing_mp_url) = request.existing_mp_url.as_ref() {
            let existing_proposal = match forge.get_proposal_by_url(existing_mp_url) {
                Ok(proposal) => proposal,
                Err(BrzError::UnsupportedForge(..)) => {
                    return Err(PublishError::Failure {
                        description: format!("Forge unsupported: {}.", forge.base_url()),
                        code: "forge-mp-url-mismatch".to_string(),
                    });
                }
                Err(e) => {
                    panic!("Unexpected error: {}", e);
                }
            };
            let resume_branch = match open_branch(
                &existing_proposal.get_source_branch_url().unwrap().unwrap(),
                possible_transports.as_mut(),
                None,
                None,
            ) {
                Ok(branch) => branch,
                Err(BranchOpenError::RateLimited { description, .. }) => {
                    return Err(PublishError::Failure {
                        description: format!("Resume branch rate limited: {}", description),
                        code: "resume-branch-rate-limited".to_string(),
                    });
                }
                Err(BranchOpenError::TemporarilyUnavailable { description, .. }) => {
                    return Err(PublishError::Failure {
                        description: format!(
                            "Resume branch temporarily unavailable: {}",
                            description
                        ),
                        code: "resume-branch-temporarily-unavailable".to_string(),
                    });
                }
                Err(BranchOpenError::Unavailable { description, .. }) => {
                    return Err(PublishError::Failure {
                        description: format!("Resume branch unavailable: {}", description),
                        code: "resume-branch-unavailable".to_string(),
                    });
                }
                Err(BranchOpenError::Missing { description, .. }) => {
                    return Err(PublishError::Failure {
                        description: format!("Resume branch missing: {}", description),
                        code: "resume-branch-missing".to_string(),
                    });
                }
                Err(e) => {
                    return Err(PublishError::Failure {
                        description: e.to_string(),
                        code: "unexpected-error".to_string(),
                    });
                }
            };
            (Some(resume_branch), Some(true), Some(existing_proposal))
        } else {
            match silver_platter::publish::find_existing_proposed(
                &target_branch,
                forge,
                &request.derived_branch_name,
                false,
                None,
                None,
            ) {
                Ok((branch, overwrite, existing_proposals)) => {
                    if let Some(mut existing_proposals) = existing_proposals {
                        if existing_proposals.len() > 1 {
                            log::warn!(
                                "Multiple existing proposals: {:?}. Using {:?}",
                                existing_proposals,
                                existing_proposals[0],
                            );
                        }
                        (branch, overwrite, Some(existing_proposals.remove(0)))
                    } else {
                        (branch, overwrite, None)
                    }
                }
                Err(BrzError::NoSuchProject(project)) => {
                    if ![Mode::Push, Mode::BuildOnly].contains(&request.mode) {
                        return Err(PublishError::Failure {
                            description: format!("Project {} not found.", project),
                            code: "project-not-found".to_string(),
                        });
                    }
                    (None, Some(false), None)
                }
                Err(BrzError::ForgeLoginRequired) => {
                    return Err(PublishError::Failure {
                        description: format!("Forge {} supported but no login known.", forge),
                        code: "hoster-no-login".to_string(),
                    });
                }
                Err(BrzError::PermissionDenied(_, e)) => {
                    return Err(PublishError::Failure {
                        description: if let Some(e) = e {
                            format!("Permission denied: {}", e)
                        } else {
                            "Permission denied.".to_string()
                        },
                        code: "permission-denied".to_string(),
                    });
                }
                Err(e) => {
                    return Err(PublishError::Failure {
                        description: e.to_string(),
                        code: "unexpected-error".to_string(),
                    });
                }
            }
        }
    } else {
        (None, None, None)
    };

    let debdiff = match crate::get_debdiff(
        &request.differ_url,
        request.unchanged_id.as_deref(),
        &request.log_id,
    ) {
        Ok(debdiff) => Some(debdiff),
        Err(crate::DebdiffError::Unavailable(e)) => {
            return Err(PublishError::Failure {
                description: format!("Unable to contact differ for build diff: {}", e),
                code: "differ-unreachable".to_string(),
            });
        }
        Err(crate::DebdiffError::MissingRun(missing_run_id)) => {
            if [Mode::Propose, Mode::AttemptPush].contains(&request.mode)
                && request.require_binary_diff
            {
                if missing_run_id == request.log_id {
                    return Err(PublishError::Failure {
                        description: format!(
                            "Build diff is not available. Run ({}) not yet published?",
                            request.log_id
                        ),
                        code: "missing-build-diff-self".to_string(),
                    });
                } else {
                    return Err(PublishError::Failure {
                        description: format!(
                            "Binary debdiff is not available. Control run ({}) not published?",
                            missing_run_id
                        ),
                        code: "missing-build-diff-control".to_string(),
                    });
                }
            }
            None
        }
        Err(crate::DebdiffError::NoUnchangedRun) => {
            // Expected for a codebase's first run: there's no earlier
            // build to diff against yet, not a run that failed to
            // publish. Only actually blocks publishing when a binary
            // diff is required.
            if [Mode::Propose, Mode::AttemptPush].contains(&request.mode)
                && request.require_binary_diff
            {
                return Err(PublishError::Failure {
                    description: "Binary debdiff is not available: no earlier run exists yet \
                             to compare against."
                        .to_string(),
                    code: "missing-build-diff-control".to_string(),
                });
            }
            None
        }
        Err(crate::DebdiffError::Http(e)) => {
            return Err(PublishError::Failure {
                description: format!("Error from differ for build diff: HTTP {}", e),
                code: "differ-http-error".to_string(),
            });
        }
    };

    let result = publish(
        template_env,
        &request.campaign,
        request.commit_message_template.as_deref(),
        request.title_template.as_deref(),
        &request.codemod_result,
        request.mode,
        &request.role,
        forge,
        target_branch,
        source_branch,
        &request.derived_branch_name,
        resume_branch,
        &request.log_id,
        existing_proposal,
        request.allow_create_proposal,
        debdiff,
        request.reviewers.clone(),
        request.tags.clone(),
        Some(&request.revision_id),
        request.extra_context.clone(),
        request.derived_owner.clone(),
        request.auto_merge.clone(),
    )?;

    if let Some(temp_sprout) = temp_sprout {
        std::mem::drop(temp_sprout);
    }

    Ok(result)
}

/// Publish changes from a source branch to a target branch.
///
/// This is the core function for publishing changes, handling different modes (propose, push, etc.)
/// and generating appropriate descriptions and commit messages.
pub fn publish(
    template_env: Environment,
    campaign: &str,
    commit_message_template: Option<&str>,
    title_template: Option<&str>,
    codemod_result: &serde_json::Value,
    mode: Mode,
    role: &str,
    forge: Option<Forge>,
    target_branch: GenericBranch,
    source_branch: GenericBranch,
    derived_branch_name: &str,
    resume_branch: Option<GenericBranch>,
    log_id: &str,
    existing_proposal: Option<MergeProposal>,
    allow_create_proposal: bool,
    debdiff: Option<Vec<u8>>,
    reviewers: Option<Vec<String>>,
    result_tags: Option<HashMap<String, RevisionId>>,
    stop_revision: Option<&RevisionId>,
    extra_context: Option<serde_json::Value>,
    derived_owner: Option<String>,
    auto_merge: Option<bool>,
) -> Result<(PublishOneResult, String), PublishError> {
    let get_proposal_description = |description_format: DescriptionFormat,
                                    _existing_proposal: Option<&MergeProposal>|
     -> String {
        let mut vs = serde_json::json!({
            "log_id":  log_id,
            "campaign": campaign,
            "role": role,
        });
        if let Some(extra_context) = extra_context.as_ref() {
            vs.as_object_mut()
                .unwrap()
                .extend(extra_context.as_object().unwrap().clone());
        }
        vs.as_object_mut()
            .unwrap()
            .extend(codemod_result.as_object().unwrap().clone());
        vs["codemod"] = codemod_result.clone();
        if let Some(debdiff) = debdiff.as_ref() {
            vs["debdiff"] = std::str::from_utf8(debdiff).unwrap().into();
        }
        let template = if description_format == DescriptionFormat::Markdown {
            template_env.get_template(&format!("{}.md", campaign))
        } else {
            template_env.get_template(&format!("{}.txt", campaign))
        }
        .unwrap();
        template.render(vs).unwrap()
    };

    let get_proposal_commit_message =
        |_existing_proposal: Option<&MergeProposal>| -> Option<String> {
            commit_message_template.map(|commit_message_template| {
                template_env
                    .render_named_str("commit_message", commit_message_template, codemod_result)
                    .unwrap()
            })
        };

    let get_proposal_title = |existing_proposal: Option<&MergeProposal>| -> Option<String> {
        if let Some(title_template) = title_template.as_ref() {
            Some(
                template_env
                    .render_named_str("title", title_template, codemod_result)
                    .unwrap(),
            )
        } else {
            Some(
                determine_title(&get_proposal_description(
                    DescriptionFormat::Plain,
                    existing_proposal,
                ))
                .unwrap_or_default(),
            )
        }
    };

    let target_lock = target_branch.lock_read();
    let source_lock = source_branch.lock_read();

    match merge_conflicts(&target_branch, &source_branch, stop_revision) {
        Ok(true) => {
            return Err(PublishError::Failure {
                code: "merge-conflict".to_string(),
                description: "merge would conflict (upstream changes?)".to_string(),
            });
        }
        Ok(false) => {}
        Err(BrzError::NoSuchRevision(revision)) => {
            return Err(PublishError::Failure {
                description: format!("Revision missing: {}", revision),
                code: "revision-missing".to_string(),
            });
        }
        Err(e) => {
            return Err(PublishError::Failure {
                description: format!("Merge conflict check failed: {}", e),
                code: "merge-conflict-check-failed".to_string(),
            });
        }
    }

    let labels = if forge
        .as_ref()
        .map(|x| x.supports_merge_proposal_labels())
        .unwrap_or(false)
    {
        Some(vec![campaign.to_string()])
    } else {
        None
    };

    match publish_changes(
        &source_branch,
        &target_branch,
        resume_branch.as_ref(),
        mode.try_into().unwrap(),
        derived_branch_name,
        get_proposal_description,
        Some(get_proposal_commit_message),
        Some(get_proposal_title),
        forge.as_ref(),
        Some(allow_create_proposal),
        labels,
        Some(true),
        existing_proposal,
        reviewers,
        result_tags,
        derived_owner.as_deref(),
        Some(true),
        stop_revision,
        auto_merge,
        None, // work_in_progress
    ) {
        Err(SvpPublishError::Other(BrzError::DivergedBranches)) => Err(PublishError::Failure {
            description: "Upstream branch has diverged from local changes.".to_string(),
            code: "diverged-branches".to_string(),
        }),
        Err(SvpPublishError::UnsupportedForge(_)) => Err(PublishError::Failure {
            description: format!(
                "Forge unsupported: {}",
                target_branch.repository().get_user_url()
            ),
            code: "hoster-unsupported".to_string(),
        }),
        Err(SvpPublishError::Other(BrzError::NoSuchProject(_))) => Err(PublishError::Failure {
            description: format!(
                "Project not found: {}",
                target_branch.repository().get_user_url()
            ),
            code: "project-not-found".to_string(),
        }),
        Err(SvpPublishError::Other(BrzError::ForkingDisabled(_))) => Err(PublishError::Failure {
            description: format!(
                "Forking disabled: {}",
                target_branch.repository().get_user_url()
            ),
            code: "forking-disabled".to_string(),
        }),
        Err(SvpPublishError::PermissionDenied) => Err(PublishError::Failure {
            description: "Permission denied.".to_string(),
            code: "permission-denied".to_string(),
        }),
        Err(SvpPublishError::Other(BrzError::MergeProposalExists(e, _))) => {
            Err(PublishError::Failure {
                description: e.to_string(),
                code: "merge-proposal-exists".to_string(),
            })
        }
        Err(SvpPublishError::Other(BrzError::GitLabConflict(..))) => Err(PublishError::Failure {
            description: "Conflict during GitLab operation. Reached repository limit?".to_string(),
            code: "gitlab-conflict".to_string(),
        }),
        Err(SvpPublishError::Other(BrzError::SourceNotDerivedFromTarget)) => {
            Err(PublishError::Failure {
                description: "The source repository is not a fork of the target repository."
                    .to_string(),
                code: "source-not-derived-from-target".to_string(),
            })
        }
        Err(SvpPublishError::Other(BrzError::ProjectCreationTimeout(project, timeout))) => {
            Err(PublishError::Failure {
                description: format!(
                    "Forking the project (to {}) timed out ({}s)",
                    project, timeout
                ),
                code: "project-creation-timeout".to_string(),
            })
        }
        Err(SvpPublishError::Other(BrzError::RemoteGitError(e))) => Err(PublishError::Failure {
            description: format!("Remote git error: {}", e),
            code: "remote-git-error".to_string(),
        }),
        Err(SvpPublishError::InsufficientChangesForNewProposal) => Err(PublishError::NothingToDo(
            "not enough changes for a new merge proposal".to_string(),
        )),
        Err(SvpPublishError::BranchOpenError(BranchOpenError::TemporarilyUnavailable {
            description,
            ..
        })) => Err(PublishError::Failure {
            description: format!("Branch temporarily unavailable: {}", description),
            code: "branch-temporarily-unavailable".to_string(),
        }),
        Err(SvpPublishError::BranchOpenError(BranchOpenError::Unavailable {
            description, ..
        })) => Err(PublishError::Failure {
            description: format!("Branch unavailable: {}", description),
            code: "branch-unavailable".to_string(),
        }),
        Err(SvpPublishError::EmptyMergeProposal) => Err(PublishError::Failure {
            code: "empty-merge-proposal".to_string(),
            description: "No changes to propose; changes made independently upstream?".to_string(),
        }),
        Ok(publish_result) => {
            std::mem::drop(target_lock);
            std::mem::drop(source_lock);
            Ok((
                PublishOneResult {
                    mode,
                    proposal: publish_result.proposal,
                    is_new: publish_result.is_new,
                    target_branch,
                    forge: publish_result.forge,
                },
                derived_branch_name.to_string(),
            ))
        }
        Err(e) => Err(PublishError::Failure {
            description: format!("Publish error: {}", e),
            code: "publish-error".to_string(),
        }),
    }
}

/// Result of a publish operation.
pub struct PublishOneResult {
    mode: Mode,
    proposal: Option<MergeProposal>,
    is_new: Option<bool>,
    target_branch: GenericBranch,
    forge: Option<Forge>,
}

impl From<(PublishOneResult, String)> for crate::PublishOneResult {
    fn from(publish_result: (PublishOneResult, String)) -> crate::PublishOneResult {
        let branch_name = publish_result.1;
        let publish_result = publish_result.0;
        crate::PublishOneResult {
            proposal_url: publish_result
                .proposal
                .as_ref()
                .map(|proposal| proposal.url().unwrap()),
            proposal_web_url: publish_result
                .proposal
                .as_ref()
                .map(|proposal| proposal.get_web_url().unwrap()),
            is_new: publish_result.proposal.and(publish_result.is_new),
            target_branch_url: publish_result.target_branch.get_user_url(),
            target_branch_web_url: publish_result
                .forge
                .map(|forge| forge.get_web_url(&publish_result.target_branch).unwrap()),
            branch_name,
            mode: publish_result.mode,
        }
    }
}
