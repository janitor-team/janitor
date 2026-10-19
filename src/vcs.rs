use crate::CodebaseName;
use async_trait::async_trait;
use breezyshim::branch::Branch;
use breezyshim::error::Error as BrzError;
use breezyshim::repository::{GenericRepository, Repository};
use breezyshim::RevisionId;
use serde::{Deserialize, Serialize};
use silver_platter::vcs::BranchOpenError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use url::Url;

/// Error types for VCS configuration and setup
#[derive(Debug)]
pub enum VcsConfigError {
    /// URL parsing failed
    InvalidUrl(url::ParseError),
    /// File URL could not be converted to file path
    InvalidFilePath,
}

impl std::fmt::Display for VcsConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VcsConfigError::InvalidUrl(e) => write!(f, "Invalid VCS URL: {}", e),
            VcsConfigError::InvalidFilePath => {
                write!(f, "VCS file URL cannot be converted to file path")
            }
        }
    }
}

impl std::error::Error for VcsConfigError {}

impl From<url::ParseError> for VcsConfigError {
    fn from(err: url::ParseError) -> Self {
        VcsConfigError::InvalidUrl(err)
    }
}

pub fn is_authenticated_url(url: &Url) -> bool {
    ["git+ssh", "bzr+ssh"].contains(&url.scheme())
}

#[derive(Debug, Clone, PartialEq, Eq, Copy, std::hash::Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VcsType {
    Bzr,
    Git,
}

impl<'a> Deserialize<'a> for VcsType {
    fn deserialize<D>(deserializer: D) -> Result<VcsType, D::Error>
    where
        D: serde::Deserializer<'a>,
    {
        let s = String::deserialize(deserializer)?;
        std::str::FromStr::from_str(&s).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for VcsType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            VcsType::Bzr => f.write_str("bzr"),
            VcsType::Git => f.write_str("git"),
        }
    }
}

impl std::str::FromStr for VcsType {
    type Err = String;

    fn from_str(s: &str) -> Result<VcsType, String> {
        match s {
            "bzr" => Ok(VcsType::Bzr),
            "git" => Ok(VcsType::Git),
            _ => Err(format!("Unknown VCS type: {}", s)),
        }
    }
}

pub fn get_branch_vcs_type(branch: &dyn Branch) -> Result<VcsType, BrzError> {
    let repository = branch.repository();
    match repository.vcs_type() {
        breezyshim::foreign::VcsType::Git => Ok(VcsType::Git),
        breezyshim::foreign::VcsType::Bazaar => Ok(VcsType::Bzr),
        other => Err(BrzError::UnsupportedVcs(format!("{:?}", other))),
    }
}

