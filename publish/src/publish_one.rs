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

/// Map the transient or missing-branch errors from opening a branch to a
/// failure coded `{prefix}rate-limited`, `{prefix}missing` etc. Any other
/// error is passed to `other`.
fn branch_open_failure(
    e: BranchOpenError,
    label: &str,
    prefix: &str,
    other: impl FnOnce(BranchOpenError) -> PublishError,
) -> PublishError {
    let (kind, description) = match e {
        BranchOpenError::RateLimited { description, .. } => ("rate-limited", description),
        BranchOpenError::TemporarilyUnavailable { description, .. } => {
            ("temporarily-unavailable", description)
        }
        BranchOpenError::Unavailable { description, .. } => ("unavailable", description),
        BranchOpenError::Missing { description, .. } => ("missing", description),
        e => return other(e),
    };
    PublishError::failure(
        format!("{}{}", prefix, kind),
        format!("{} {}: {}", label, kind.replace('-', " "), description),
    )
}

/// Open the source and target branches for `request` and call the
/// underlying publish function.
pub fn publish_one(
    template_env: Environment,
    request: &crate::PublishOneRequest,
    possible_transports: &mut Option<Vec<Transport>>,
) -> Result<(PublishOneResult, String), PublishError> {
    let mut args = match shlex::split(&request.command) {
        Some(args) => args,
        None => {
            return Err(PublishError::failure(
                "invalid-command-syntax",
                format!("Invalid command syntax: {}", request.command),
            ));
        }
    };
    drop_env(&mut args);

    let mut source_branch = open_branch(
        &request.source_branch_url,
        possible_transports.as_mut(),
        None,
        None,
    )
    .map_err(|e| {
        branch_open_failure(e, "Local branch", "local-branch-", |e| {
            PublishError::failure("local-branch-error", format!("Local branch error: {}", e))
        })
    })?;

    let temp_sprout = if is_remote_git_branch(&source_branch) {
        match silver_platter::utils::TempSprout::new(&source_branch, None) {
            Ok(sprout) => {
                source_branch = sprout.tree().branch();
                Some(sprout)
            }
            Err(e) => {
                return Err(PublishError::failure(
                    "temp-sprout-failed",
                    format!("Failed to create temporary branch sprout: {}", e),
                ));
            }
        }
    } else {
        None
    };

    let target_branch = open_branch(
        &request.target_branch_url,
        possible_transports.as_mut(),
        None,
        None,
    )
    .map_err(|e| {
        branch_open_failure(e, "Target branch", "branch-", |e| match e {
            BranchOpenError::Unsupported { description, .. } => PublishError::failure(
                "branch-unsupported",
                format!("Target branch unsupported: {}", description),
            ),
            e => PublishError::failure("branch-error", format!("Target branch error: {}", e)),
        })
    })?;

    assert_ne!(request.mode, Mode::Bts);

    let forge: Option<Forge> = match breezyshim::forge::get_forge(&target_branch) {
        Err(e @ BrzError::UnsupportedForge(..)) => {
            if ![Mode::Push, Mode::BuildOnly].contains(&request.mode) {
                let url = target_branch.get_user_url();
                let netloc = url.host_str().unwrap_or("unknown");
                return Err(PublishError::failure(
                    "hoster-unsupported",
                    format!("Forge unsupported: {}.", netloc),
                ));
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
                let netloc = url.host_str().unwrap_or("unknown");
                return Err(PublishError::failure(
                    "hoster-no-login",
                    format!("Forge {} supported but no login known.", netloc),
                ));
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
                return Err(PublishError::failure(
                    "bad-gateway",
                    if let Some(extra) = extra {
                        format!("Bad gateway: {}", extra)
                    } else {
                        "Bad gateway.".to_string()
                    },
                ));
            }
            429 => {
                return Err(PublishError::failure(
                    "too-many-requests",
                    if let Some(extra) = extra {
                        format!("Too many requests: {}", extra)
                    } else {
                        "Too many requests.".to_string()
                    },
                ));
            }
            _ => {
                return Err(PublishError::failure(
                    format!("http-{}", code),
                    format!("HTTP error: {}", code),
                ));
            }
        },
        Err(_) => {
            return Err(PublishError::failure(
                "unexpected-error",
                "Unexpected error.",
            ));
        }
        Ok(forge) => Some(forge),
    };

    let (resume_branch, _overwrite, existing_proposal) = if let Some(forge) = forge.as_ref() {
        if let Some(existing_mp_url) = request.existing_mp_url.as_ref() {
            let existing_proposal = match forge.get_proposal_by_url(existing_mp_url) {
                Ok(proposal) => proposal,
                Err(BrzError::UnsupportedForge(..)) => {
                    return Err(PublishError::failure(
                        "forge-mp-url-mismatch",
                        format!("Forge unsupported: {}.", forge.base_url()),
                    ));
                }
                Err(e) => {
                    return Err(PublishError::failure(
                        "merge-proposal-retrieval-failed",
                        format!("Unable to retrieve merge proposal: {}", e),
                    ));
                }
            };
            let source_branch_url = match existing_proposal.get_source_branch_url() {
                Ok(Some(url)) => url,
                Ok(None) => {
                    return Err(PublishError::failure(
                        "merge-proposal-no-source-url",
                        "Merge proposal has no source branch URL",
                    ));
                }
                Err(e) => {
                    return Err(PublishError::failure(
                        "merge-proposal-source-url-failed",
                        format!("Unable to get source branch URL: {}", e),
                    ));
                }
            };
            let resume_branch =
                open_branch(&source_branch_url, possible_transports.as_mut(), None, None).map_err(
                    |e| {
                        branch_open_failure(e, "Resume branch", "resume-branch-", |e| {
                            PublishError::failure("unexpected-error", e.to_string())
                        })
                    },
                )?;
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
                        if existing_proposals.is_empty() {
                            (branch, overwrite, None)
                        } else {
                            if existing_proposals.len() > 1 {
                                log::warn!(
                                    "Multiple existing proposals: {:?}. Using {:?}",
                                    existing_proposals,
                                    existing_proposals[0],
                                );
                            }
                            (branch, overwrite, Some(existing_proposals.remove(0)))
                        }
                    } else {
                        (branch, overwrite, None)
                    }
                }
                Err(BrzError::NoSuchProject(project)) => {
                    if ![Mode::Push, Mode::BuildOnly].contains(&request.mode) {
                        return Err(PublishError::failure(
                            "project-not-found",
                            format!("Project {} not found.", project),
                        ));
                    }
                    (None, Some(false), None)
                }
                Err(BrzError::ForgeLoginRequired) => {
                    return Err(PublishError::failure(
                        "hoster-no-login",
                        format!("Forge {} supported but no login known.", forge),
                    ));
                }
                Err(BrzError::PermissionDenied(_, e)) => {
                    return Err(PublishError::failure(
                        "permission-denied",
                        if let Some(e) = e {
                            format!("Permission denied: {}", e)
                        } else {
                            "Permission denied.".to_string()
                        },
                    ));
                }
                Err(e) => {
                    return Err(PublishError::failure("unexpected-error", e.to_string()));
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
            return Err(PublishError::failure(
                "differ-unreachable",
                format!("Unable to contact differ for build diff: {}", e),
            ));
        }
        Err(crate::DebdiffError::MissingRun(missing_run_id)) => {
            if [Mode::Propose, Mode::AttemptPush].contains(&request.mode)
                && request.require_binary_diff
            {
                if missing_run_id == request.log_id {
                    return Err(PublishError::failure(
                        "missing-build-diff-self",
                        format!(
                            "Build diff is not available. Run ({}) not yet published?",
                            request.log_id
                        ),
                    ));
                } else {
                    return Err(PublishError::failure(
                        "missing-build-diff-control",
                        format!(
                            "Binary debdiff is not available. Control run ({}) not published?",
                            missing_run_id
                        ),
                    ));
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
                return Err(PublishError::failure(
                    "missing-build-diff-control",
                    "Binary debdiff is not available: no earlier run exists yet \
                             to compare against.",
                ));
            }
            None
        }
        Err(crate::DebdiffError::Http(e)) => {
            return Err(PublishError::failure(
                "differ-http-error",
                format!("Error from differ for build diff: HTTP {}", e),
            ));
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
        request.auto_merge,
    )?;

    if let Some(temp_sprout) = temp_sprout {
        std::mem::drop(temp_sprout);
    }

    Ok(result)
}

/// Publish changes from a source branch to a target branch for the
/// given mode (propose, push, etc.), generating descriptions and commit
/// messages from the campaign templates.
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
            if let (Some(vs_obj), Some(extra_obj)) = (vs.as_object_mut(), extra_context.as_object())
            {
                vs_obj.extend(extra_obj.clone());
            } else {
                log::warn!("Failed to merge extra_context: not an object");
            }
        }
        if let (Some(vs_obj), Some(codemod_obj)) = (vs.as_object_mut(), codemod_result.as_object())
        {
            vs_obj.extend(codemod_obj.clone());
        } else {
            log::warn!("Failed to merge codemod_result: not an object");
        }
        vs["codemod"] = codemod_result.clone();
        if let Some(debdiff) = debdiff.as_ref() {
            match std::str::from_utf8(debdiff) {
                Ok(debdiff_str) => {
                    vs["debdiff"] = debdiff_str.into();
                }
                Err(e) => {
                    log::warn!("Debdiff contains invalid UTF-8: {}", e);
                    vs["debdiff"] = format!("(Binary diff - {} bytes)", debdiff.len()).into();
                }
            }
        }
        let template_name = if description_format == DescriptionFormat::Markdown {
            format!("{}.md", campaign)
        } else {
            format!("{}.txt", campaign)
        };
        let template = match template_env.get_template(&template_name) {
            Ok(template) => template,
            Err(e) => {
                log::warn!("Template {} not found: {}, using default", template_name, e);
                // Return a simple default description
                return format!("Changes for {} ({})", campaign, log_id);
            }
        };
        match template.render(vs) {
            Ok(rendered) => rendered,
            Err(e) => {
                log::warn!("Template rendering failed: {}, using default", e);
                format!("Changes for {} ({})", campaign, log_id)
            }
        }
    };

    let get_proposal_commit_message =
        |_existing_proposal: Option<&MergeProposal>| -> Option<String> {
            commit_message_template.and_then(|commit_message_template| {
                match template_env.render_named_str(
                    "commit_message",
                    commit_message_template,
                    codemod_result,
                ) {
                    Ok(rendered) => Some(rendered),
                    Err(e) => {
                        log::warn!("Commit message template rendering failed: {}", e);
                        None
                    }
                }
            })
        };

    let get_proposal_title = |existing_proposal: Option<&MergeProposal>| -> Option<String> {
        if let Some(title_template) = title_template.as_ref() {
            match template_env.render_named_str("title", title_template, codemod_result) {
                Ok(rendered) => Some(rendered),
                Err(e) => {
                    log::warn!("Title template rendering failed: {}", e);
                    // Fall back to determining title from description
                    match determine_title(&get_proposal_description(
                        DescriptionFormat::Plain,
                        existing_proposal,
                    )) {
                        Ok(title) => Some(title),
                        Err(e) => {
                            log::warn!("Failed to determine fallback title: {}", e);
                            Some(format!("Changes for {}", campaign))
                        }
                    }
                }
            }
        } else {
            match determine_title(&get_proposal_description(
                DescriptionFormat::Plain,
                existing_proposal,
            )) {
                Ok(title) => Some(title),
                Err(e) => {
                    log::warn!("Failed to determine title: {}", e);
                    None
                }
            }
        }
    };

    let target_lock = target_branch.lock_read();
    let source_lock = source_branch.lock_read();

    match merge_conflicts(&target_branch, &source_branch, stop_revision) {
        Ok(true) => {
            return Err(PublishError::failure(
                "merge-conflict",
                "merge would conflict (upstream changes?)",
            ));
        }
        Ok(false) => {}
        Err(BrzError::NoSuchRevision(revision)) => {
            return Err(PublishError::failure(
                "revision-missing",
                format!("Revision missing: {}", revision),
            ));
        }
        Err(e) => {
            return Err(PublishError::failure(
                "merge-conflict-check-failed",
                format!("Merge conflict check failed: {}", e),
            ));
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
        mode.try_into()
            .map_err(|_| PublishError::failure("invalid-mode", "Invalid publish mode"))?,
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
        Err(SvpPublishError::Other(BrzError::DivergedBranches)) => Err(PublishError::failure(
            "diverged-branches",
            "Upstream branch has diverged from local changes.",
        )),
        Err(SvpPublishError::UnsupportedForge(_)) => Err(PublishError::failure(
            "hoster-unsupported",
            format!(
                "Forge unsupported: {}",
                target_branch.repository().get_user_url()
            ),
        )),
        Err(SvpPublishError::Other(BrzError::NoSuchProject(_))) => Err(PublishError::failure(
            "project-not-found",
            format!(
                "Project not found: {}",
                target_branch.repository().get_user_url()
            ),
        )),
        Err(SvpPublishError::Other(BrzError::ForkingDisabled(_))) => Err(PublishError::failure(
            "forking-disabled",
            format!(
                "Forking disabled: {}",
                target_branch.repository().get_user_url()
            ),
        )),
        Err(SvpPublishError::PermissionDenied) => Err(PublishError::failure(
            "permission-denied",
            "Permission denied.",
        )),
        Err(SvpPublishError::Other(BrzError::MergeProposalExists(e, _))) => Err(
            PublishError::failure("merge-proposal-exists", e.to_string()),
        ),
        Err(SvpPublishError::Other(BrzError::GitLabConflict(..))) => Err(PublishError::failure(
            "gitlab-conflict",
            "Conflict during GitLab operation. Reached repository limit?",
        )),
        Err(SvpPublishError::Other(BrzError::SourceNotDerivedFromTarget)) => {
            Err(PublishError::failure(
                "source-not-derived-from-target",
                "The source repository is not a fork of the target repository.",
            ))
        }
        Err(SvpPublishError::Other(BrzError::ProjectCreationTimeout(project, timeout))) => {
            Err(PublishError::failure(
                "project-creation-timeout",
                format!(
                    "Forking the project (to {}) timed out ({}s)",
                    project, timeout
                ),
            ))
        }
        Err(SvpPublishError::Other(BrzError::RemoteGitError(e))) => Err(PublishError::failure(
            "remote-git-error",
            format!("Remote git error: {}", e),
        )),
        Err(SvpPublishError::InsufficientChangesForNewProposal) => Err(PublishError::NothingToDo(
            "not enough changes for a new merge proposal".to_string(),
        )),
        Err(SvpPublishError::BranchOpenError(BranchOpenError::TemporarilyUnavailable {
            description,
            ..
        })) => Err(PublishError::failure(
            "branch-temporarily-unavailable",
            format!("Branch temporarily unavailable: {}", description),
        )),
        Err(SvpPublishError::BranchOpenError(BranchOpenError::Unavailable {
            description, ..
        })) => Err(PublishError::failure(
            "branch-unavailable",
            format!("Branch unavailable: {}", description),
        )),
        Err(SvpPublishError::EmptyMergeProposal) => Err(PublishError::failure(
            "empty-merge-proposal",
            "No changes to propose; changes made independently upstream?",
        )),
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
        Err(e) => Err(PublishError::failure(
            "publish-error",
            format!("Publish error: {}", e),
        )),
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
                .and_then(|proposal| proposal.url().ok()),
            proposal_web_url: publish_result
                .proposal
                .as_ref()
                .and_then(|proposal| proposal.get_web_url().ok()),
            is_new: publish_result.proposal.and(publish_result.is_new),
            target_branch_url: publish_result.target_branch.get_user_url(),
            target_branch_web_url: publish_result
                .forge
                .and_then(|forge| forge.get_web_url(&publish_result.target_branch).ok()),
            branch_name,
            mode: publish_result.mode,
        }
    }
}

#[cfg(test)]
#[path = "publish_one_tests.rs"]
mod tests;
