use super::*;

#[test]
fn test_drop_env() {
    let mut args = vec![
        "FOO=bar".to_string(),
        "BAZ=qux".to_string(),
        "actual".to_string(),
        "command".to_string(),
    ];
    drop_env(&mut args);
    assert_eq!(args, vec!["actual", "command"]);
}

#[test]
fn test_drop_env_no_env_vars() {
    let mut args = vec!["actual".to_string(), "command".to_string()];
    let original = args.clone();
    drop_env(&mut args);
    assert_eq!(args, original);
}

#[test]
fn test_drop_env_empty() {
    let mut args = vec![];
    drop_env(&mut args);
    assert!(args.is_empty());
}

#[test]
fn test_drop_env_only_env_vars() {
    let mut args = vec!["FOO=bar".to_string(), "BAZ=qux".to_string()];
    drop_env(&mut args);
    assert!(args.is_empty());
}

#[test]
fn test_drop_env_with_equals_in_arg() {
    let mut args = vec![
        "FOO=bar".to_string(),
        "command".to_string(),
        "--option=value".to_string(),
    ];
    drop_env(&mut args);
    assert_eq!(args, vec!["command", "--option=value"]);
}

fn template_dir(templates: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, contents) in templates {
        std::fs::write(dir.path().join(name), contents).unwrap();
    }
    dir
}

fn render(
    env: &Environment,
    codemod_result: &serde_json::Value,
    extra_context: Option<&serde_json::Value>,
    debdiff: Option<&[u8]>,
    format: DescriptionFormat,
) -> Result<String, RenderDescriptionError> {
    render_proposal_description(
        env,
        "lintian-fixes",
        "run-1",
        "main",
        codemod_result,
        extra_context,
        debdiff,
        format,
    )
}

#[test]
fn test_render_proposal_description_markdown() {
    let dir = template_dir(&[(
        "lintian-fixes.md",
        "{{ campaign }}/{{ log_id }}/{{ role }}: {{ applied }} {{ codemod.applied }} {{ extra }}\n",
    )]);
    let env = load_template_env(dir.path());
    let rendered = render(
        &env,
        &serde_json::json!({"applied": 3}),
        Some(&serde_json::json!({"extra": "ctx"})),
        None,
        DescriptionFormat::Markdown,
    )
    .unwrap();
    assert_eq!(rendered, "lintian-fixes/run-1/main: 3 3 ctx");
}

#[test]
fn test_render_proposal_description_plain() {
    let dir = template_dir(&[
        ("lintian-fixes.md", "markdown"),
        ("lintian-fixes.txt", "plain {{ log_id }}: {{ debdiff }}"),
    ]);
    let env = load_template_env(dir.path());
    let rendered = render(
        &env,
        &serde_json::json!({}),
        None,
        Some(b"some diff"),
        DescriptionFormat::Plain,
    )
    .unwrap();
    assert_eq!(rendered, "plain run-1: some diff");
}

#[test]
fn test_render_proposal_description_codemod_overrides_extra_context() {
    let dir = template_dir(&[("lintian-fixes.md", "{{ value }}")]);
    let env = load_template_env(dir.path());
    let rendered = render(
        &env,
        &serde_json::json!({"value": "codemod"}),
        Some(&serde_json::json!({"value": "extra"})),
        None,
        DescriptionFormat::Markdown,
    )
    .unwrap();
    assert_eq!(rendered, "codemod");
}

#[test]
fn test_render_proposal_description_template_functions() {
    let dir = template_dir(&[(
        "lintian-fixes.md",
        "{{ parseaddr('Joe Example <joe@example.com>')[1] }}{% if debdiff_is_empty('') %} empty{% endif %}",
    )]);
    let env = load_template_env(dir.path());
    let rendered = render(
        &env,
        &serde_json::json!({}),
        None,
        None,
        DescriptionFormat::Markdown,
    )
    .unwrap();
    assert_eq!(rendered, "joe@example.com empty");
}

#[test]
fn test_render_proposal_description_missing_template() {
    let dir = template_dir(&[("lintian-fixes.md", "markdown")]);
    let env = load_template_env(dir.path());
    let err = render(
        &env,
        &serde_json::json!({}),
        None,
        None,
        DescriptionFormat::Plain,
    )
    .unwrap_err();
    assert!(
        matches!(&err, RenderDescriptionError::Template { name, .. } if name == "lintian-fixes.txt")
    );
    assert!(err
        .to_string()
        .starts_with("Template lintian-fixes.txt not found: "));
}

#[test]
fn test_render_proposal_description_render_error() {
    let dir = template_dir(&[("lintian-fixes.md", "{{ missing.attribute.nested }}")]);
    let env = load_template_env(dir.path());
    let err = render(
        &env,
        &serde_json::json!({}),
        None,
        None,
        DescriptionFormat::Markdown,
    )
    .unwrap_err();
    assert!(matches!(err, RenderDescriptionError::Render(_)));
    assert!(err.to_string().starts_with("Template rendering failed: "));
}

