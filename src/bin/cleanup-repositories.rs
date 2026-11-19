#!/usr/bin/env rust-script
//! Cleanup owned repositories that are no longer needed for merge proposals.
//!
//! This is necessary in particular because some hosting sites
//! (e.g. default GitLab) have restrictions on the number of repositories
//! that a single user can own (in the case of GitLab, 1000).

use clap::Parser;
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Parser, Debug)]
#[command(author, version, about = "Cleanup owned repositories that are no longer needed")]
struct Args {
    /// Perform a dry run without actually deleting anything
    #[arg(long)]
    dry_run: bool,

    /// Log level (error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    log_level: String,
}

/// Represents a forge instance (GitHub, GitLab, etc.)
#[derive(Debug)]
struct ForgeInstance {
    name: String,
    api_client: Box<dyn ForgeClient>,
}

/// Trait for forge-specific operations
trait ForgeClient: Send + Sync + std::fmt::Debug {
    /// Get all merge proposals owned by the authenticated user
    fn iter_my_proposals(&self) -> Result<Vec<MergeProposal>, Box<dyn std::error::Error>>;
    
    /// Get all forks owned by the authenticated user
    fn iter_my_forks(&self) -> Result<Vec<String>, Box<dyn std::error::Error>>;
    
    /// Delete a project/repository
    fn delete_project(&self, project: &str) -> Result<(), Box<dyn std::error::Error>>;
}

#[derive(Debug, Clone)]
struct MergeProposal {
    source_project: String,
    is_closed: bool,
    is_merged: bool,
}

/// GitHub API client implementation
#[derive(Debug)]
struct GitHubClient {
    client: reqwest::blocking::Client,
    token: String,
    owner: String,
}

impl GitHubClient {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let token = std::env::var("GITHUB_TOKEN")
            .map_err(|_| "GITHUB_TOKEN environment variable not set")?;
        
        let client = reqwest::blocking::Client::builder()
            .user_agent("janitor-cleanup-repositories/0.1.0")
            .build()?;
        
        // Get the authenticated user
        let user: serde_json::Value = client
            .get("https://api.github.com/user")
            .bearer_auth(&token)
            .send()?
            .error_for_status()?
            .json()?;
        
        let owner = user["login"]
            .as_str()
            .ok_or("Failed to get GitHub username")?
            .to_string();
        
        Ok(Self { client, token, owner })
    }
}

impl ForgeClient for GitHubClient {
    fn iter_my_proposals(&self) -> Result<Vec<MergeProposal>, Box<dyn std::error::Error>> {
        let mut proposals = Vec::new();
        let mut page = 1;
        
        loop {
            let response: Vec<serde_json::Value> = self.client
                .get("https://api.github.com/user/issues")
                .bearer_auth(&self.token)
                .query(&[
                    ("filter", "created"),
                    ("state", "all"),
                    ("per_page", "100"),
                    ("page", &page.to_string()),
                ])
                .send()?
                .error_for_status()?
                .json()?;
            
            if response.is_empty() {
                break;
            }
            
            for issue in response {
                if let Some(pr_url) = issue["pull_request"]["url"].as_str() {
                    let pr: serde_json::Value = self.client
                        .get(pr_url)
                        .bearer_auth(&self.token)
                        .send()?
                        .error_for_status()?
                        .json()?;
                    
                    if let Some(head_repo) = pr["head"]["repo"]["full_name"].as_str() {
                        proposals.push(MergeProposal {
                            source_project: head_repo.to_string(),
                            is_closed: pr["state"].as_str() == Some("closed"),
                            is_merged: pr["merged"].as_bool().unwrap_or(false),
                        });
                    }
                }
            }
            
            page += 1;
        }
        
        Ok(proposals)
    }
    
    fn iter_my_forks(&self) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let mut forks = Vec::new();
        let mut page = 1;
        
        loop {
            let response: Vec<serde_json::Value> = self.client
                .get(&format!("https://api.github.com/users/{}/repos", self.owner))
                .bearer_auth(&self.token)
                .query(&[
                    ("type", "owner"),
                    ("per_page", "100"),
                    ("page", &page.to_string()),
                ])
                .send()?
                .error_for_status()?
                .json()?;
            
            if response.is_empty() {
                break;
            }
            
            for repo in response {
                if repo["fork"].as_bool() == Some(true) {
                    if let Some(full_name) = repo["full_name"].as_str() {
                        forks.push(full_name.to_string());
                    }
                }
            }
            
            page += 1;
        }
        
        Ok(forks)
    }
    
    fn delete_project(&self, project: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.client
            .delete(&format!("https://api.github.com/repos/{}", project))
            .bearer_auth(&self.token)
            .send()?
            .error_for_status()?;
        Ok(())
    }
}

/// GitLab API client implementation
#[derive(Debug)]
struct GitLabClient {
    client: reqwest::blocking::Client,
    token: String,
    base_url: String,
}

impl GitLabClient {
    fn new(base_url: Option<String>) -> Result<Self, Box<dyn std::error::Error>> {
        let token = std::env::var("GITLAB_TOKEN")
            .map_err(|_| "GITLAB_TOKEN environment variable not set")?;
        
        let base_url = base_url.unwrap_or_else(|| "https://gitlab.com".to_string());
        
        let client = reqwest::blocking::Client::builder()
            .user_agent("janitor-cleanup-repositories/0.1.0")
            .build()?;
        
        Ok(Self { client, token, base_url })
    }
}

