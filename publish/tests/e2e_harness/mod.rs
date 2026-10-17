//! Shared end-to-end harness for the publish path.
//!
//! Mount it from a test file with `mod e2e_harness;`.
//!
//! `GitFixture` builds real git repositories on disk and the request to
//! publish between them, so `publish_one` runs against real branches with no
//! forge. `GitDaemon` serves those repositories over git://, which is how a
//! remote source branch is opened. `setup()` builds a real `AppState` and
//! router over a throwaway database and skips cleanly when there is no
//! Postgres.
#![allow(dead_code)]

use axum::Router;
use janitor::test_utils::{TestDatabase, TestDatabaseConfig};
use janitor::vcs::{LocalGitVcsManager, RemoteGitVcsManager, VcsManager, VcsType};
use janitor_publish::{AppState, PublishError, PublishOneRequest, PublishWorker};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, RwLock};

pub const CAMPAIGN: &str = "lintian-fixes";
pub const ROLE: &str = "main";

/// Real git repositories under one temporary directory.
pub struct GitFixture {
    dir: tempfile::TempDir,
}

impl GitFixture {
    pub fn new() -> Self {
        breezyshim::init();
        Self {
            dir: tempfile::tempdir().expect("temporary directory"),
        }
    }

    pub fn base_path(&self) -> &Path {
        self.dir.path()
    }

    pub fn repo_path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Run git in `repo` and return its trimmed stdout.
    pub fn git(&self, repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.com",
            ])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .expect("git is runnable");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("git output is utf-8")
            .trim()
            .to_string()
    }

    /// Create a repository `name` whose `main` has one commit.
    pub fn init_repo(&self, name: &str) -> PathBuf {
        let repo = self.repo_path(name);
        std::fs::create_dir_all(&repo).expect("repository directory");
        self.git(&repo, &["init", "-q", "-b", "main"]);
        self.commit_file(&repo, "base.txt", "base\n");
        repo
    }

    /// Commit a file on the current branch of `repo` and return the commit id.
    pub fn commit_file(&self, repo: &Path, file: &str, contents: &str) -> String {
        std::fs::write(repo.join(file), contents).expect("write file");
        self.git(repo, &["add", file]);
        self.git(repo, &["commit", "-q", "-m", file]);
        self.git(repo, &["rev-parse", "HEAD"])
    }

    /// Add `branch` to `repo` with one commit beyond `main`, leave `main`
    /// checked out, and return the new tip.
    pub fn add_branch(&self, repo: &Path, branch: &str) -> String {
        self.git(repo, &["checkout", "-q", "-b", branch]);
        let tip = self.commit_file(repo, "change.txt", "change\n");
        self.git(repo, &["checkout", "-q", "main"]);
        tip
    }

    /// Create `name` as a bare clone of the repository `from`.
    pub fn bare_clone(&self, from: &Path, name: &str) -> PathBuf {
        let dest = self.repo_path(name);
        self.git(
            self.base_path(),
            &[
                "clone",
                "-q",
                "--bare",
                from.to_str().expect("utf-8 path"),
                dest.to_str().expect("utf-8 path"),
            ],
        );
        dest
    }

    pub fn tip(&self, repo: &Path, rev: &str) -> String {
        self.git(repo, &["rev-parse", rev])
    }

    pub fn vcs_manager(&self) -> LocalGitVcsManager {
        LocalGitVcsManager::new(self.dir.path().to_path_buf())
    }

    /// The URL the publisher gives `publish_one` for the source branch of
    /// `codebase`, built by the same `get_branch_url` call it uses.
    pub fn source_branch_url(&self, codebase: &str) -> url::Url {
        self.vcs_manager()
            .get_branch_url(codebase, &format!("{}/{}", CAMPAIGN, ROLE))
    }

    pub fn url_of(&self, repo: &Path) -> url::Url {
        url::Url::from_directory_path(repo).expect("absolute path")
    }

    /// A request to publish `revision` (a git commit id) from
    /// `source_branch_url` to `target_branch_url` in `mode`.
    pub fn request(
        &self,
        source_branch_url: url::Url,
        target_branch_url: url::Url,
        revision: &str,
        mode: janitor::publish::Mode,
    ) -> PublishOneRequest {
        PublishOneRequest {
            campaign: CAMPAIGN.to_string(),
            target_branch_url,
            role: ROLE.to_string(),
            log_id: "run-1".to_string(),
            reviewers: None,
            revision_id: format!("git-v1:{}", revision).into_bytes().into(),
            unchanged_id: None,
            require_binary_diff: false,
            differ_url: url::Url::parse("http://differ.invalid").unwrap(),
            derived_branch_name: format!("{}/{}", CAMPAIGN, ROLE),
            tags: None,
            allow_create_proposal: false,
            source_branch_url,
            codemod_result: serde_json::json!({}),
            commit_message_template: None,
            title_template: None,
            existing_mp_url: None,
            extra_context: None,
            mode,
            command: "true".to_string(),
            external_url: None,
            derived_owner: None,
            auto_merge: None,
        }
    }
}