pub fn is_alioth_url(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("svn.debian.org")
            | Some("bzr.debian.org")
            | Some("anonscm.debian.org")
            | Some("hg.debian.org")
            | Some("git.debian.org")
            | Some("alioth.debian.org")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_authenticated_urls() {
        assert!(is_authenticated_url(
            &Url::parse("git+ssh://example.com").unwrap()
        ));
        assert!(is_authenticated_url(
            &Url::parse("bzr+ssh://example.com").unwrap()
        ));
        assert!(is_authenticated_url(
            &Url::parse("git+ssh://git.example.com/repo.git").unwrap()
        ));
    }

    #[test]
    fn test_not_authenticated_urls() {
        assert!(!is_authenticated_url(
            &Url::parse("http://example.com").unwrap()
        ));
        assert!(!is_authenticated_url(
            &Url::parse("https://example.com").unwrap()
        ));
        assert!(!is_authenticated_url(
            &Url::parse("git://example.com/repo.git").unwrap()
        ));
    }

    #[test]
    fn test_alioth_urls() {
        assert!(is_alioth_url(
            &Url::parse("https://anonscm.debian.org/cgit/pkg-ocaml-maint/packages/ocamlbuild.git")
                .unwrap()
        ));
        assert!(is_alioth_url(
            &Url::parse("https://git.debian.org/git/pkg-ocaml-maint/packages/ocamlbuild.git")
                .unwrap()
        ));
        assert!(is_alioth_url(
            &Url::parse(
                "https://alioth.debian.org/anonscm/git/pkg-ocaml-maint/packages/ocamlbuild.git"
            )
            .unwrap()
        ));
        assert!(is_alioth_url(
            &Url::parse("https://svn.debian.org/svn/some-pkg").unwrap()
        ));
        assert!(is_alioth_url(
            &Url::parse("https://bzr.debian.org/bzr/some-pkg").unwrap()
        ));
        assert!(is_alioth_url(
            &Url::parse("https://hg.debian.org/hg/some-pkg").unwrap()
        ));
    }

    #[test]
    fn test_not_alioth_urls() {
        assert!(!is_alioth_url(&Url::parse("https://example.com").unwrap()));
        assert!(!is_alioth_url(
            &Url::parse("https://salsa.debian.org/foo/bar").unwrap()
        ));
        assert!(!is_alioth_url(
            &Url::parse("https://github.com/foo/bar").unwrap()
        ));
    }

    #[test]
    fn test_vcs_type_from_str() {
        assert_eq!("git".parse::<VcsType>().unwrap(), VcsType::Git);
        assert_eq!("bzr".parse::<VcsType>().unwrap(), VcsType::Bzr);
        assert!("svn".parse::<VcsType>().is_err());
        assert!("".parse::<VcsType>().is_err());
    }

    #[test]
    fn test_vcs_type_display() {
        assert_eq!(VcsType::Git.to_string(), "git");
        assert_eq!(VcsType::Bzr.to_string(), "bzr");
    }

    #[test]
    fn test_vcs_type_serde_roundtrip() {
        let git = VcsType::Git;
        let json = serde_json::to_string(&git).unwrap();
        assert_eq!(json, r#""git""#);
        let roundtripped: VcsType = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped, git);

        let bzr = VcsType::Bzr;
        let json = serde_json::to_string(&bzr).unwrap();
        assert_eq!(json, r#""bzr""#);
        let roundtripped: VcsType = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped, bzr);
    }

    #[test]
    fn test_vcs_type_serde_invalid() {
        assert!(serde_json::from_str::<VcsType>(r#""svn""#).is_err());
    }

    #[test]
    fn test_convert_branch_exception_rate_limited() {
        let err = BranchOpenError::RateLimited {
            url: Url::parse("https://example.com").unwrap(),
            description: "Too many requests".to_string(),
            retry_after: Some(60.0),
        };
        let failure = convert_branch_exception(&Url::parse("https://example.com").unwrap(), err);
        assert_eq!(failure.code, "too-many-requests");
        assert_eq!(failure.retry_after, Some(chrono::Duration::seconds(60)));
    }

    #[test]
    fn test_convert_branch_exception_missing() {
        let err = BranchOpenError::Missing {
            url: Url::parse("https://example.com/repo").unwrap(),
            description: "Not found".to_string(),
        };
        let failure =
            convert_branch_exception(&Url::parse("https://example.com/repo").unwrap(), err);
        assert_eq!(failure.code, "branch-missing");
    }

    #[test]
    fn test_convert_branch_exception_missing_alioth() {
        let err = BranchOpenError::Missing {
            url: Url::parse("https://anonscm.debian.org/repo").unwrap(),
            description: "Branch does not exist: Not a branch: \"https://anonscm.debian.org/repo\""
                .to_string(),
        };
        let failure =
            convert_branch_exception(&Url::parse("https://anonscm.debian.org/repo").unwrap(), err);
        assert_eq!(failure.code, "hosted-on-alioth");
    }

    #[test]
    fn test_convert_branch_exception_unavailable_alioth() {
        let err = BranchOpenError::Unavailable {
            url: Url::parse("https://anonscm.debian.org/repo").unwrap(),
            description: "Connection refused".to_string(),
        };
        let failure =
            convert_branch_exception(&Url::parse("https://anonscm.debian.org/repo").unwrap(), err);
        assert_eq!(failure.code, "hosted-on-alioth");
    }

    #[test]
    fn test_convert_branch_exception_unavailable_401() {
        let err = BranchOpenError::Unavailable {
            url: Url::parse("https://example.com/repo").unwrap(),
            description: "Unable to handle http code 401: Unauthorized".to_string(),
        };
        let failure =
            convert_branch_exception(&Url::parse("https://example.com/repo").unwrap(), err);
        assert_eq!(failure.code, "401-unauthorized");
    }

    #[test]
    fn test_convert_branch_exception_unsupported_svn() {
        let err = BranchOpenError::Unsupported {
            url: Url::parse("svn://example.com/repo").unwrap(),
            description: "Unsupported protocol for url svn://example.com/repo".to_string(),
            vcs: None,
        };
        let failure = convert_branch_exception(&Url::parse("svn://example.com/repo").unwrap(), err);
        assert_eq!(failure.code, "unsupported-vcs-svn");
    }

    #[test]
    fn test_convert_branch_exception_temporarily_unavailable() {
        let err = BranchOpenError::TemporarilyUnavailable {
            url: Url::parse("https://example.com/repo").unwrap(),
            description: "Server busy".to_string(),
        };
        let failure =
            convert_branch_exception(&Url::parse("https://example.com/repo").unwrap(), err);
        assert_eq!(failure.code, "branch-temporarily-unavailable");
    }

    #[test]
    fn test_convert_branch_exception_other() {
        let err = BranchOpenError::Other("something weird".to_string());
        let failure = convert_branch_exception(&Url::parse("https://example.com").unwrap(), err);
        assert_eq!(failure.code, "unknown");
        assert_eq!(failure.description, "something weird");
    }

    #[test]
    fn test_get_vcs_managers_simple_url() {
        let managers = get_vcs_managers("https://vcs.example.com/").unwrap();
        assert!(managers.contains_key(&VcsType::Git));
        assert!(managers.contains_key(&VcsType::Bzr));
    }

    #[test]
    fn test_get_vcs_managers_explicit() {
        let managers =
            get_vcs_managers("git=https://git.example.com/,bzr=https://bzr.example.com/").unwrap();
        assert!(managers.contains_key(&VcsType::Git));
        assert!(managers.contains_key(&VcsType::Bzr));
    }

    #[test]
    fn test_get_vcs_managers_git_only() {
        let managers = get_vcs_managers("git=https://git.example.com/").unwrap();
        assert!(managers.contains_key(&VcsType::Git));
        assert!(!managers.contains_key(&VcsType::Bzr));
    }

    #[test]
    fn test_branch_open_failure_display() {
        let failure = BranchOpenFailure {
            code: "branch-missing".to_string(),
            description: "Not found".to_string(),
            retry_after: None,
        };
        assert_eq!(
            failure.to_string(),
            "BranchOpenFailure(code=branch-missing, description=Not found)"
        );

        let failure_with_retry = BranchOpenFailure {
            code: "too-many-requests".to_string(),
            description: "Slow down".to_string(),
            retry_after: Some(chrono::Duration::seconds(60)),
        };
        assert_eq!(
            failure_with_retry.to_string(),
            "BranchOpenFailure(code=too-many-requests, description=Slow down, retry_after=PT60S)"
        );
    }

    #[test]
    fn test_remote_git_vcs_manager_diff_url() {
        let mgr = RemoteGitVcsManager::new(Url::parse("https://vcs.example.com/git/").unwrap());
        let old = RevisionId::from(b"git-v1:aaaa".to_vec());
        let new = RevisionId::from(b"git-v1:bbbb".to_vec());
        let url = mgr.get_diff_url(&CodebaseName::new("mycodebase").unwrap(), &old, &new);
        assert_eq!(
            url.as_str(),
            "https://vcs.example.com/git/mycodebase/diff?old=aaaa&new=bbbb"
        );
    }

    #[test]
    fn test_remote_bzr_vcs_manager_branch_url() {
        let mgr = RemoteBzrVcsManager::new(Url::parse("https://vcs.example.com/bzr/").unwrap());
        let url = mgr.get_branch_url(&CodebaseName::new("mycodebase").unwrap(), "main");
        assert_eq!(url.as_str(), "https://vcs.example.com/bzr/mycodebase/main");
    }

    #[test]
    fn test_remote_bzr_vcs_manager_diff_url() {
        let mgr = RemoteBzrVcsManager::new(Url::parse("https://vcs.example.com/bzr/").unwrap());
        let old = RevisionId::from(b"old-revid".to_vec());
        let new = RevisionId::from(b"new-revid".to_vec());
        let url = mgr.get_diff_url(&CodebaseName::new("mycodebase").unwrap(), &old, &new);
        assert_eq!(
            url.as_str(),
            "https://vcs.example.com/bzr/mycodebase/diff?old=old-revid&new=new-revid"
        );
    }

    #[test]
    fn test_remote_git_vcs_manager_repository_url() {
        let mgr = RemoteGitVcsManager::new(Url::parse("https://vcs.example.com/git/").unwrap());
        assert_eq!(
            mgr.get_repository_url(&CodebaseName::new("mycodebase").unwrap())
                .as_str(),
            "https://vcs.example.com/git/mycodebase"
        );
    }

    #[test]
    fn test_remote_bzr_vcs_manager_repository_url() {
        let mgr = RemoteBzrVcsManager::new(Url::parse("https://vcs.example.com/bzr/").unwrap());
        assert_eq!(
            mgr.get_repository_url(&CodebaseName::new("mycodebase").unwrap())
                .as_str(),
            "https://vcs.example.com/bzr/mycodebase"
        );
    }
}

#[derive(Debug)]
pub struct BranchOpenFailure {
    pub code: String,
    pub description: String,
    pub retry_after: Option<chrono::Duration>,
}

impl std::fmt::Display for BranchOpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        if let Some(retry_after) = self.retry_after {
            write!(
                f,
                "BranchOpenFailure(code={}, description={}, retry_after={})",
                self.code, self.description, retry_after
            )
        } else {
            write!(
                f,
                "BranchOpenFailure(code={}, description={})",
                self.code, self.description
            )
        }
    }
}

