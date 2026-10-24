use super::*;

// Tests for existing data files
#[test]
fn test_parse_github_merged_email() {
    let email = include_bytes!("../tests/data/github-merged-email.txt");

    assert_eq!(
        Some("https://github.com/UbuntuBudgie/budgie-desktop/pull/78"),
        parse_email(std::io::Cursor::new(email)).as_deref()
    );
}

#[test]
fn test_parse_gitlab_merged_email() {
    let email = include_bytes!("../tests/data/gitlab-merged-email.txt");

    assert_eq!(
        Some("https://salsa.debian.org/debian/pkg-lojban-common/-/merge_requests/2"),
        parse_email(std::io::Cursor::new(email)).as_deref()
    );
}

#[test]
fn test_parse_single_part_launchpad_email() {
    let email = b"From: Example <noreply@launchpad.net>\r\n\
Subject: [Merge] ~user/project/trunk into project/trunk\r\n\
Content-Type: text/plain; charset=\"utf-8\"\r\n\
\r\n\
The proposal to merge ~user/project/trunk into project/trunk has been updated.\r\n\
For more details, see:\r\n\
https://code.launchpad.net/~user/project/trunk/+merge/123456\r\n";

    assert_eq!(
        Some("https://code.launchpad.net/~user/project/trunk/+merge/123456"),
        parse_email(std::io::Cursor::new(email)).as_deref()
    );
}

#[test]
fn test_parse_nested_multipart_email() {
    let email = b"From: Example <noreply@github.com>\r\n\
Subject: Merged\r\n\
Content-Type: multipart/mixed; boundary=\"outer\"\r\n\
\r\n\
--outer\r\n\
Content-Type: multipart/alternative; boundary=\"inner\"\r\n\
\r\n\
--inner\r\n\
Content-Type: text/plain; charset=\"utf-8\"\r\n\
\r\n\
Merged #1 into main.\r\n\
Reply to this email directly or view it on GitHub:\r\n\
https://github.com/owner/repo/pull/1#event-1\r\n\
--inner--\r\n\
--outer--\r\n";

    assert_eq!(
        Some("https://github.com/owner/repo/pull/1"),
        parse_email(std::io::Cursor::new(email)).as_deref()
    );
}

// Unit tests for parse_plain_text_body
#[test]
fn test_parse_plain_text_body_github() {
    let text = r#"Some header text
Reply to this email directly or view it on GitHub:
https://github.com/user/repo/pull/123#issuecomment-456
Footer text"#;

    let result = parse_plain_text_body(text);
    assert_eq!(
        result,
        Some("https://github.com/user/repo/pull/123".to_string())
    );
}

#[test]
fn test_parse_plain_text_body_launchpad() {
    let text = r#"Subject: Test
For more details, see:
https://code.launchpad.net/~user/project/+merge/123456
Thanks"#;

    let result = parse_plain_text_body(text);
    assert_eq!(
        result,
        Some("https://code.launchpad.net/~user/project/+merge/123456".to_string())
    );
}

#[test]
fn test_parse_plain_text_body_gitlab() {
    let text = r#"Hello,
Merge Request Url: https://gitlab.com/user/project/-/merge_requests/789
Best regards"#;

    let result = parse_plain_text_body(text);
    assert_eq!(
        result,
        Some("https://gitlab.com/user/project/-/merge_requests/789".to_string())
    );
}

#[test]
fn test_parse_plain_text_body_gitlab_case_insensitive() {
    let text = r#"Hello,
merge request URL: https://gitlab.com/user/project/-/merge_requests/789
Best regards"#;

    let result = parse_plain_text_body(text);
    assert_eq!(
        result,
        Some("https://gitlab.com/user/project/-/merge_requests/789".to_string())
    );
}

#[test]
fn test_parse_plain_text_body_no_url() {
    let text = r#"This is a regular email
Without any merge request URLs
Just normal text"#;

    let result = parse_plain_text_body(text);
    assert_eq!(result, None);
}

#[test]
fn test_parse_plain_text_body_empty() {
    let result = parse_plain_text_body("");
    assert_eq!(result, None);
}

