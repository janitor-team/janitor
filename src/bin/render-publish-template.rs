#!/usr/bin/env rust-script
//! Render publish templates for merge proposals
use clap::Parser;
use janitor::config::Config;
use janitor::state::create_pool;
use log::info;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::PathBuf;
// This binary requires the tera feature to be enabled
#[cfg(not(feature = "tera"))]
compile_error!("This binary requires the 'tera' feature to be enabled");

use tera::{Context as TeraContext, Tera};
use tokio::fs;

#[derive(Debug, sqlx::FromRow)]
struct RunInfo {
    package: Option<String>,
    campaign: Option<String>,
    log_id: String,
    result: Option<String>,
}

#[derive(Parser, Debug)]
#[command(author, version, about = "Render publish templates for runs")]
struct Args {
    /// Path to configuration file
    #[arg(long, default_value = "janitor.conf")]
    config: String,

    /// Run ID to process
    #[arg(short = 'r', long)]
    run_id: String,

    /// Role (default: main)
    #[arg(long, default_value = "main")]
    role: String,

    /// Output format
    #[arg(long, value_enum, default_value = "md")]
    format: Format,

    /// Path to template environment directory
    #[arg(long)]
    template_env_path: Option<PathBuf>,

    /// Log level (error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum Format {
    #[value(name = "md")]
    Markdown,
    #[value(name = "txt")]
    Text,
}

impl Format {
    fn extension(&self) -> &'static str {
        match self {
            Format::Markdown => "md",
            Format::Text => "txt",
        }
    }
}

/// Check if a debdiff is empty
fn debdiff_is_empty(value: &Value, _args: &HashMap<String, Value>) -> tera::Result<Value> {
    if let Some(debdiff) = value.as_str() {
        // A debdiff is considered empty if it only contains header lines
        let lines: Vec<&str> = debdiff.lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        
        // Check if all lines are headers or metadata
        let is_empty = lines.iter().all(|line| {
            line.starts_with("diff ") ||
            line.starts_with("Binary files ") ||
            line.starts_with("Files ") ||
            line.starts_with("---") ||
            line.starts_with("+++") ||
            line.starts_with("@@")
        });
        
        Ok(Value::Bool(is_empty))
    } else {
        Ok(Value::Bool(true))
    }
}

/// Convert debdiff to markdown format
fn markdownify_debdiff(value: &Value, _args: &HashMap<String, Value>) -> tera::Result<Value> {
    if let Some(debdiff) = value.as_str() {
        let mut markdown = String::new();
        let mut in_diff_block = false;
        
        for line in debdiff.lines() {
            if line.starts_with("diff ") || line.starts_with("Binary files ") {
                if in_diff_block {
                    markdown.push_str("```\n");
                    in_diff_block = false;
                }
                markdown.push_str(&format!("\n### {}\n", line));
            } else if line.starts_with("@@") {
                if !in_diff_block {
                    markdown.push_str("\n```diff\n");
                    in_diff_block = true;
                }
                markdown.push_str(&format!("{}\n", line));
            } else if line.starts_with("+") && !line.starts_with("+++") {
                markdown.push_str(&format!("{}\n", line));
            } else if line.starts_with("-") && !line.starts_with("---") {
                markdown.push_str(&format!("{}\n", line));
            } else {
                markdown.push_str(&format!("{}\n", line));
            }
        }
        
        if in_diff_block {
            markdown.push_str("```\n");
        }
        
        Ok(Value::String(markdown))
    } else {
        Ok(Value::String(String::new()))
    }
}

/// Load template environment
fn load_template_env(template_path: Option<&PathBuf>) -> Result<Tera, Box<dyn std::error::Error>> {
    let mut tera = if let Some(path) = template_path {
        Tera::new(path.join("**/*").to_str().ok_or("Invalid template path")?)?
    } else {
        // Default templates directory
        Tera::new("templates/**/*")?
    };
    
    // Register custom filters
    tera.register_filter("debdiff_is_empty", debdiff_is_empty);
    tera.register_filter("markdownify_debdiff", markdownify_debdiff);
    
    Ok(tera)
}

/// Process a build and render its template
async fn process_build(
    pool: &PgPool,
    run_id: &str,
    role: &str,
    format: Format,
    template_env: &Tera,
) -> Result<(), Box<dyn std::error::Error>> {
    // Query run information with package details
    let row: RunInfo = sqlx::query_as(
        r#"
        SELECT
          package.name AS package,
          run.campaign,
          run.id AS log_id,
          run.result AS result
        FROM run
        LEFT JOIN package ON run.codebase = package.codebase
        WHERE
          run.id = $1
        "#
    )
    .bind(run_id)
    .fetch_one(pool)
    .await
    .map_err(|e| format!("Failed to fetch run information: {}", e))?;
    
    // Build template context
    let mut context = TeraContext::new();
    
    // Add basic fields
    context.insert("package", &row.package);
    context.insert("campaign", &row.campaign);
    context.insert("suite", &row.campaign); // suite is an alias for campaign
    context.insert("log_id", &row.log_id);
    context.insert("external_url", "https://janitor.debian.net/");
    
    // Add result fields if present
    if let Some(result) = &row.result {
        // Merge result JSON into context
        if let Some(obj) = result.as_object() {
            for (key, value) in obj {
                context.insert(key, value);
            }
        }
    }
    
    // Render template
    let template_name = format!("{}.{}", row.campaign, format.extension());
    
    match template_env.render(&template_name, &context) {
        Ok(rendered) => {
            println!("{}", rendered);
        }
        Err(e) => {
            // If template not found, try a generic template
            let generic_template = format!("generic.{}", format.extension());
            match template_env.render(&generic_template, &context) {
                Ok(rendered) => {
                    println!("{}", rendered);
                }
                Err(_) => {
                    return Err(format!(
                        "Template not found: {} (also tried {})",
                        template_name,
                        generic_template
                    ).into());
                }
            }
        }
    }
    
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    
    // Initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&args.log_level))
        .format_timestamp(None)
        .init();
    
    // Load configuration
    let config_content = fs::read_to_string(&args.config).await
        .map_err(|e| format!("Failed to read config file: {}: {}", args.config, e))?;
    let config: Config = toml::from_str(&config_content)
        .map_err(|e| format!("Failed to parse configuration: {}", e))?;
    
    // Create database pool
    let pool = create_pool(&config).await?;
    
    // Load template environment
    let template_env = load_template_env(args.template_env_path.as_ref())?;
    
    // Process the build
    process_build(&pool, &args.run_id, &args.role, args.format, &template_env).await?;
    
    Ok(())
}