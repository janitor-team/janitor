//! Exit codes of the `janitor-migrate-logs` binary.

use std::process::Command;

#[test]
fn an_unreadable_config_exits_non_zero_and_says_why() {
    let td = tempfile::tempdir().expect("tempdir");
    let missing = td.path().join("no-such-file.conf");

    let out = Command::new(env!("CARGO_BIN_EXE_janitor-migrate-logs"))
        .arg("--config")
        .arg(&missing)
        .arg(td.path())
        .arg(td.path())
        .output()
        .expect("run janitor-migrate-logs");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !out.status.success(),
        "exit code was {:?}, stderr was {}",
        out.status.code(),
        stderr
    );
    assert!(
        stderr.contains("Unable to read config"),
        "stderr did not say why it stopped: {}",
        stderr
    );
}

/// A run whose log cannot be moved must make the tool exit non-zero, which is
/// what `docs/production.md` promises. Reaching it needs a real database.
#[tokio::test]
async fn a_failed_run_exits_non_zero() {
    let Some(db) = janitor::test_utils::TestDatabase::new_optional()
        .await
        .unwrap()
    else {
        return;
    };
    janitor::schema::setup_test_database(&db.pool)
        .await
        .unwrap();
    for stmt in [
        "INSERT INTO codebase (name) VALUES ('codebase')",
        "INSERT INTO change_set (id, campaign) VALUES ('cs', 'lintian-fixes')",
        "INSERT INTO run (id, codebase, suite, change_set, result_code, logfilenames) \
         VALUES ('run-1', 'codebase', 'lintian-fixes', 'cs', 'success', ARRAY['worker.log'])",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(stmt))
            .execute(&db.pool)
            .await
            .unwrap();
    }

    let Ok(base) = std::env::var("TEST_DATABASE_URL") else {
        return;
    };
    let url = format!(
        "{}/{}",
        base.rsplit_once('/').expect("a database in the url").0,
        db.database_name
    );

    let td = tempfile::tempdir().expect("tempdir");
    let conf = td.path().join("janitor.conf");
    std::fs::write(&conf, format!("database_location: {:?}\n", url)).unwrap();

    let from = td.path().join("from");
    std::fs::create_dir_all(from.join("codebase").join("run-1")).unwrap();
    std::fs::write(
        from.join("codebase").join("run-1").join("worker.log"),
        "worker\n",
    )
    .unwrap();

    // A regular file where the destination needs a directory, so the move fails.
    let to = td.path().join("to");
    std::fs::write(&to, "").unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_janitor-migrate-logs"))
        .arg("--config")
        .arg(&conf)
        .arg(&from)
        .arg(&to)
        .output()
        .expect("run janitor-migrate-logs");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !out.status.success(),
        "exit code was {:?}, stderr was {}",
        out.status.code(),
        stderr
    );
    assert!(
        stderr.contains("Failed to process 1 of 1 runs"),
        "stderr did not report the failed run: {}",
        stderr
    );
}