// Unit tests for parse_html_body
#[test]
fn test_parse_html_body_github() {
    let html = r#"<html>
<head>
<script type="application/ld+json">
{
    "@context": "http://schema.org",
    "@type": "EmailMessage",
    "potentialAction": {
        "@type": "ViewAction",
        "url": "https://github.com/user/repo/pull/123",
        "name": "View Pull Request"
    }
}
</script>
</head>
<body>Pull request notification</body>
</html>"#;

    let result = parse_html_body(html);
    assert_eq!(
        result,
        Some("https://github.com/user/repo/pull/123".to_string())
    );
}

#[test]
fn test_parse_html_body_no_script() {
    let html = r#"<html>
<body>
<p>Just a regular HTML email</p>
</body>
</html>"#;

    let result = parse_html_body(html);
    assert_eq!(result, None);
}

#[test]
fn test_parse_html_body_invalid_json() {
    let html = r#"<html>
<head>
<script type="application/ld+json">
{ invalid json
</script>
</head>
<body>test</body>
</html>"#;

    let result = parse_html_body(html);
    assert_eq!(result, None);
}

#[test]
fn test_parse_html_json_ld_array() {
    let html = r#"<html>
    <head>
        <script type="application/ld+json">
        [{
            "@context": "https://schema.org",
            "@type": "EmailMessage",
            "potentialAction": {
                "@type": "ViewAction",
                "url": "https://github.com/owner/repo/pull/7"
            }
        }]
        </script>
    </head>
    <body>Email body</body>
    </html>"#;
    assert_eq!(
        parse_html_body(html),
        Some("https://github.com/owner/repo/pull/7".to_string())
    );
}

#[test]
fn test_parse_html_wrong_type() {
    let html = r#"<html>
    <head>
        <script type="application/ld+json">
        {
            "@context": "https://schema.org",
            "@type": "BlogPosting",
            "url": "https://example.com/blog/1"
        }
        </script>
    </head>
    <body>Email body</body>
    </html>"#;
    assert_eq!(parse_html_body(html), None);
}

#[test]
fn test_parse_html_wrong_action_type() {
    let html = r#"<html>
    <head>
        <script type="application/ld+json">
        {
            "@context": "https://schema.org",
            "@type": "EmailMessage",
            "potentialAction": {
                "@type": "ConfirmAction",
                "url": "https://example.com/confirm"
            }
        }
        </script>
    </head>
    <body>Email body</body>
    </html>"#;
    assert_eq!(parse_html_body(html), None);
}

// Unit tests for parse_json_ld
#[test]
fn test_parse_json_ld_valid() {
    let json = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "EmailMessage",
        "potentialAction": {
            "@type": "ViewAction",
            "url": "https://github.com/user/repo/pull/456#comment"
        }
    });

    let result = parse_json_ld(&json);
    assert_eq!(
        result,
        Some("https://github.com/user/repo/pull/456".to_string())
    );
}

#[test]
fn test_parse_json_ld_array() {
    let json = serde_json::json!([
        {
            "@context": "https://schema.org",
            "@type": "EmailMessage",
            "potentialAction": {
                "@type": "ViewAction",
                "url": "https://github.com/user/repo/pull/789"
            }
        }
    ]);

    let result = parse_json_ld(&json);
    assert_eq!(
        result,
        Some("https://github.com/user/repo/pull/789".to_string())
    );
}

#[test]
fn test_parse_json_ld_wrong_type() {
    let json = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "Article",
        "potentialAction": {
            "@type": "ViewAction",
            "url": "https://github.com/user/repo/pull/123"
        }
    });

    let result = parse_json_ld(&json);
    assert_eq!(result, None);
}

#[test]
fn test_parse_json_ld_wrong_action_type() {
    let json = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "EmailMessage",
        "potentialAction": {
            "@type": "EditAction",
            "url": "https://github.com/user/repo/pull/123"
        }
    });

    let result = parse_json_ld(&json);
    assert_eq!(result, None);
}

#[test]
fn test_parse_json_ld_missing_url() {
    let json = serde_json::json!({
        "@context": "https://schema.org",
        "@type": "EmailMessage",
        "potentialAction": {
            "@type": "ViewAction"
        }
    });

    let result = parse_json_ld(&json);
    assert_eq!(result, None);
}