impl std::error::Error for BranchOpenFailure {}

pub fn open_branch_ext(
    vcs_url: &Url,
    possible_transports: Option<&mut Vec<breezyshim::transport::Transport>>,
    probers: Option<&[&dyn breezyshim::controldir::PyProber]>,
) -> Result<breezyshim::branch::GenericBranch, BranchOpenFailure> {
    match silver_platter::vcs::open_branch(vcs_url, possible_transports, probers, None) {
        Ok(branch) => Ok(branch),
        Err(e) => Err(convert_branch_exception(vcs_url, e)),
    }
}

/// Map breezy's "<VCS> branches are not yet supported" messages to a result code.
fn unsupported_vcs_code(description: &str) -> Option<&'static str> {
    [
        ("Subversion", "unsupported-vcs-svn"),
        ("Mercurial", "unsupported-vcs-hg"),
        ("Darcs", "unsupported-vcs-darcs"),
        ("Fossil", "unsupported-vcs-fossil"),
    ]
    .into_iter()
    .find(|(vcs, _)| description.contains(&format!("{} branches are not yet", vcs)))
    .map(|(_, code)| code)
}

fn convert_branch_exception(vcs_url: &Url, e: BranchOpenError) -> BranchOpenFailure {
    match e {
        BranchOpenError::RateLimited {
            retry_after,
            description,
            ..
        } => BranchOpenFailure {
            code: "too-many-requests".to_string(),
            description,
            retry_after: retry_after.map(|x| chrono::Duration::seconds(x as i64)),
        },
        BranchOpenError::Unavailable {
            ref description, ..
        } => {
            let code = if description.contains("http code 429: Too Many Requests") {
                "too-many-requests"
            } else if is_alioth_url(vcs_url) {
                "hosted-on-alioth"
            } else if description.contains("Unable to handle http code 401: Unauthorized")
                || description.contains("Unexpected HTTP status 401 for ")
            {
                "401-unauthorized"
            } else if description.contains("Unable to handle http code 502: Bad Gateway")
                || description.contains("Unexpected HTTP status 502 for ")
            {
                "502-bad-gateway"
            } else {
                unsupported_vcs_code(description).unwrap_or("branch-unavailable")
            };
            BranchOpenFailure {
                code: code.to_string(),
                description: description.to_string(),
                retry_after: None,
            }
        }
        BranchOpenError::TemporarilyUnavailable { url, description } => BranchOpenFailure {
            code: "branch-temporarily-unavailable".to_string(),
            description: format!("{} ({})", description, url),
            retry_after: None,
        },
        BranchOpenError::Missing {
            url,
            ref description,
            ..
        } => {
            let code = if description
                .starts_with("Branch does not exist: Not a branch: \"https://anonscm.debian.org")
            {
                "hosted-on-alioth"
            } else {
                "branch-missing"
            };
            BranchOpenFailure {
                code: code.to_string(),
                description: format!("{} ({})", description, url),
                retry_after: None,
            }
        }
        BranchOpenError::Unsupported { description, .. } => {
            let code = if description.contains("Unsupported protocol for url ") {
                if description.contains("anonscm.debian.org")
                    || description.contains("svn.debian.org")
                {
                    "hosted-on-alioth"
                } else if description.contains("svn://") {
                    "unsupported-vcs-svn"
                } else if description.contains("cvs+pserver://") {
                    "unsupported-vcs-cvs"
                } else {
                    "unsupported-vcs-protocol"
                }
            } else {
                unsupported_vcs_code(&description).unwrap_or("unsupported-vcs")
            };
            BranchOpenFailure {
                code: code.to_string(),
                description,
                retry_after: None,
            }
        }
        BranchOpenError::Other(description) => BranchOpenFailure {
            code: "unknown".to_string(),
            description,
            retry_after: None,
        },
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RevisionInfo {
    pub commit_id: Option<Vec<u8>>,
    pub revision_id: RevisionId,
    pub message: String,
    pub link: Option<Url>,
}

pub const EMPTY_GIT_TREE: &[u8] = b"4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[async_trait]
pub trait VcsManager: Send + Sync {
    fn get_branch(
        &self,
        codebase: &CodebaseName,
        branch_name: &str,
    ) -> Result<Option<breezyshim::branch::GenericBranch>, BranchOpenError>;

    /// Get the URL for the branch.
    fn get_branch_url(&self, codebase: &CodebaseName, branch_name: &str) -> Url;

    /// Get the repository for the codebase.
    fn get_repository(
        &self,
        codebase: &CodebaseName,
    ) -> Result<Option<GenericRepository>, BrzError> {
        let url = self.get_repository_url(codebase);
        match breezyshim::repository::open(&url) {
            Ok(repo) => Ok(Some(repo)),
            Err(BrzError::NotBranchError(..)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Get the URL for the repository.
    fn get_repository_url(&self, codebase: &CodebaseName) -> Url;

    /// List all repositories.
    fn list_repositories(&self) -> Vec<String>;

    /// Get the diff between two revisions.
    async fn get_diff(
        &self,
        codebase: &CodebaseName,
        old_revid: &RevisionId,
        new_revid: &RevisionId,
    ) -> Vec<u8>;

    async fn get_revision_info(
        &self,
        codebase: &CodebaseName,
        old_revid: &RevisionId,
        new_revid: &RevisionId,
    ) -> Vec<RevisionInfo>;
}

fn local_repository_url(base_path: &Path, codebase: &CodebaseName) -> Url {
    std::fs::create_dir_all(base_path).unwrap();
    let base_path = base_path.canonicalize().unwrap();
    Url::from_directory_path(base_path)
        .unwrap()
        .join(codebase)
        .unwrap()
}

fn list_local_repositories(base_path: &Path) -> Vec<String> {
    base_path
        .read_dir()
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect()
}

/// Open a branch in a local store, treating a missing or unavailable branch as absent.
fn open_local_branch(
    url: &Url,
    vcs: &str,
) -> Result<Option<breezyshim::branch::GenericBranch>, BranchOpenError> {
    let probers = silver_platter::probers::select_probers(Some(vcs));
    let probers = probers.iter().map(AsRef::as_ref).collect::<Vec<_>>();
    match silver_platter::vcs::open_branch(url, None, Some(probers.as_slice()), None) {
        Ok(branch) => Ok(Some(branch)),
        Err(BranchOpenError::Unavailable { .. }) | Err(BranchOpenError::Missing { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalGitVcsManager {
    base_path: PathBuf,
}

impl LocalGitVcsManager {
    pub fn new(base_path: PathBuf) -> Self {
        Self { base_path }
    }

    pub fn base_path(&self) -> &Path {
        &self.base_path
    }
}

#[async_trait]
impl VcsManager for LocalGitVcsManager {
    fn get_branch(
        &self,
        codebase: &CodebaseName,
        branch_name: &str,
    ) -> Result<Option<breezyshim::branch::GenericBranch>, BranchOpenError> {
        open_local_branch(&self.get_branch_url(codebase, branch_name), "git")
    }

    fn get_branch_url(&self, codebase: &CodebaseName, branch_name: &str) -> Url {
        let url = Url::from_directory_path(&self.base_path)
            .expect("LocalGitVcsManager base_path must be an absolute path");
        let url = url.join(codebase).expect(&format!(
            "Failed to join codebase '{}' to base URL",
            codebase
        ));
        let mut params = std::collections::HashMap::new();
        params.insert("branch".to_string(), branch_name.to_string());
        breezyshim::urlutils::join_segment_parameters(&url, params)
    }

    fn get_repository_url(&self, codebase: &CodebaseName) -> Url {
        local_repository_url(&self.base_path, codebase)
    }

    fn list_repositories(&self) -> Vec<String> {
        list_local_repositories(&self.base_path)
    }

    async fn get_diff(
        &self,
        codebase: &CodebaseName,
        old_revid: &RevisionId,
        new_revid: &RevisionId,
    ) -> Vec<u8> {
        if old_revid == new_revid {
            return vec![];
        }
        let repo = self.get_repository(codebase).unwrap().unwrap();
        let old_sha = if old_revid.is_null() {
            EMPTY_GIT_TREE.to_vec()
        } else {
            repo.lookup_bzr_revision_id(old_revid).unwrap().0
        };
        let new_sha = if new_revid.is_null() {
            EMPTY_GIT_TREE.to_vec()
        } else {
            repo.lookup_bzr_revision_id(new_revid).unwrap().0
        };
        let current_dir = repo.user_transport().local_abspath(Path::new(".")).unwrap();
        drop(repo); // Drop repo before await
        let output = tokio::process::Command::new("git")
            .arg("diff")
            .arg(std::str::from_utf8(&old_sha).unwrap())
            .arg(std::str::from_utf8(&new_sha).unwrap())
            .current_dir(current_dir)
            .output()
            .await
            .unwrap();
        if !output.status.success() {
            panic!(
                "git diff failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        output.stdout
    }

    async fn get_revision_info(
        &self,
        codebase: &CodebaseName,
        old_revid: &RevisionId,
        new_revid: &RevisionId,
    ) -> Vec<RevisionInfo> {
        let repo = self.get_repository(codebase).unwrap().unwrap();
        let old_sha = repo.lookup_bzr_revision_id(old_revid).unwrap().0;
        let new_sha = repo.lookup_bzr_revision_id(new_revid).unwrap().0;
        let old_sha_str = std::str::from_utf8(&old_sha).unwrap();
        let new_sha_str = std::str::from_utf8(&new_sha).unwrap();
        let repo_path = repo.user_transport().local_abspath(Path::new(".")).unwrap();

        // Use git log with a format that separates commit hash and message with a NUL byte
        let output = tokio::process::Command::new("git")
            .arg("log")
            .arg("--format=%H%x00%s")
            .arg(&format!("{}..{}", old_sha_str, new_sha_str))
            .current_dir(&repo_path)
            .output()
            .await
            .unwrap();

        if !output.status.success() {
            panic!(
                "git log failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                let (commit_hex, message) = line.split_once('\0').unwrap();
                let commit_id = commit_hex.as_bytes().to_vec();
                let revision_id = repo.lookup_foreign_revision_id(&commit_id).unwrap();
                RevisionInfo {
                    commit_id: Some(commit_id),
                    revision_id,
                    message: message.to_string(),
                    link: None,
                }
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalBzrVcsManager {
    base_path: PathBuf,
}

impl LocalBzrVcsManager {
    pub fn new(base_path: PathBuf) -> Self {
        Self { base_path }
    }

    pub fn base_path(&self) -> &Path {
        &self.base_path
    }
}

#[async_trait]
impl VcsManager for LocalBzrVcsManager {
    fn get_branch(
        &self,
        codebase: &CodebaseName,
        branch_name: &str,
    ) -> Result<Option<breezyshim::branch::GenericBranch>, BranchOpenError> {
        open_local_branch(&self.get_branch_url(codebase, branch_name), "bzr")
    }

    fn get_branch_url(&self, codebase: &CodebaseName, branch_name: &str) -> Url {
        let url = Url::from_directory_path(&self.base_path).unwrap();
        url.join(&format!("{}/", codebase))
            .unwrap()
            .join(branch_name)
            .unwrap()
    }

    fn get_repository_url(&self, codebase: &CodebaseName) -> Url {
        local_repository_url(&self.base_path, codebase)
    }

    fn list_repositories(&self) -> Vec<String> {
        list_local_repositories(&self.base_path)
    }

    async fn get_diff(
        &self,
        codebase: &CodebaseName,
        old_revid: &RevisionId,
        new_revid: &RevisionId,
    ) -> Vec<u8> {
        if old_revid == new_revid {
            return vec![];
        }
        let repo = self.get_repository(codebase).unwrap().unwrap();
        let output = tokio::process::Command::new("bzr")
            .arg("diff")
            .arg("-r")
            .arg(format!("{}..{}", old_revid, new_revid))
            .current_dir(repo.user_transport().local_abspath(Path::new(".")).unwrap())
            .output()
            .await
            .unwrap();
        if !output.status.success() {
            panic!(
                "bzr diff failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        output.stdout
    }

    async fn get_revision_info(
        &self,
        codebase: &CodebaseName,
        old_revid: &RevisionId,
        new_revid: &RevisionId,
    ) -> Vec<RevisionInfo> {
        let repo = self.get_repository(codebase).unwrap().unwrap();

        let lock = repo.lock_read();
        let mut ret = vec![];

        let graph = repo.get_graph();
        let revids = graph
            .iter_lefthand_ancestry(new_revid, Some(&[old_revid.clone()]))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for (_revid, rev) in repo.iter_revisions(revids) {
            if let Some(rev) = rev {
                ret.push(RevisionInfo {
                    revision_id: rev.revision_id,
                    link: None,
                    message: rev.message.to_string(),
                    commit_id: None,
                });
            }
        }

        std::mem::drop(lock);
        ret
    }
}

#[cfg(test)]
mod local_repository_url_tests {
    use super::*;

    #[test]
    fn get_repository_url_does_not_require_existing_directory() {
        let base = tempfile::tempdir().unwrap();
        let missing = base.path().join("not-created-yet");

        let git_manager = LocalGitVcsManager::new(missing.clone());
        let git_url = git_manager.get_repository_url(&CodebaseName::new("mycodebase").unwrap());
        assert!(git_url.as_str().ends_with("/not-created-yet/mycodebase"));

        let bzr_manager = LocalBzrVcsManager::new(missing.clone());
        let bzr_url = bzr_manager.get_repository_url(&CodebaseName::new("mycodebase").unwrap());
        assert!(bzr_url.as_str().ends_with("/not-created-yet/mycodebase"));
    }

    #[test]
    fn get_repository_url_accepts_relative_base_path() {
        let git_manager = LocalGitVcsManager::new(PathBuf::from("."));
        let git_url = git_manager.get_repository_url(&CodebaseName::new("mycodebase").unwrap());
        assert!(git_url.as_str().starts_with("file://"));
        assert!(git_url.as_str().ends_with("/mycodebase"));

        let bzr_manager = LocalBzrVcsManager::new(PathBuf::from("."));
        let bzr_url = bzr_manager.get_repository_url(&CodebaseName::new("mycodebase").unwrap());
        assert!(bzr_url.as_str().starts_with("file://"));
        assert!(bzr_url.as_str().ends_with("/mycodebase"));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteGitVcsManager {
    base_url: Url,
}

impl RemoteGitVcsManager {
    pub fn new(base_url: Url) -> Self {
        Self { base_url }
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub fn lookup_revid<'a>(revid: &'a RevisionId, default: &'a [u8]) -> &'a [u8] {
        if revid.is_null() {
            default
        } else {
            revid.as_bytes().strip_prefix(b"git-v1:").unwrap()
        }
    }

    pub fn get_diff_url(
        &self,
        codebase: &CodebaseName,
        old_revid: &RevisionId,
        new_revid: &RevisionId,
    ) -> Url {
        self.base_url
            .join(&format!(
                "{}/diff?old={}&new={}",
                codebase,
                std::str::from_utf8(RemoteGitVcsManager::lookup_revid(old_revid, EMPTY_GIT_TREE))
                    .unwrap(),
                std::str::from_utf8(RemoteGitVcsManager::lookup_revid(new_revid, EMPTY_GIT_TREE))
                    .unwrap()
            ))
            .unwrap()
    }
}

#[async_trait]
impl VcsManager for RemoteGitVcsManager {
    async fn get_diff(
        &self,
        codebase: &CodebaseName,
        old_revid: &RevisionId,
        new_revid: &RevisionId,
    ) -> Vec<u8> {
        if old_revid == new_revid {
            return vec![];
        }
        let url = self.get_diff_url(codebase, old_revid, new_revid);
        let client = reqwest::Client::new();
        let resp = client
            .get(url)
            .send()
            .await
            .expect("Git VCS manager should be able to fetch diff");
        resp.bytes()
            .await
            .expect("Git VCS manager should be able to read diff bytes")
            .to_vec()
    }

    async fn get_revision_info(
        &self,
        codebase: &CodebaseName,
        old_revid: &RevisionId,
        new_revid: &RevisionId,
    ) -> Vec<RevisionInfo> {
        let url = self
            .base_url
            .join(&format!(
                "{}/revision-info?old={}&new={}",
                codebase,
                std::str::from_utf8(RemoteGitVcsManager::lookup_revid(
                    old_revid,
                    breezyshim::git::ZERO_SHA
                ))
                .unwrap(),
                std::str::from_utf8(RemoteGitVcsManager::lookup_revid(
                    new_revid,
                    breezyshim::git::ZERO_SHA
                ))
                .unwrap()
            ))
            .unwrap();
        let client = reqwest::Client::new();
        let resp = client.get(url).send().await.unwrap();
        resp.json().await.unwrap()
    }

    fn get_branch_url(&self, codebase: &CodebaseName, branch_name: &str) -> Url {
        let url = self.base_url.join(codebase).unwrap();
        let params = std::collections::HashMap::from_iter(vec![(
            "branch".to_string(),
            branch_name.to_string(),
        )]);
        breezyshim::urlutils::join_segment_parameters(&url, params)
    }

    fn get_branch(
        &self,
        codebase: &CodebaseName,
        branch_name: &str,
    ) -> Result<Option<breezyshim::branch::GenericBranch>, BranchOpenError> {
        let url = self.get_branch_url(codebase, branch_name);
        // TODO: plumb tracing::Span::current() into `open_cached_branch`
        // as an `x-trace-span-id` header so upstream git-store logs can
        // be correlated with our spans.
        open_cached_branch(&url).map_err(|e| BranchOpenError::from_err(url, &e))
    }

    fn get_repository_url(&self, codebase: &CodebaseName) -> Url {
        self.base_url.join(codebase).unwrap()
    }

    fn list_repositories(&self) -> Vec<String> {
        // For remote Git VCS manager, try to query the repository list endpoint
        let list_url = match self.base_url.join("repositories") {
            Ok(url) => url,
            Err(_) => return Vec::new(),
        };

        // Use blocking HTTP request since this is a sync method
        if let Ok(rt) = tokio::runtime::Runtime::new() {
            let client = reqwest::Client::new();
            if let Ok(response) = rt.block_on(client.get(list_url).send()) {
                if response.status().is_success() {
                    if let Ok(repos) = rt.block_on(response.json::<Vec<String>>()) {
                        return repos;
                    }
                }
            }
        }

        // Return empty list if API call fails
        Vec::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteBzrVcsManager {
    base_url: Url,
}

impl RemoteBzrVcsManager {
    pub fn new(base_url: Url) -> Self {
        Self { base_url }
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub fn get_diff_url(
        &self,
        codebase: &CodebaseName,
        old_revid: &RevisionId,
        new_revid: &RevisionId,
    ) -> Url {
        self.base_url
            .join(&format!(
                "{}/diff?old={}&new={}",
                codebase, old_revid, new_revid
            ))
            .unwrap()
    }
}

#[async_trait]
impl VcsManager for RemoteBzrVcsManager {
    async fn get_diff(
        &self,
        codebase: &CodebaseName,
        old_revid: &RevisionId,
        new_revid: &RevisionId,
    ) -> Vec<u8> {
        if old_revid == new_revid {
            return vec![];
        }
        let url = self.get_diff_url(codebase, old_revid, new_revid);
        let client = reqwest::Client::new();

        // Add trace context headers if available from current tracing span
        let mut request = client.get(url);
        if let Some(trace_id) = tracing::Span::current().id() {
            request = request.header("x-trace-span-id", format!("{:?}", trace_id));
        }

        let resp = request.send().await.unwrap();
        resp.bytes().await.unwrap().to_vec()
    }

    async fn get_revision_info(
        &self,
        codebase: &CodebaseName,
        old_revid: &RevisionId,
        new_revid: &RevisionId,
    ) -> Vec<RevisionInfo> {
        let url = self
            .base_url
            .join(&format!(
                "{}/revision-info?old={}&new={}",
                codebase, old_revid, new_revid
            ))
            .unwrap();
        let client = reqwest::Client::new();

        // Add trace context headers if available from current tracing span
        let mut request = client.get(url);
        if let Some(trace_id) = tracing::Span::current().id() {
            request = request.header("x-trace-span-id", format!("{:?}", trace_id));
        }

        let resp = request.send().await.unwrap();
        resp.json().await.unwrap()
    }

    fn get_branch_url(&self, codebase: &CodebaseName, branch_name: &str) -> Url {
        self.base_url
            .join(&format!("{}/", codebase))
            .unwrap()
            .join(branch_name)
            .unwrap()
    }

    fn get_branch(
        &self,
        codebase: &CodebaseName,
        branch_name: &str,
    ) -> Result<Option<breezyshim::branch::GenericBranch>, BranchOpenError> {
        let url = self.get_branch_url(codebase, branch_name);
        // TODO: plumb tracing::Span::current() into `open_cached_branch`
        // as an `x-trace-span-id` header for cross-service correlation.
        open_cached_branch(&url).map_err(|e| BranchOpenError::from_err(url, &e))
    }

    fn get_repository_url(&self, codebase: &CodebaseName) -> Url {
        self.base_url.join(codebase).unwrap()
    }

    fn list_repositories(&self) -> Vec<String> {
        // For remote Bzr VCS manager, try to query the repository list endpoint
        let list_url = match self.base_url.join("repositories") {
            Ok(url) => url,
            Err(_) => return Vec::new(),
        };

        // Use blocking HTTP request since this is a sync method
        if let Ok(rt) = tokio::runtime::Runtime::new() {
            let client = reqwest::Client::new();

            // Add trace context headers if available from current tracing span
            let mut request = client.get(list_url);
            if let Some(trace_id) = tracing::Span::current().id() {
                request = request.header("x-trace-span-id", format!("{:?}", trace_id));
            }

            if let Ok(response) = rt.block_on(request.send()) {
                if response.status().is_success() {
                    if let Ok(repos) = rt.block_on(response.json::<Vec<String>>()) {
                        return repos;
                    }
                }
            }
        }

        // Return empty list if API call fails
        Vec::new()
    }
}

fn open_cached_branch(url: &Url) -> Result<Option<breezyshim::branch::GenericBranch>, BrzError> {
    fn convert_error(e: BrzError) -> Option<BrzError> {
        match e {
            BrzError::NotBranchError(..) => None,
            BrzError::RemoteGitError(..) => None,
            BrzError::InvalidHttpResponse(..) => None,
            BrzError::ConnectionError(e) => {
                log::info!("Unable to reach cache server: {}", e);
                None
            }
            BrzError::BranchReferenceLoop => None,
            e => Some(e),
        }
    }

    match breezyshim::transport::get_transport(url, None) {
        Ok(transport) => match breezyshim::branch::open_from_transport_as_generic(&transport) {
            Ok(branch) => Ok(Some(branch)),
            Err(e) => match convert_error(e) {
                Some(e) => Err(e),
                None => Ok(None),
            },
        },
        Err(e) => match convert_error(e) {
            Some(e) => Err(e),
            None => Ok(None),
        },
    }
}

pub fn get_vcs_managers(
    location: &str,
) -> Result<HashMap<VcsType, Box<dyn VcsManager>>, url::ParseError> {
    if !location.contains("=") {
        let base_url = Url::parse(location)?;
        // Trailing slash matters - Url::join() replaces the last segment
        // instead of appending, silently dropping "git"/"bzr" otherwise.
        let git_url = base_url.join("git/")?;
        let bzr_url = base_url.join("bzr/")?;

        Ok(vec![
            (
                VcsType::Git,
                Box::new(RemoteGitVcsManager::new(git_url)) as Box<dyn VcsManager>,
            ),
            (
                VcsType::Bzr,
                Box::new(RemoteBzrVcsManager::new(bzr_url)) as Box<dyn VcsManager>,
            ),
        ]
        .into_iter()
        .collect())
    } else {
        let mut ret: HashMap<VcsType, Box<dyn VcsManager>> = HashMap::new();
        for p in location.split(",") {
            match p.split_once("=") {
                Some(("git", v)) => {
                    let url = Url::parse(v)?;
                    ret.insert(VcsType::Git, Box::new(RemoteGitVcsManager::new(url)));
                }
                Some(("bzr", v)) => {
                    let url = Url::parse(v)?;
                    ret.insert(VcsType::Bzr, Box::new(RemoteBzrVcsManager::new(url)));
                }
                _ => return Err(url::ParseError::EmptyHost), // Better than panic
            }
        }
        Ok(ret)
    }
}

/// Parse a store location: the path for a `file:` URL, otherwise the URL itself.
fn local_path(location: &str) -> Result<Result<PathBuf, Url>, VcsConfigError> {
    let url = Url::parse(location)?;
    if url.scheme() == "file" {
        url.to_file_path()
            .map(Ok)
            .map_err(|_| VcsConfigError::InvalidFilePath)
    } else {
        Ok(Err(url))
    }
}

pub fn get_vcs_managers_from_config(
    config: &crate::config::Config,
) -> Result<HashMap<VcsType, Box<dyn VcsManager>>, VcsConfigError> {
    let mut ret: HashMap<VcsType, Box<dyn VcsManager>> = HashMap::new();

    if let Some(location) = config.git_location.as_ref() {
        let manager: Box<dyn VcsManager> = match local_path(location)? {
            Ok(path) => Box::new(LocalGitVcsManager::new(path)),
            Err(url) => Box::new(RemoteGitVcsManager::new(url)),
        };
        ret.insert(VcsType::Git, manager);
    }

    if let Some(location) = config.bzr_location.as_ref() {
        let manager: Box<dyn VcsManager> = match local_path(location)? {
            Ok(path) => Box::new(LocalBzrVcsManager::new(path)),
            Err(url) => Box::new(RemoteBzrVcsManager::new(url)),
        };
        ret.insert(VcsType::Bzr, manager);
    }

    Ok(ret)
}