#[test]
fn test_proposal_description_or_default() {
    let dir = template_dir(&[
        ("lintian-fixes.md", "rendered {{ log_id }}"),
        ("lintian-fixes.txt", "{{ missing.attribute.nested }}"),
    ]);
    let env = load_template_env(dir.path());
    let describe = |campaign: &str, format: DescriptionFormat| {
        proposal_description_or_default(
            &env,
            campaign,
            "run-1",
            "main",
            &serde_json::json!({}),
            None,
            None,
            format,
        )
    };
    assert_eq!(
        describe("lintian-fixes", DescriptionFormat::Markdown),
        "rendered run-1"
    );
    // Render failure falls back to the default.
    assert_eq!(
        describe("lintian-fixes", DescriptionFormat::Plain),
        "Changes for lintian-fixes (run-1)"
    );
    // Missing template falls back to the default.
    assert_eq!(
        describe("unknown", DescriptionFormat::Markdown),
        "Changes for unknown (run-1)"
    );
}

#[test]
fn test_render_proposal_description_null_codemod_result() {
    let dir = template_dir(&[(
        "lintian-fixes.md",
        "{{ log_id }}{% if codemod is none %} none{% endif %}",
    )]);
    let env = load_template_env(dir.path());
    let rendered = render(
        &env,
        &serde_json::Value::Null,
        None,
        None,
        DescriptionFormat::Markdown,
    )
    .unwrap();
    assert_eq!(rendered, "run-1 none");
}

#[test]
fn test_render_proposal_description_binary_debdiff() {
    let dir = template_dir(&[("lintian-fixes.md", "{{ debdiff }}")]);
    let env = load_template_env(dir.path());
    let rendered = render(
        &env,
        &serde_json::json!({}),
        None,
        Some(&[0xff, 0xfe]),
        DescriptionFormat::Markdown,
    )
    .unwrap();
    assert_eq!(rendered, "(Binary diff - 2 bytes)");
}

#[test]
fn test_template_env_trims_blocks() {
    // trim_blocks and lstrip_blocks are set on the environment, and nothing
    // rendered a template with block tags on their own lines, so neither
    // setting was observable.
    let dir = template_dir(&[(
        "lintian-fixes.md",
        "a\n    {% if true %}\nb\n    {% endif %}\nc\n",
    )]);
    let env = load_template_env(dir.path());
    let rendered = render(
        &env,
        &serde_json::json!({}),
        None,
        None,
        DescriptionFormat::Markdown,
    )
    .unwrap();
    assert_eq!(rendered, "a\nb\nc");
}

#[test]
fn test_template_env_does_not_escape_markdown() {
    let dir = template_dir(&[("lintian-fixes.md", "{{ value }}")]);
    let env = load_template_env(dir.path());
    let rendered = render(
        &env,
        &serde_json::json!({"value": "<a href='x'>&"}),
        None,
        None,
        DescriptionFormat::Markdown,
    )
    .unwrap();
    assert_eq!(rendered, "<a href='x'>&");
}

#[test]
fn test_template_env_escapes_other_suffixes() {
    // render_proposal_description only ever asks for .md or .txt, so the
    // other arm of the auto-escape callback needs a direct render.
    let dir = template_dir(&[("page.html", "{{ value }}")]);
    let env = load_template_env(dir.path());
    let rendered = env
        .get_template("page.html")
        .unwrap()
        .render(minijinja::context! { value => "<b>" })
        .unwrap();
    assert_eq!(rendered, "&lt;b&gt;");
}

#[test]
fn test_template_env_registers_markdownify_debdiff() {
    let debdiff = "Files in second .changes but not in first\n-----------------------------------------\n-rw-r--r--  root/root   /usr/lib/somefile\n";
    let dir = template_dir(&[("lintian-fixes.md", "{{ markdownify_debdiff(debdiff) }}")]);
    let env = load_template_env(dir.path());
    let rendered = render(
        &env,
        &serde_json::json!({}),
        None,
        Some(debdiff.as_bytes()),
        DescriptionFormat::Markdown,
    )
    .unwrap();
    assert!(
        rendered.starts_with("### Files in second .changes but not in first"),
        "{}",
        rendered
    );
}

#[test]
fn test_parseaddr_without_a_display_name() {
    // The sequence's first element is none when the address carries no name,
    // which is why parseaddr returns a sequence rather than a pair.
    let dir = template_dir(&[(
        "lintian-fixes.md",
        "{% if parseaddr('joe@example.com')[0] is none %}none{% endif %}{{ parseaddr('joe@example.com')[1] }}",
    )]);
    let env = load_template_env(dir.path());
    let rendered = render(
        &env,
        &serde_json::json!({}),
        None,
        None,
        DescriptionFormat::Markdown,
    )
    .unwrap();
    assert_eq!(rendered, "nonejoe@example.com");
}

#[test]
fn test_render_proposal_description_non_object_extra_context() {
    // A non-object extra_context is warned about and ignored rather than
    // failing the render.
    let dir = template_dir(&[("lintian-fixes.md", "{{ log_id }}")]);
    let env = load_template_env(dir.path());
    let rendered = render(
        &env,
        &serde_json::json!({}),
        Some(&serde_json::json!("not an object")),
        None,
        DescriptionFormat::Markdown,
    )
    .unwrap();
    assert_eq!(rendered, "run-1");
}
