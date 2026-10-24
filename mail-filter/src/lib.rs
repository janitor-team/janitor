//! Mail Filter crate for the Janitor project.
//!
//! This crate provides functionality for parsing and filtering email messages,
//! particularly to extract merge proposal URLs from notification emails.

#![deny(missing_docs)]

use mailparse::{parse_mail, MailHeaderMap};
use select::document::Document;
use select::predicate::{And, Attr, Name};
use serde_json::Value;

/// Parse a plain text email body to extract a merge proposal URL.
///
/// # Arguments
/// * `text` - The plain text email body
///
/// # Returns
/// An Option containing the merge proposal URL if found
pub fn parse_plain_text_body(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        if line == &"Reply to this email directly or view it on GitHub:" {
            let url = lines.get(i + 1)?.split('#').next().unwrap();
            return Some(url.to_string());
        }
        if line == &"For more details, see:"
            && lines
                .get(i + 1)
                .is_some_and(|l| l.starts_with("https://code.launchpad.net/"))
        {
            return Some(lines[i + 1].to_string());
        }
        if let Some((field, value)) = line.split_once(':') {
            if field.eq_ignore_ascii_case("merge request url") {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// Parse JSON-LD data to extract a merge proposal URL.
///
/// # Arguments
/// * `ld` - The JSON-LD data
///
/// # Returns
/// An Option containing the merge proposal URL if found
fn parse_json_ld(ld: &Value) -> Option<String> {
    match ld {
        Value::Array(ld_array) => ld_array.iter().find_map(parse_json_ld),
        Value::Object(ld_object) => {
            let context = ld_object.get("@context")?;
            if context == &Value::String("https://schema.org".to_string())
                || context == &Value::String("http://schema.org".to_string())
            {
                let type_ = ld_object.get("@type")?;
                if type_ == &Value::String("EmailMessage".to_string()) {
                    let action = ld_object
                        .get("action")
                        .or_else(|| ld_object.get("potentialAction"))?;
                    let action_type = action.get("@type")?;
                    if action_type == &Value::String("ViewAction".to_string()) {
                        let url = action.get("url")?.as_str()?;
                        return Some(url.split('#').next().unwrap().to_string());
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Parse an HTML email body to extract a merge proposal URL.
///
/// # Arguments
/// * `contents` - The HTML email body
///
/// # Returns
/// An Option containing the merge proposal URL if found
pub fn parse_html_body(contents: &str) -> Option<String> {
    let document = Document::from(contents);
    let ld = document
        .find(And(Name("script"), Attr("type", "application/ld+json")))
        .next()?;
    if let Ok(ld_json) = serde_json::from_str(ld.text().as_str()) {
        parse_json_ld(&ld_json)
    } else {
        None
    }
}

/// Parse an email file to extract a merge proposal URL.
///
/// Returns `None` if the file cannot be read, the email cannot be parsed,
/// or no merge proposal URL is found in any text/html or text/plain part.
pub fn parse_email<F: std::io::Read>(mut file: F) -> Option<String> {
    let mut data = String::new();
    file.read_to_string(&mut data)
        .inspect_err(|e| log::error!("Failed to read email: {}", e))
        .ok()?;

    let mail = parse_mail(data.as_bytes())
        .inspect_err(|e| log::error!("Failed to parse email: {}", e))
        .ok()?;

    // Gitea names the pull request only in a header.
    if let Some(link) = mail.headers.get_first_value("X-Gitea-Issue-Link") {
        if link.contains("/pulls/") {
            return Some(link);
        }
    }

    for part in mail.subparts {
        let body = match part.get_body() {
            Ok(body) => body,
            Err(e) => {
                log::debug!("Failed to read part body: {}", e);
                continue;
            }
        };
        let parsed = match part.ctype.mimetype.as_str() {
            "text/html" => parse_html_body(&body),
            "text/plain" => parse_plain_text_body(&body),
            _ => continue,
        };
        if parsed.is_some() {
            return parsed;
        }
    }

    None
}

#[cfg(test)]
mod tests;