/// `git daemon` serving the repositories under a fixture, killed on drop.
pub struct GitDaemon {
    child: std::process::Child,
    port: u16,
}

impl GitDaemon {
    pub fn start(fixture: &GitFixture) -> Self {
        for _ in 0..5 {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("free port")
                .local_addr()
                .expect("local address")
                .port();
            let child = Command::new("git")
                .arg("daemon")
                .arg(format!("--base-path={}", fixture.base_path().display()))
                .args(["--export-all", "--reuseaddr", "--listen=127.0.0.1"])
                .arg(format!("--port={}", port))
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("git daemon is runnable");
            let mut daemon = Self { child, port };
            for _ in 0..100 {
                if daemon.child.try_wait().expect("daemon status").is_some() {
                    break;
                }
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    if daemon.child.try_wait().expect("daemon status").is_none() {
                        return daemon;
                    }
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        panic!("git daemon did not start");
    }

    pub fn url_of(&self, name: &str) -> url::Url {
        self.base_url().join(name).unwrap()
    }

    pub fn base_url(&self) -> url::Url {
        url::Url::parse(&format!("git://127.0.0.1:{}/", self.port)).unwrap()
    }

    /// The URL the publisher gives `publish_one` for the source branch of
    /// `codebase`, built by the same `get_branch_url` call it uses.
    pub fn source_branch_url(&self, codebase: &str) -> url::Url {
        RemoteGitVcsManager::new(self.base_url())
            .get_branch_url(codebase, &format!("{}/{}", CAMPAIGN, ROLE))
    }
}

impl Drop for GitDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Call the real `publish_one`.
pub fn run_publish_one(
    request: &PublishOneRequest,
) -> Result<(janitor_publish::publish_one::PublishOneResult, String), PublishError> {
    janitor_publish::publish_one::publish_one(minijinja::Environment::new(), request, &mut None)
}

/// A real `AppState` and router over a throwaway database.
pub struct Harness {
    pub router: Router,
    pub state: Arc<AppState>,
    pub git: GitFixture,
    _db: TestDatabase,
}

/// Build the harness, or return `None` when there is no Postgres.
pub async fn setup() -> Option<Harness> {
    let db = match TestDatabase::with_config(TestDatabaseConfig::default()).await {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Skipping: no Postgres available: {}", e);
            return None;
        }
    };
    if let Err(e) = janitor::schema::setup_test_database(db.pool()).await {
        eprintln!("Skipping: schema setup failed: {}", e);
        return None;
    }
    let git = GitFixture::new();
    let config: &'static janitor::config::Config = Box::leak(Box::new(
        janitor::config::read_string("").expect("empty config parses"),
    ));
    let publish_worker = PublishWorker::new(
        None,
        None,
        url::Url::parse("http://differ.invalid").unwrap(),
        None,
        None,
        None,
    )
    .await;
    let health_checker = Arc::new(janitor_publish::health::BasicHealthChecker::with_info(
        "publish-test".to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    ));
    let mut vcs_managers: HashMap<VcsType, Box<dyn VcsManager>> = HashMap::new();
    vcs_managers.insert(VcsType::Git, Box::new(git.vcs_manager()));
    let state = Arc::new(AppState {
        conn: db.pool().clone(),
        bucket_rate_limiter: Arc::new(Mutex::new(Box::new(
            janitor_publish::rate_limiter::NonRateLimiter,
        ))),
        forge_rate_limiter: Arc::new(RwLock::new(HashMap::new())),
        push_limit: None,
        redis: None,
        redis_manager: None,
        config,
        publish_worker,
        vcs_managers: Arc::new(vcs_managers),
        modify_mp_limit: None,
        unexpected_mp_limit: None,
        gpg: Arc::new(breezyshim::gpg::GPGContext::new()),
        require_binary_diff: false,
        health_checker,
        last_full_scan_at: tokio::sync::watch::channel(None).0,
    });
    Some(Harness {
        router: janitor_publish::web::app(state.clone()),
        state,
        git,
        _db: db,
    })
}