impl ForgeClient for GitLabClient {
    fn iter_my_proposals(&self) -> Result<Vec<MergeProposal>, Box<dyn std::error::Error>> {
        let mut proposals = Vec::new();
        let mut page = 1;
        
        loop {
            let response: Vec<serde_json::Value> = self.client
                .get(&format!("{}/api/v4/merge_requests", self.base_url))
                .header("PRIVATE-TOKEN", &self.token)
                .query(&[
                    ("scope", "created_by_me"),
                    ("state", "all"),
                    ("per_page", "100"),
                    ("page", &page.to_string()),
                ])
                .send()?
                .error_for_status()?
                .json()?;
            
            if response.is_empty() {
                break;
            }
            
            for mr in response {
                if let Some(source_project_id) = mr["source_project_id"].as_i64() {
                    // Get project details
                    let project: serde_json::Value = self.client
                        .get(&format!("{}/api/v4/projects/{}", self.base_url, source_project_id))
                        .header("PRIVATE-TOKEN", &self.token)
                        .send()?
                        .error_for_status()?
                        .json()?;
                    
                    if let Some(path_with_namespace) = project["path_with_namespace"].as_str() {
                        proposals.push(MergeProposal {
                            source_project: path_with_namespace.to_string(),
                            is_closed: mr["state"].as_str() == Some("closed"),
                            is_merged: mr["state"].as_str() == Some("merged"),
                        });
                    }
                }
            }
            
            page += 1;
        }
        
        Ok(proposals)
    }
    
    fn iter_my_forks(&self) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let mut forks = Vec::new();
        let mut page = 1;
        
        loop {
            let response: Vec<serde_json::Value> = self.client
                .get(&format!("{}/api/v4/projects", self.base_url))
                .header("PRIVATE-TOKEN", &self.token)
                .query(&[
                    ("owned", "true"),
                    ("per_page", "100"),
                    ("page", &page.to_string()),
                ])
                .send()?
                .error_for_status()?
                .json()?;
            
            if response.is_empty() {
                break;
            }
            
            for project in response {
                if let Some(forked_from) = project.get("forked_from_project") {
                    if !forked_from.is_null() {
                        if let Some(path_with_namespace) = project["path_with_namespace"].as_str() {
                            forks.push(path_with_namespace.to_string());
                        }
                    }
                }
            }
            
            page += 1;
        }
        
        Ok(forks)
    }
    
    fn delete_project(&self, project: &str) -> Result<(), Box<dyn std::error::Error>> {
        // URL encode the project path
        let encoded_project = urlencoding::encode(project);
        
        self.client
            .delete(&format!("{}/api/v4/projects/{}", self.base_url, encoded_project))
            .header("PRIVATE-TOKEN", &self.token)
            .send()?
            .error_for_status()?;
        Ok(())
    }
}

/// Determine which projects can be safely removed
fn projects_to_remove(forge: &ForgeInstance) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let proposals = forge.api_client.iter_my_proposals()?;
    let forks = forge.api_client.iter_my_forks()?;
    
    // Collect projects that are still in use (have active merge proposals)
    let in_use: HashSet<String> = proposals
        .iter()
        .filter(|mp| !mp.is_closed && !mp.is_merged)
        .map(|mp| mp.source_project.clone())
        .collect();
    
    // Return forks that are not in use
    Ok(forks
        .into_iter()
        .filter(|project| !in_use.contains(project))
        .collect())
}

/// Get all available forge instances
fn get_forge_instances() -> Vec<ForgeInstance> {
    let mut instances = Vec::new();
    
    // Check for GitHub
    if std::env::var("GITHUB_TOKEN").is_ok() {
        match GitHubClient::new() {
            Ok(client) => {
                instances.push(ForgeInstance {
                    name: "GitHub".to_string(),
                    api_client: Box::new(client),
                });
            }
            Err(e) => warn!("Failed to initialize GitHub client: {}", e),
        }
    }
    
    // Check for GitLab
    if std::env::var("GITLAB_TOKEN").is_ok() {
        match GitLabClient::new(None) {
            Ok(client) => {
                instances.push(ForgeInstance {
                    name: "GitLab".to_string(),
                    api_client: Box::new(client),
                });
            }
            Err(e) => warn!("Failed to initialize GitLab client: {}", e),
        }
    }
    
    // Could add more forge types here (Gitea, Gogs, etc.)
    
    instances
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&args.log_level))
        .format_timestamp(None)
        .init();
    
    let instances = get_forge_instances();
    
    if instances.is_empty() {
        warn!("No forge instances configured. Set GITHUB_TOKEN or GITLAB_TOKEN environment variables.");
        return Ok(());
    }
    
    for instance in instances {
        info!("Processing {}", instance.name);
        
        match projects_to_remove(&instance) {
            Ok(projects) => {
                for project in projects {
                    info!("Deleting {} from {}", project, instance.name);
                    
                    if !args.dry_run {
                        if let Err(e) = instance.api_client.delete_project(&project) {
                            warn!("Failed to delete {}: {}", project, e);
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Failed to get projects from {}: {}", instance.name, e);
            }
        }
    }
    
    Ok(())
}