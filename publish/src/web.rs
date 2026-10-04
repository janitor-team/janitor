use crate::api_types::{NotFoundResponse, SuccessWithUrlResponse};
use crate::health;
use crate::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{delete, get, post, put};
use axum::Router;
use breezyshim::error::Error as BrzError;
use log::error;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;

/// Helper function to serialize values to JSON with error handling
fn json_response<T: serde::Serialize>(
    value: T,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    serde_json::to_value(value).map(Json).map_err(|e| {
        log::error!("JSON serialization error: {}", e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_response("Internal serialization error"),
        )
    })
}

/// Helper function to serialize success responses
fn success_json<T: serde::Serialize>(value: T) -> impl IntoResponse {
    match json_response(value) {
        Ok(json) => (StatusCode::OK, json),
        Err(err) => err,
    }
}

/// Helper function to create error responses safely
fn error_response(msg: &str) -> Json<serde_json::Value> {
    Json(
        serde_json::to_value(crate::api_types::ErrorResponse::new(msg.to_string()))
            .unwrap_or_else(|_| serde_json::json!({"error": "Serialization failed"})),
    )
}

/// Slim merge-proposal listing row. Matches the JSON shape
/// `py/janitor/publish.py::handle_merge_proposal_list` returns:
/// `[{url, status}, ...]` with no other fields. Each of the three
/// list_merge_proposals endpoints emits a list of these.
#[derive(serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct MergeProposalListEntry {
    /// URL of the merge proposal.
    pub url: String,
    /// Current status of the merge proposal as recorded in
    /// `merge_proposal.status`.
    pub status: Option<String>,
}

/// Shared helper for the three `/merge-proposals` listing endpoints.
/// Runs a `SELECT ... FROM merge_proposal LEFT JOIN run` scoped by the
/// optional codebase and campaign filters, and emits `[{url, status}]`.
async fn fetch_merge_proposal_list(
    conn: &sqlx::PgPool,
    codebase: Option<&str>,
    campaign: Option<&str>,
) -> Result<Vec<MergeProposalListEntry>, sqlx::Error> {
    let mut query = String::from(
        "SELECT DISTINCT ON (merge_proposal.url) \
            merge_proposal.url AS url, \
            merge_proposal.status::text AS status, \
            (run.finish_time AT TIME ZONE 'UTC') AS finish_time \
         FROM merge_proposal \
         LEFT JOIN run \
            ON merge_proposal.revision = run.revision \
            AND run.result_code = 'success'",
    );
    let mut conditions = Vec::new();
    if codebase.is_some() {
        conditions.push(format!("run.codebase = ${}", conditions.len() + 1));
    }
    if campaign.is_some() {
        conditions.push(format!("run.suite = ${}", conditions.len() + 1));
    }
    if !conditions.is_empty() {
        query.push_str(" WHERE ");
        query.push_str(&conditions.join(" AND "));
    }
    query.push_str(" ORDER BY merge_proposal.url, run.finish_time DESC");

    // The DISTINCT ON requires we select finish_time so we can sort
    // by it inside the same query, but we don't return it. Use a
    // FromRow-friendly tuple and project.
    let mut q = sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
    >(sqlx::AssertSqlSafe(&*query));
    if let Some(cb) = codebase {
        q = q.bind(cb);
    }
    if let Some(c) = campaign {
        q = q.bind(c);
    }
    let rows = q.fetch_all(conn).await?;
    Ok(rows
        .into_iter()
        .map(|(url, status, _)| MergeProposalListEntry { url, status })
        .collect())
}

async fn get_merge_proposals_by_campaign(
    State(state): State<Arc<AppState>>,
    Path(campaign): Path<String>,
) -> impl IntoResponse {
    match fetch_merge_proposal_list(&state.conn, None, Some(&campaign)).await {
        Ok(rows) => (StatusCode::OK, Json(serde_json::to_value(rows).unwrap())),
        Err(e) => {
            log::error!(
                "Error fetching merge proposals for campaign {}: {}",
                campaign,
                e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_response("Database error"),
            )
        }
    }
}

async fn get_merge_proposals_by_codebase(
    State(state): State<Arc<AppState>>,
    Path(codebase): Path<String>,
) -> impl IntoResponse {
    match fetch_merge_proposal_list(&state.conn, Some(&codebase), None).await {
        Ok(rows) => (StatusCode::OK, Json(serde_json::to_value(rows).unwrap())),
        Err(e) => {
            log::error!(
                "Error fetching merge proposals for codebase {}: {}",
                codebase,
                e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_response("Database error"),
            )
        }
    }
}

async fn list_merge_proposals(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match fetch_merge_proposal_list(&state.conn, None, None).await {
        Ok(rows) => (StatusCode::OK, Json(serde_json::to_value(rows).unwrap())),
        Err(e) => {
            log::error!("Error fetching merge proposals: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_response("Database error"),
            )
        }
    }
}

/// Response structure for absorbed runs.
#[derive(serde::Serialize, sqlx::FromRow)]
struct AbsorbedRun {
    mode: String,
    change_set: Option<String>,
    codebase: String,
    delay: Option<i64>, // seconds
    campaign: String,
    result: Option<serde_json::Value>,
    id: String,
    absorbed_at: Option<chrono::DateTime<chrono::Utc>>,
    merged_by: Option<String>,
    #[serde(rename = "merged-by-url")]
    merged_by_url: Option<String>,
    merge_proposal_url: Option<String>,
}

async fn absorbed(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let mut query = String::from(
        r#"
        SELECT
            mode,
            change_set,
            codebase,
            EXTRACT(epoch FROM delay) as delay,
            campaign,
            result,
            id,
            absorbed_at,
            merged_by,
            merge_proposal_url
        FROM absorbed_runs
        "#,
    );

    let mut query_params = Vec::new();

    if let Some(since_str) = params.get("since") {
        match chrono::DateTime::parse_from_rfc3339(since_str) {
            Ok(since) => {
                query_params.push(since.with_timezone(&chrono::Utc));
                query.push_str(&format!(" WHERE absorbed_at >= ${}", query_params.len()));
            }
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    error_response("Invalid date format for 'since' parameter"),
                );
            }
        }
    }

    query.push_str(" ORDER BY absorbed_at DESC");

    let mut absorbed_runs = Vec::new();

    let query_result = if query_params.is_empty() {
        sqlx::query_as::<_, AbsorbedRun>(sqlx::AssertSqlSafe(&*query))
            .fetch_all(&state.conn)
            .await
    } else {
        sqlx::query_as::<_, AbsorbedRun>(sqlx::AssertSqlSafe(&*query))
            .bind(query_params[0])
            .fetch_all(&state.conn)
            .await
    };

    match query_result {
        Ok(rows) => {
            // Resolve merged-by-url for each row via crate::get_merged_by_user_url.
            // The helper makes a forge call (get_forge_by_hostname +
            // get_user_url) on a blocking thread. Different rows hit
            // different forges, so spawn all lookups up-front and await
            // them together rather than one at a time. Failures are
            // non-fatal: the row is still returned with merged_by_url = None.
            let lookups: Vec<_> = rows
                .iter()
                .map(|row| {
                    let mp_url = row
                        .merge_proposal_url
                        .as_deref()
                        .and_then(|s| s.parse::<url::Url>().ok());
                    let merged_by = row.merged_by.clone();
                    match (mp_url, merged_by) {
                        (Some(mp_url), Some(merged_by)) => {
                            Some(tokio::task::spawn_blocking(move || {
                                (
                                    mp_url.clone(),
                                    crate::get_merged_by_user_url(&mp_url, &merged_by),
                                )
                            }))
                        }
                        _ => None,
                    }
                })
                .collect();

            for (mut row, lookup) in rows.into_iter().zip(lookups) {
                if let Some(handle) = lookup {
                    row.merged_by_url = match handle.await {
                        Ok((_, Ok(Some(u)))) => Some(u.to_string()),
                        Ok((_, Ok(None))) => None,
                        Ok((mp_url, Err(e))) => {
                            log::debug!("get_merged_by_user_url failed for {}: {}", mp_url, e);
                            None
                        }
                        Err(_) => None,
                    };
                }
                absorbed_runs.push(row);
            }
        }
        Err(e) => {
            log::error!("Failed to fetch absorbed runs: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::to_value(crate::api_types::ErrorResponse::new(
                        "Failed to fetch absorbed runs",
                    ))
                    .unwrap_or_else(
                        |_| serde_json::json!({"error": "Failed to fetch absorbed runs"}),
                    ),
                ),
            );
        }
    }

    (
        StatusCode::OK,
        json_response(absorbed_runs).unwrap_or_else(|(_, msg)| msg),
    )
}

/// Per-role branch policy entry as it appears on the wire. Matches
/// the shape `py/janitor/publish.py::handle_policy_get` returns.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct PerBranchPolicy {
    /// Publish mode for this branch role (e.g. "propose", "push").
    mode: String,
    /// Optional max frequency between publish attempts, in days.
    max_frequency_days: Option<i32>,
}

/// Full policy document as returned by `GET /policy/{name}` and
/// accepted by `PUT /policy/{name}`.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct PolicyDocument {
    /// Optional rate-limit bucket key.
    rate_limit_bucket: Option<String>,
    /// Per-branch role -> policy entry map.
    per_branch: HashMap<String, PerBranchPolicy>,
}

/// Read the per-branch policy rows for a single named publish policy
/// via UNNEST (the composite-array column is not directly decodable
/// from sqlx, so we explode it into `(role, mode, frequency_days)`
/// rows at the SQL level - same trick `crate::get_publish_policy`
/// uses).
async fn read_per_branch_policy(
    conn: &sqlx::PgPool,
    name: &str,
) -> Result<HashMap<String, PerBranchPolicy>, sqlx::Error> {
    let rows: Vec<(String, String, Option<i32>)> = sqlx::query_as(
        r#"
        SELECT pp.role, pp.mode::text, pp.frequency_days
        FROM named_publish_policy
        CROSS JOIN UNNEST(named_publish_policy.per_branch_policy) AS pp
        WHERE name = $1
        "#,
    )
    .bind(name)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(role, mode, max_frequency_days)| {
            (
                role,
                PerBranchPolicy {
                    mode,
                    max_frequency_days,
                },
            )
        })
        .collect())
}

/// Project a [`PolicyDocument`]'s per-branch entries into three
/// parallel vectors `(roles, modes, max_frequency_days)`. The DB
/// composite-type insert in [`upsert_named_publish_policy`] takes
/// these as `text[]`, `text[]`, `int[]`.
pub(crate) fn project_per_branch_policy(
    document: &PolicyDocument,
) -> (Vec<String>, Vec<String>, Vec<Option<i32>>) {
    let mut roles = Vec::with_capacity(document.per_branch.len());
    let mut modes = Vec::with_capacity(document.per_branch.len());
    let mut freqs = Vec::with_capacity(document.per_branch.len());
    for (role, entry) in &document.per_branch {
        roles.push(role.clone());
        modes.push(entry.mode.clone());
        freqs.push(entry.max_frequency_days);
    }
    (roles, modes, freqs)
}

/// Persist a full policy document, replacing the existing
/// per_branch_policy. Builds the `branch_publish_policy[]` composite
/// array server-side from three parallel text/text/int arrays, the
/// same way `runner::database::store_run` builds `result_tag[]`.
async fn upsert_named_publish_policy<'e, E>(
    executor: E,
    name: &str,
    document: &PolicyDocument,
) -> Result<(), sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let (roles, modes, freqs) = project_per_branch_policy(document);

    sqlx::query(
        r#"
        INSERT INTO named_publish_policy (name, per_branch_policy, rate_limit_bucket)
        VALUES (
            $1,
            (
                SELECT COALESCE(
                    array_agg(ROW(role, mode::publish_mode, freq)::branch_publish_policy),
                    ARRAY[]::branch_publish_policy[]
                )
                FROM unnest($2::text[], $3::text[], $4::int[])
                  AS t(role, mode, freq)
            ),
            $5
        )
        ON CONFLICT (name) DO UPDATE SET
            per_branch_policy = EXCLUDED.per_branch_policy,
            rate_limit_bucket = EXCLUDED.rate_limit_bucket
        "#,
    )
    .bind(name)
    .bind(&roles)
    .bind(&modes)
    .bind(&freqs)
    .bind(document.rate_limit_bucket.as_deref())
    .execute(executor)
    .await?;
    Ok(())
}

async fn get_policy(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Confirm the named policy exists, then load its per-branch rows.
    let head: Option<(Option<String>,)> =
        match sqlx::query_as("SELECT rate_limit_bucket FROM named_publish_policy WHERE name = $1")
            .bind(&name)
            .fetch_optional(&state.conn)
            .await
        {
            Ok(row) => row,
            Err(e) => {
                log::error!("Error fetching policy {}: {}", name, e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error_response("Database error"),
                );
            }
        };

    let rate_limit_bucket = match head {
        Some((b,)) => b,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    serde_json::to_value(NotFoundResponse {
                        reason: "Publish policy not found".to_string(),
                        name: Some(name),
                        url: None,
                        id: None,
                        bucket: None,
                        run_id: None,
                        campaign: None,
                        codebase: None,
                    })
                    .unwrap(),
                ),
            );
        }
    };

    let per_branch = match read_per_branch_policy(&state.conn, &name).await {
        Ok(map) => map,
        Err(e) => {
            log::error!("Error reading per-branch policy for {}: {}", name, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_response("Database error"),
            );
        }
    };

    let document = PolicyDocument {
        rate_limit_bucket,
        per_branch,
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(document).unwrap()),
    )
}

async fn get_policies(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Stream the response one named_publish_policy at a time instead
    // of buffering the whole table.
    //
    // Previously this buffered every row via `fetch_all`, copied the
    // whole set into a `HashMap<String, PolicyDocument>`, cloned it
    // again into a `serde_json::Value` tree, and then serialised that
    // - about 4× peak memory. With ~648k named_publish_policy rows
    // (and on the order of 1.3M rows once the per-branch UNNEST is
    // applied) the publisher pod blew through its 1Gi memory limit
    // and got OOMKilled; the daily policy-refresh CronJob saw
    // `connection closed before message completed` and panicked.
    //
    // The SQL is `ORDER BY npp.name`, so consecutive rows for the
    // same name arrive back-to-back. We build only one
    // PolicyDocument's worth of data at a time, serialise it with
    // serde_json (which handles JSON escaping for the codebase
    // names - important since they're free-form text), and emit it
    // directly to the response body wrapped by `{` ... `}` and
    // separators. Peak memory is now O(per-branch entries per name),
    // not O(table).
    use bytes::Bytes;
    use sqlx::Row;
    use std::convert::Infallible;
    let pool = state.conn.clone();
    let stream = async_stream::try_stream! {
        let mut rows = sqlx::query(
            r#"
            SELECT npp.name, npp.rate_limit_bucket,
                   pp.role, pp.mode::text AS mode, pp.frequency_days
              FROM named_publish_policy npp
              CROSS JOIN UNNEST(npp.per_branch_policy) AS pp
             ORDER BY npp.name
            "#,
        )
        .fetch(&pool);

        // Helper: serialise one `"<name>":<json>` entry as a Bytes
        // chunk, prefixing with `,` for entries after the first. Pure
        // function so the `?` lives in the caller's `try_stream!`
        // scope (using a `macro_rules!` here ran the `?` operator
        // through a layer the macro expansion couldn't see).
        fn render_entry(
            name: &str,
            doc: &PolicyDocument,
            first: &mut bool,
        ) -> Result<Bytes, serde_json::Error> {
            let mut buf = Vec::with_capacity(256);
            if !*first {
                buf.push(b',');
            }
            *first = false;
            // serde_json::to_writer escapes the name correctly
            // (codebase names can in theory contain `"`).
            serde_json::to_writer(&mut buf, name)?;
            buf.push(b':');
            serde_json::to_writer(&mut buf, doc)?;
            Ok(Bytes::from(buf))
        }

        // Outer `{`. We emit each `"name":{...}` entry separated by
        // commas; the `first` flag controls the comma.
        yield Bytes::from_static(b"{");
        let mut first = true;
        let mut current_name: Option<String> = None;
        let mut current_doc: Option<PolicyDocument> = None;

        use futures::StreamExt;
        while let Some(row) = rows.next().await {
            let row = row?;
            let name: String = row.try_get("name")?;
            let rate_limit_bucket: Option<String> = row.try_get("rate_limit_bucket")?;
            let role: String = row.try_get("role")?;
            let mode: String = row.try_get("mode")?;
            let frequency_days: Option<i32> = row.try_get("frequency_days")?;

            if current_name.as_deref() != Some(&name) {
                if let (Some(prev_name), Some(prev_doc)) =
                    (current_name.take(), current_doc.take())
                {
                    yield render_entry(&prev_name, &prev_doc, &mut first)
                        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
                }
                current_name = Some(name.clone());
                current_doc = Some(PolicyDocument {
                    rate_limit_bucket,
                    per_branch: HashMap::new(),
                });
            }
            if let Some(doc) = current_doc.as_mut() {
                doc.per_branch.insert(
                    role,
                    PerBranchPolicy {
                        mode,
                        max_frequency_days: frequency_days,
                    },
                );
            }
        }
        if let (Some(prev_name), Some(prev_doc)) = (current_name.take(), current_doc.take()) {
            yield render_entry(&prev_name, &prev_doc, &mut first)
                .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
        }
        yield Bytes::from_static(b"}");
    };
    // Map sqlx::Error -> Infallible by logging+truncating: a mid-stream
    // DB error would already have produced partial JSON anyway, and
    // the client treats truncation as an IncompleteMessage retry.
    let mapped = futures::StreamExt::map(
        stream,
        |r: Result<Bytes, sqlx::Error>| -> Result<Bytes, Infallible> {
            match r {
                Ok(b) => Ok(b),
                Err(e) => {
                    log::error!("Error streaming policies: {}", e);
                    Ok(Bytes::new())
                }
            }
        },
    );

    let body = axum::body::Body::from_stream(mapped);
    let resp = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(body)
        .expect("response builder");
    resp.into_response()
}

async fn put_policy(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(document): Json<PolicyDocument>,
) -> impl IntoResponse {
    match upsert_named_publish_policy(&state.conn, &name, &document).await {
        Ok(()) => {
            log::info!("Successfully created/updated policy: {}", name);
            (StatusCode::OK, Json(serde_json::json!({})))
        }
        Err(e) => {
            log::error!("Error creating/updating policy {}: {}", name, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_response("Database error"),
            )
        }
    }
}

/// `PUT /policy`: bulk replace the named publish policies. Takes a
/// `{policy_name: PolicyDocument, ...}` JSON object, upserts each entry,
/// and DELETEs every other named_publish_policy row in one transaction.
async fn put_policies(
    State(state): State<Arc<AppState>>,
    Json(documents): Json<HashMap<String, PolicyDocument>>,
) -> impl IntoResponse {
    let mut tx = match state.conn.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            log::error!("Failed to begin transaction: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_response("Database error"),
            );
        }
    };

    let mut keep_names: Vec<String> = Vec::with_capacity(documents.len());
    for (name, document) in documents.iter() {
        if let Err(e) = upsert_named_publish_policy(&mut *tx, name, document).await {
            log::error!("Failed to update policy {}: {}", name, e);
            let _ = tx.rollback().await;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::to_value(crate::api_types::ErrorResponse::new(format!(
                        "Failed to update policy {}",
                        name
                    )))
                    .unwrap(),
                ),
            );
        }
        keep_names.push(name.clone());
    }

    if let Err(e) =
        sqlx::query("DELETE FROM named_publish_policy WHERE NOT (name = ANY($1::text[]))")
            .bind(&keep_names)
            .execute(&mut *tx)
            .await
    {
        log::error!("Failed to prune obsolete policies: {}", e);
        let _ = tx.rollback().await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_response("Database error"),
        );
    }

    if let Err(e) = tx.commit().await {
        log::error!("Failed to commit transaction: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Database error"})),
        );
    }

    (StatusCode::OK, Json(serde_json::json!({})))
}

/// Form body for `POST /merge-proposal`. Matches the urlencoded shape
/// `py/janitor/site/api.py::handle_merge_proposal_change` forwards:
/// `url=...&status=...` plus an optional `comment`.
#[derive(serde::Deserialize)]
struct UpdateMergeProposalForm {
    url: String,
    status: String,
    comment: Option<String>,
}

/// Statuses that mean "the proposal is no longer open"; used by the
/// transition guard in `update_merge_proposal`.
const CLOSED_STATUSES: &[&str] = &["closed", "abandoned", "rejected", "applied"];

/// What the operator-initiated MP status transition handler should
/// do. Returned by [`classify_mp_status_transition`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MpStatusTransition {
    /// Both the current and new status are in `CLOSED_STATUSES`: no
    /// forge interaction needed, just rewrite the DB row.
    NoOpUpdate,
    /// open -> closed: open the proposal on the forge, optionally
    /// post a comment, then close it, then update the DB.
    CloseOnForge,
    /// Anything else (closed -> open, unknown statuses, etc.) is a
    /// 400: the publisher refuses to manufacture forge transitions it
    /// doesn't know how to perform.
    Forbidden,
}

/// Classify the requested merge-proposal status transition into one of
/// three actions. Both status strings are matched against
/// `CLOSED_STATUSES` to decide whether they're closed-class.
#[must_use]
pub(crate) fn classify_mp_status_transition(
    current_status: &str,
    new_status: &str,
) -> MpStatusTransition {
    let current_closed = CLOSED_STATUSES.contains(&current_status);
    let new_closed = CLOSED_STATUSES.contains(&new_status);
    if current_closed && new_closed {
        MpStatusTransition::NoOpUpdate
    } else if current_status == "open" && new_closed {
        MpStatusTransition::CloseOnForge
    } else {
        MpStatusTransition::Forbidden
    }
}

/// `POST /merge-proposal`: operator-initiated state transition for a
/// merge proposal.
///
///   1. Read the current `status` from the `merge_proposal` row.
///   2. closed -> closed transitions are no-ops on the forge; the
///      post-status is still written to the row.
///   3. open -> closed transitions close the proposal on the forge:
///      open it via `MergeProposal.from_url`, post the comment if one
///      was supplied (PermissionDenied logged but non-fatal), then
///      `mp.close()` (PermissionDenied reraised so the operator knows
///      the close didn't take).
///   4. Anything else (closed -> open, etc.) is a 400.
///   5. After the forge work, the new status is written to the row.
async fn update_merge_proposal(
    State(state): State<Arc<AppState>>,
    axum::extract::Form(form): axum::extract::Form<UpdateMergeProposalForm>,
) -> impl IntoResponse {
    let mut tx = match state.conn.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            log::error!(
                "Error opening transaction for merge proposal {}: {}",
                form.url,
                e
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_response("Database error"),
            );
        }
    };

    let row: Option<(String,)> =
        match sqlx::query_as("SELECT status FROM merge_proposal WHERE url = $1")
            .bind(&form.url)
            .fetch_optional(&mut *tx)
            .await
        {
            Ok(row) => row,
            Err(e) => {
                log::error!(
                    "Error reading merge proposal {} for transition: {}",
                    form.url,
                    e
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error_response("Database error"),
                );
            }
        };

    let current_status = match row {
        Some((s,)) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    serde_json::to_value(NotFoundResponse {
                        reason: "No such merge proposal".to_string(),
                        url: Some(form.url),
                        name: None,
                        id: None,
                        bucket: None,
                        run_id: None,
                        campaign: None,
                        codebase: None,
                    })
                    .unwrap(),
                ),
            );
        }
    };

    match classify_mp_status_transition(&current_status, &form.status) {
        MpStatusTransition::NoOpUpdate => {
            // No-op transition; just update the row below.
        }
        MpStatusTransition::Forbidden => {
            return (
                StatusCode::BAD_REQUEST,
                error_response(&format!(
                    "no transition from {} to {}",
                    current_status, form.status
                )),
            );
        }
        MpStatusTransition::CloseOnForge => {
            // Real state change - open the proposal on the forge and
            // close it. spawn_blocking because breezyshim is sync.
            let mp_url = match form.url.parse::<url::Url>() {
                Ok(u) => u,
                Err(e) => {
                    log::warn!("Bad merge proposal URL {}: {}", form.url, e);
                    return (
                        StatusCode::BAD_REQUEST,
                        error_response("Invalid merge proposal URL"),
                    );
                }
            };

            let mp = match tokio::task::spawn_blocking({
                let mp_url = mp_url.clone();
                move || breezyshim::forge::MergeProposal::from_url(&mp_url)
            })
            .await
            {
                Ok(Ok(mp)) => mp,
                Ok(Err(e)) => {
                    log::error!("Failed to open merge proposal {}: {}", form.url, e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        error_response("Failed to open merge proposal"),
                    );
                }
                Err(_) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        error_response("Task join error"),
                    );
                }
            };

            if let Some(comment) = form.comment.as_deref() {
                log::info!("{}: {}", mp_url, comment);
                let post_result = tokio::task::spawn_blocking({
                    let mp = mp.clone();
                    let comment = comment.to_string();
                    move || mp.post_comment(&comment)
                })
                .await;
                match post_result {
                    Ok(Ok(())) => {}
                    Ok(Err(breezyshim::error::Error::PermissionDenied(_, msg))) => {
                        log::warn!(
                            "Permission denied posting comment to {}: {}",
                            mp_url,
                            msg.unwrap_or_default()
                        );
                    }
                    Ok(Err(e)) => {
                        log::warn!("Failed to post comment to {}: {}", mp_url, e);
                    }
                    Err(_) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            error_response("Task join error"),
                        );
                    }
                }
            }

            let close_result = tokio::task::spawn_blocking({
                let mp = mp.clone();
                move || mp.close()
            })
            .await;
            match close_result {
                Ok(Ok(())) => {}
                Ok(Err(breezyshim::error::Error::PermissionDenied(_, msg))) => {
                    let msg = msg.unwrap_or_default();
                    log::warn!(
                        "Permission denied closing merge request {}: {}",
                        mp_url,
                        msg
                    );
                    return (
                        StatusCode::FORBIDDEN,
                        error_response(&format!(
                            "Permission denied closing merge request: {}",
                            msg
                        )),
                    );
                }
                Ok(Err(e)) => {
                    log::warn!("Failed to close merge request {}: {}", mp_url, e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        error_response("Failed to close merge request"),
                    );
                }
                Err(_) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        error_response("Task join error"),
                    );
                }
            }
        }
    }

    // ::merge_proposal_status cast - same enum/text mismatch as in
    // proposal_info.rs::update_proposal_info.
    if let Err(e) =
        sqlx::query("UPDATE merge_proposal SET status = $1::merge_proposal_status WHERE url = $2")
            .bind(&form.status)
            .bind(&form.url)
            .execute(&mut *tx)
            .await
    {
        log::error!("Error updating merge proposal {} status: {}", form.url, e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_response("Database error"),
        );
    }

    if let Err(e) = tx.commit().await {
        log::error!("Error committing merge proposal {} update: {}", form.url, e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            error_response("Database error"),
        );
    }

    log::info!("Successfully updated merge proposal: {}", form.url);
    (
        StatusCode::OK,
        Json(
            serde_json::to_value(SuccessWithUrlResponse {
                status: "success".to_string(),
                url: form.url,
            })
            .unwrap(),
        ),
    )
}

/// `DELETE /policy/{name}`: drop a named publish policy.
///
/// On a foreign-key violation (a candidate still references the policy)
/// return 412 Precondition Failed with an empty body. On success,
/// including "no such policy", return 200 with an empty body.
async fn delete_policy(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match sqlx::query("DELETE FROM named_publish_policy WHERE name = $1")
        .bind(&name)
        .execute(&state.conn)
        .await
    {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({}))),
        Err(e)
            if e.as_database_error()
                .map(|e| e.is_foreign_key_violation())
                .unwrap_or(false) =>
        {
            (StatusCode::PRECONDITION_FAILED, Json(serde_json::json!({})))
        }
        Err(e) => {
            log::warn!("Error deleting policy {}: {}", name, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_response("Error deleting policy"),
            )
        }
    }
}

async fn consider(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> impl IntoResponse {
    async fn run(state: Arc<AppState>, id: String) {
        let (run, rate_limit_bucket, command, unpublished_branches) =
            match crate::state::iter_publish_ready(&state.conn, Some(&id)).await {
                Ok(results) => match results.into_iter().next() {
                    Some((run, rate_limit_bucket, command, unpublished_branches)) => {
                        (run, rate_limit_bucket, command, unpublished_branches)
                    }
                    None => {
                        log::warn!("No publish-ready runs found for id: {}", id);
                        return;
                    }
                },
                Err(e) => {
                    log::error!("Failed to fetch publish-ready runs for id {}: {}", id, e);
                    return;
                }
            };
        // The manual /consider/{run_id} endpoint deliberately omits
        // push_limit so operators can bypass the global push budget.
        // The bulk /autopublish path still respects it.
        if let Err(e) = crate::consider_publish_run(
            &state.conn,
            state.redis.clone(),
            state.config,
            &state.publish_worker,
            &state.vcs_managers,
            &state.bucket_rate_limiter,
            &run,
            &rate_limit_bucket,
            unpublished_branches.as_slice(),
            &command,
            None,
            state.require_binary_diff,
        )
        .await
        {
            log::error!("Failed to consider publish run for id {}: {}", id, e);
        }
    }

    tokio::spawn(run(state.clone(), id));
    (StatusCode::OK, Json(serde_json::json!({})))
}

#[derive(serde::Serialize, serde::Deserialize, sqlx::FromRow)]
/// Details about a publish operation.
pub struct PublishDetails {
    codebase: Option<String>,
    target_branch_url: Option<String>,
    branch_name: Option<String>,
    main_branch_revision: Option<String>,
    revision: Option<String>,
    mode: String,
    merge_proposal_url: Option<String>,
    result_code: String,
    description: Option<String>,
}

async fn get_publish_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let publish = match sqlx::query_as::<_, PublishDetails>(
        r#"
SELECT
  codebase,
  branch_name,
  main_branch_revision,
  revision,
  mode,
  merge_proposal_url,
  target_branch_url,
  result_code,
  description
FROM publish
LEFT JOIN codebase
ON codebase.branch_url = publish.target_branch_url
WHERE id = $1
"#,
    )
    .bind(&id)
    .fetch_optional(&state.conn)
    .await
    {
        Ok(result) => result,
        Err(e) => {
            log::error!("Failed to fetch publish details for id {}: {}", id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::to_value(crate::api_types::ErrorResponse::new(
                        "Failed to fetch publish details",
                    ))
                    .unwrap_or_else(
                        |_| serde_json::json!({"error": "Failed to fetch publish details"}),
                    ),
                ),
            );
        }
    };

    if let Some(details) = publish {
        match json_response(details) {
            Ok(json) => (StatusCode::OK, json),
            Err((status, msg)) => (status, msg),
        }
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(
                serde_json::to_value(NotFoundResponse {
                    reason: "No such publish".to_string(),
                    id: Some(id),
                    name: None,
                    url: None,
                    bucket: None,
                    run_id: None,
                    campaign: None,
                    codebase: None,
                })
                .unwrap(),
            ),
        )
    }
}

#[derive(serde::Deserialize, Default)]
struct PublishForm {
    /// One of "push-derived" / "push" / "propose" / "attempt-push".
    /// When None, each role falls back to its configured publish
    /// policy mode (or MODE_SKIP).
    mode: Option<String>,
    /// Free-form text recorded on the publish row.
    requester: Option<String>,
}

/// Query parameters for the same endpoint. Python reads `role` from
/// the query string (`?role=main`); we keep parity.
#[derive(serde::Deserialize, Default)]
struct PublishQuery {
    role: Option<String>,
}

/// `POST /{campaign}/{codebase}/publish`: operator-initiated publish of
/// the latest effective run. Loads the last effective run, resolves a
/// mode per role from either the `mode` form field or the configured
/// publish policy, and spawns one `publish_and_store` call per role in
/// the background.
///
/// Returns 400 if there's no effective run, 200 + `{run_id, code,
/// description}` if nothing ends up being published, or 202 +
/// `{run_id, mode, publish_ids}` if at least one role was queued.
///
/// No rate-limit or already-published check: this endpoint is the
/// operator override.
async fn publish(
    State(state): State<Arc<AppState>>,
    Path((campaign, codebase)): Path<(String, String)>,
    Query(query): Query<PublishQuery>,
    axum::extract::Form(form): axum::extract::Form<PublishForm>,
) -> impl IntoResponse {
    log::info!("Handling request to publish {}/{}", codebase, campaign);

    // Validate mode up-front if the caller supplied one.
    if let Some(m) = form.mode.as_deref() {
        if !matches!(m, "push-derived" | "push" | "propose" | "attempt-push") {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Invalid mode", "mode": m})),
            );
        }
    }

    let run = match crate::state::get_last_effective_run(&state.conn, &codebase, &campaign).await {
        Ok(Some(r)) => Arc::new(r),
        Ok(None) => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({})));
        }
        Err(e) => {
            log::error!(
                "Error loading last effective run for {}/{}: {}",
                codebase,
                campaign,
                e
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_response("Database error"),
            );
        }
    };

    let (publish_policy, _cmd, rate_limit_bucket) =
        match crate::get_publish_policy(&state.conn, &codebase, &campaign).await {
            Ok(Some(p)) => p,
            Ok(None) => (HashMap::new(), None, None),
            Err(e) => {
                log::error!(
                    "Error loading publish policy for {}/{}: {}",
                    codebase,
                    campaign,
                    e
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error_response("Database error"),
                );
            }
        };
    let rate_limit_bucket = Arc::new(rate_limit_bucket);
    let requester = Arc::new(form.requester.clone());

    // Role selection: one specific role via ?role=..., or all of
    // the run's result branches.
    let roles: Vec<String> = if let Some(r) = query.role {
        vec![r]
    } else {
        run.result_branches
            .as_ref()
            .map(|bs| bs.iter().map(|(role, _, _, _)| role.clone()).collect())
            .unwrap_or_default()
    };

    // For each role, resolve the mode: either the form mode applied
    // to every role, or the per-role policy mode with MODE_SKIP as
    // the fallback.
    let branches: Vec<(String, String)> = if let Some(ref m) = form.mode {
        roles.into_iter().map(|r| (r, m.clone())).collect()
    } else {
        roles
            .into_iter()
            .map(|r| {
                let mode = publish_policy
                    .get(&r)
                    .map(|(m, _)| m.clone())
                    .unwrap_or_else(|| "skip".to_string());
                (r, mode)
            })
            .collect()
    };

    // Spawn one publish_and_store per active (role, mode) pair.
    // Skip/build-only roles get recorded in publish_ids without
    // queuing any work.
    let mut publish_ids: HashMap<String, String> = HashMap::new();
    for (role, mode_str) in branches {
        let publish_id = uuid::Uuid::new_v4().to_string();
        publish_ids.insert(role.clone(), publish_id.clone());

        log::info!(".. publishing for role {}: {}", role, mode_str);

        if mode_str == "skip" || mode_str == "build-only" {
            continue;
        }

        let mode = match <crate::Mode as std::str::FromStr>::from_str(&mode_str) {
            Ok(m) => m,
            Err(e) => {
                log::warn!(
                    "Invalid mode {:?} for role {} in publish request: {}",
                    mode_str,
                    role,
                    e
                );
                continue;
            }
        };

        let state = state.clone();
        let run = run.clone();
        let rate_limit_bucket = rate_limit_bucket.clone();
        let requester = requester.clone();
        let role_owned = role.clone();
        let publish_id_owned = publish_id.clone();
        tokio::spawn(async move {
            let campaign_config = match state.config.campaign.iter().find(|c| c.name() == run.suite)
            {
                Some(c) => c,
                None => {
                    log::warn!(
                        "publish handler: no campaign config for suite {}",
                        run.suite
                    );
                    return;
                }
            };
            if let Err(e) = crate::publish_and_store(
                &state.conn,
                state.redis_manager.as_ref(),
                campaign_config,
                &state.publish_worker,
                &publish_id_owned,
                &run,
                mode,
                &role_owned,
                rate_limit_bucket.as_deref(),
                &state.vcs_managers,
                &state.bucket_rate_limiter,
                Some(true), // allow_create_proposal
                false,      // require_binary_diff
                requester.as_deref(),
            )
            .await
            {
                log::warn!(
                    "publish_and_store failed for {}/{}/{}: {}",
                    run.codebase,
                    run.suite,
                    role_owned,
                    e
                );
            }
        });
    }

    if publish_ids.is_empty() {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "run_id": run.id,
                "code": "done",
                "description": "Nothing to do",
            })),
        );
    }

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "run_id": run.id,
            "mode": form.mode,
            "publish_ids": publish_ids,
        })),
    )
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ForgeCredentials {
    kind: String,
    name: String,
    url: url::Url,
    user: Option<String>,
    user_url: Option<url::Url>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Credentials {
    ssh_keys: Vec<String>,
    pgp_keys: Vec<String>,
    hosting: Vec<ForgeCredentials>,
}

/// `GET /credentials`: surface the publisher's SSH public keys, minimal
/// PGP exports, and per-forge login state.
///
/// All the breezyshim work runs on a blocking thread: breezyshim is
/// synchronous and each forge call goes through PyO3, so running these
/// on the async runtime would freeze the worker.
async fn get_credentials(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // SSH public keys: scan ~/.ssh for *.pub. Use dirs::home_dir
    // semantics (HOME on Unix) but fall back to /root on
    // containers where HOME is unset rather than silently
    // returning an empty list.
    let ssh_dir = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/root"))
        .join(".ssh");

    let mut ssh_keys: Vec<String> = Vec::new();
    if ssh_dir.is_dir() {
        match std::fs::read_dir(&ssh_dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("pub") {
                        continue;
                    }
                    match std::fs::read_to_string(&path) {
                        Ok(content) => {
                            ssh_keys.extend(
                                content
                                    .lines()
                                    .map(str::trim)
                                    .filter(|l| !l.is_empty())
                                    .map(str::to_string),
                            );
                        }
                        Err(e) => {
                            log::warn!("Failed to read SSH public key {}: {}", path.display(), e)
                        }
                    }
                }
            }
            Err(e) => log::warn!("Failed to read SSH directory {}: {}", ssh_dir.display(), e),
        }
    }

    // PGP keys: enumerate the secret keyring and minimal-export each.
    // breezyshim::gpg uses PyO3 internally, so move the work to a
    // blocking thread.
    let pgp_keys = {
        let gpg = state.gpg.clone();
        match tokio::task::spawn_blocking(move || {
            let mut keys = Vec::new();
            for entry in gpg.keylist(true) {
                let exported = gpg.key_export_minimal(&entry.fpr);
                match String::from_utf8(exported) {
                    Ok(armored) => keys.push(armored),
                    Err(e) => log::warn!("Skipping PGP key {}: invalid UTF-8: {}", entry.fpr, e),
                }
            }
            keys
        })
        .await
        {
            Ok(keys) => keys,
            Err(e) => {
                log::error!("PGP keylist task panicked: {}", e);
                Vec::new()
            }
        }
    };

    // Forge enumeration: also blocking PyO3 work.
    let hosting = match tokio::task::spawn_blocking(|| {
        let mut hosting: Vec<ForgeCredentials> = Vec::new();
        for instance in breezyshim::forge::iter_forge_instances() {
            let current_user = match instance.get_current_user() {
                Ok(user) => user,
                Err(BrzError::ForgeLoginRequired) => continue,
                Err(BrzError::UnsupportedForge(..)) => continue,
                Err(BrzError::RedirectRequested { .. }) => continue,
                Err(e) => {
                    log::warn!(
                        "Error getting current user for {}: {}",
                        instance.forge_name(),
                        e
                    );
                    continue;
                }
            };
            let current_user_url = match current_user.as_ref() {
                Some(u) => match instance.get_user_url(u) {
                    Ok(url) => Some(url),
                    Err(e) => {
                        log::warn!(
                            "Error getting user URL for {} on {}: {}",
                            u,
                            instance.forge_name(),
                            e
                        );
                        None
                    }
                },
                None => None,
            };
            hosting.push(ForgeCredentials {
                kind: instance.forge_kind(),
                name: instance.forge_name(),
                url: instance.base_url(),
                user: current_user,
                user_url: current_user_url,
            });
        }
        hosting
    })
    .await
    {
        Ok(hosting) => hosting,
        Err(e) => {
            log::error!("Forge enumeration task panicked: {}", e);
            Vec::new()
        }
    };

    (
        StatusCode::OK,
        Json(Credentials {
            ssh_keys,
            pgp_keys,
            hosting,
        }),
    )
}

// Health and ready handlers are now provided by shared_web module

/// Re-fetch a stale merge-proposal URL to discover whether it has
/// moved (`update_canonical_url`) or disappeared
/// (`delete_proposal_info`). Other status codes are logged and ignored.
/// Reused by `queue::check_stragglers`.
pub(crate) async fn check_straggler(
    proposal_info_manager: &crate::proposal_info::ProposalInfoManager,
    url: &url::Url,
) {
    let resp = match reqwest::get(url.to_string()).await {
        Ok(resp) => resp,
        Err(e) => {
            log::warn!("Got error loading straggler {}: {}", url, e);
            return;
        }
    };

    let status = resp.status();
    if status == reqwest::StatusCode::OK {
        if resp.url() != url {
            if let Err(e) = proposal_info_manager
                .update_canonical_url(url, resp.url())
                .await
            {
                log::warn!("update_canonical_url failed for {}: {}", url, e);
            }
        }
    } else if status == reqwest::StatusCode::NOT_FOUND {
        if let Err(e) = proposal_info_manager.delete_proposal_info(url).await {
            log::warn!("delete_proposal_info failed for {}: {}", url, e);
        }
    } else {
        log::warn!("Got status {} loading straggler {}", status.as_u16(), url);
    }
}

/// Query parameters for `POST /check-stragglers`. `?ndays=N` selects
/// merge proposals not scanned in the last N days; default 5.
#[derive(serde::Deserialize, Default)]
struct CheckStragglersQuery {
    #[serde(default = "default_ndays")]
    ndays: i64,
}

fn default_ndays() -> i64 {
    5
}

/// `POST /check-stragglers`: find merge proposals whose `last_scanned`
/// is older than `ndays` (default 5) and re-scan them in the background.
/// Returns the list of URLs that were queued.
async fn check_stragglers(
    State(state): State<Arc<AppState>>,
    Query(query): Query<CheckStragglersQuery>,
) -> impl IntoResponse {
    let proposal_info_manager =
        crate::proposal_info::ProposalInfoManager::new(state.conn.clone(), state.redis.clone())
            .await;

    let urls = match proposal_info_manager
        .iter_outdated_proposal_info_urls(chrono::Duration::days(query.ndays))
        .await
    {
        Ok(urls) => urls,
        Err(e) => {
            log::error!("Failed to load outdated proposal info URLs: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_response("Database error"),
            );
        }
    };

    // Background scan: rebuild a manager with our own connection
    // pool clone so the spawned task doesn't outlive the request.
    async fn scan(conn: PgPool, redis: Option<redis::aio::ConnectionManager>, urls: Vec<url::Url>) {
        let proposal_info_manager =
            crate::proposal_info::ProposalInfoManager::new(conn, redis).await;
        for url in urls {
            check_straggler(&proposal_info_manager, &url).await;
        }
    }
    tokio::spawn(scan(state.conn.clone(), state.redis.clone(), urls.clone()));

    (StatusCode::OK, Json(serde_json::to_value(&urls).unwrap()))
}

/// `POST /scan`: kick off a background `check_existing` pass over every
/// known merge proposal. Returns 202 immediately.
async fn scan(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    async fn scan(state: Arc<AppState>) {
        crate::check_existing(
            state.conn.clone(),
            state.redis.clone(),
            state.config,
            &state.publish_worker,
            &state.bucket_rate_limiter,
            state.forge_rate_limiter.clone(),
            &state.vcs_managers,
            state.modify_mp_limit,
            state.unexpected_mp_limit,
        )
        .await;
    }

    tokio::spawn(scan(state));
    (StatusCode::ACCEPTED, "Scan started.")
}

/// Form body for `POST /refresh-status`: a urlencoded `url=...`.
#[derive(serde::Deserialize)]
struct RefreshStatusForm {
    url: String,
}

/// `POST /refresh-status`: re-run `check_existing_mp` against a single
/// proposal URL in the background.
async fn refresh_status(
    State(state): State<Arc<AppState>>,
    axum::extract::Form(form): axum::extract::Form<RefreshStatusForm>,
) -> axum::response::Response {
    let url: url::Url = match form.url.parse() {
        Ok(u) => u,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                error_response(&format!("Invalid url parameter: {}", e)),
            )
                .into_response();
        }
    };
    log::info!("Request to refresh proposal status for {}", url);

    async fn scan(state: Arc<AppState>, url: url::Url) {
        // get_proposal_by_url and is_merged/is_closed are PyO3 calls
        // that block synchronously. Move the lookup-and-classify
        // step onto a blocking thread so the tokio worker stays
        // responsive.
        let mp_url = url.clone();
        let mp_status = tokio::task::spawn_blocking(move || {
            let mp = breezyshim::forge::get_proposal_by_url(&mp_url)?;
            let is_merged = mp.is_merged().unwrap_or(false);
            let is_closed = mp.is_closed().unwrap_or(false);
            let status = if is_merged {
                breezyshim::forge::MergeProposalStatus::Merged
            } else if is_closed {
                breezyshim::forge::MergeProposalStatus::Closed
            } else {
                breezyshim::forge::MergeProposalStatus::Open
            };
            Ok::<_, BrzError>((mp, status))
        })
        .await;

        let (mp, status) = match mp_status {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                log::error!("Failed to get merge proposal for {}: {}", url, e);
                return;
            }
            Err(e) => {
                log::error!("Task join error refreshing {}: {}", url, e);
                return;
            }
        };

        match crate::check_existing_mp(
            &state.conn,
            state.redis.clone(),
            state.config,
            &state.publish_worker,
            &mp,
            status,
            &state.vcs_managers,
            &state.bucket_rate_limiter,
            false,
            None,
            None,
        )
        .await
        {
            Ok(_) => {
                log::info!("Refreshed proposal status for {}", url);
            }
            Err(crate::CheckMpError::NoRunForMergeProposal(no_run_url)) => {
                log::warn!(
                    "Unable to find stored metadata for {}, skipping",
                    no_run_url
                );
            }
            Err(crate::CheckMpError::BranchRateLimited { .. }) => {
                log::warn!("Rate-limited accessing {}", url);
            }
            Err(crate::CheckMpError::UnexpectedHttpStatus) => {
                log::warn!("Unexpected HTTP status refreshing {}", url);
            }
            Err(crate::CheckMpError::ForgeLoginRequired) => {
                log::warn!("Forge login required refreshing {}", url);
            }
            Err(crate::CheckMpError::Database(e)) => {
                log::warn!("Database error refreshing proposal {}: {}", url, e);
            }
            Err(crate::CheckMpError::Brz(msg)) => {
                log::warn!("Forge call failed refreshing proposal {}: {}", url, msg);
            }
        }
    }

    tokio::spawn(scan(state.clone(), url));
    (
        StatusCode::ACCEPTED,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        "Refresh of proposal started.",
    )
        .into_response()
}

/// `POST /autopublish`: kick off a background `publish_pending_ready`
/// pass over all publish-ready runs. Returns 202 immediately.
///
/// Optional `?push_limit=N` overrides the deployment's configured limit
/// for this run only, which is useful for smoke-testing a small batch
/// (e.g. `POST /autopublish?push_limit=2`) before letting the publisher
/// work through everything.
#[derive(Debug, Default, serde::Deserialize)]
struct AutopublishQuery {
    #[serde(default)]
    push_limit: Option<usize>,
}

async fn autopublish(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<AutopublishQuery>,
) -> impl IntoResponse {
    let push_limit = query.push_limit.or(state.push_limit);
    let require_binary_diff = state.require_binary_diff;
    let state_for_task = state.clone();
    tokio::spawn(async move {
        if let Err(e) =
            crate::publish_pending_ready(state_for_task, push_limit, require_binary_diff).await
        {
            log::warn!("publish_pending_ready failed: {}", e);
        }
    });
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "message": "Autopublish started.",
            "push_limit": push_limit,
        })),
    )
}

/// `GET /rate-limits/{bucket}`: current open and max-open figures for a
/// single bucket. An unknown bucket returns 200 with
/// `{open: null, max_open: null, remaining: null}`, not 404. Both
/// numbers are read under one lock so they can't drift mid-call.
async fn get_rate_limit(
    State(state): State<Arc<AppState>>,
    Path(bucket): Path<String>,
) -> impl IntoResponse {
    let (current_open, max_open) = match state.bucket_rate_limiter.lock() {
        Ok(limiter) => {
            let stats = limiter.get_stats();
            let current = stats
                .as_ref()
                .and_then(|s| s.per_bucket.get(&bucket).copied());
            let max = limiter.get_max_open(&bucket);
            (current, max)
        }
        Err(e) => {
            error!("Failed to acquire rate limiter lock: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::to_value(crate::api_types::ErrorResponse::internal_server_error())
                        .unwrap(),
                ),
            );
        }
    };

    let remaining = crate::rate_limit_remaining(current_open, max_open);
    (
        StatusCode::OK,
        Json(
            serde_json::to_value(&BucketRateLimit {
                open: current_open,
                max_open,
                remaining,
            })
            .unwrap(),
        ),
    )
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BucketRateLimit {
    open: Option<usize>,
    max_open: Option<usize>,
    remaining: Option<usize>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RateLimitsInfo {
    /// Per-bucket open/max/remaining figures. Field name matches
    /// the JSON key Python emits in
    /// `py/janitor/publish.py::rate_limits_request`.
    proposals_per_bucket: HashMap<String, BucketRateLimit>,
    per_forge: HashMap<String, chrono::DateTime<chrono::Utc>>,
    push_limit: Option<usize>,
}

/// `GET /rate-limits`: full rate-limit picture across all buckets and
/// forges. `proposals_per_bucket` covers every bucket the limiter knows
/// about; `per_forge` is the in-memory forge rate-limit table. Both
/// bucket figures come from the same lock acquisition.
async fn get_all_rate_limits(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let proposals_per_bucket: HashMap<String, BucketRateLimit> = match state
        .bucket_rate_limiter
        .lock()
    {
        Ok(limiter) => {
            let stats = limiter.get_stats();
            let mut out = HashMap::new();
            if let Some(stats) = stats {
                for (bucket, current_open) in stats.per_bucket.iter() {
                    let max_open = limiter.get_max_open(bucket);
                    let remaining = max_open.map(|m| m.saturating_sub(*current_open));
                    out.insert(
                        bucket.clone(),
                        BucketRateLimit {
                            open: Some(*current_open),
                            max_open,
                            remaining,
                        },
                    );
                }
            }
            out
        }
        Err(e) => {
            error!("Failed to acquire rate limiter lock: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::to_value(crate::api_types::ErrorResponse::internal_server_error())
                        .unwrap(),
                ),
            );
        }
    };

    (
        StatusCode::OK,
        Json(
            serde_json::to_value(&RateLimitsInfo {
                proposals_per_bucket,
                per_forge: state
                    .forge_rate_limiter
                    .read()
                    .unwrap()
                    .iter()
                    .map(|(f, t)| (f.to_string(), *t))
                    .collect(),
                push_limit: state.push_limit,
            })
            .unwrap(),
        ),
    )
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Blocker<D> {
    result: bool,
    details: D,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerSuccessDetails {
    result_code: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerInactiveDetails {
    inactive: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerCommandDetails {
    correct: String,
    actual: String,
}

/// Inner value of the `reviews` map in
/// BlockerPublishStatusDetails. Note that the `reviewer` is the
/// outer map key, not a field on this value, and the `reviewed_at`
/// timestamp is serialized as `timestamp` to match
/// py/janitor/publish.py::blockers_request.
#[derive(serde::Serialize, serde::Deserialize)]
struct ReviewSummary {
    timestamp: chrono::DateTime<chrono::Utc>,
    comment: Option<String>,
    verdict: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerPublishStatusDetails {
    status: String,
    reviews: HashMap<String, ReviewSummary>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerBackoffDetails {
    attempt_count: usize,
    next_try_time: chrono::DateTime<chrono::Utc>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerChangeSetDetails {
    change_set_id: String,
    change_set_state: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerPreviousMpDetails {
    url: String,
    status: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BlockerInfo {
    success: Blocker<BlockerSuccessDetails>,
    inactive: Blocker<BlockerInactiveDetails>,
    command: Blocker<BlockerCommandDetails>,
    publish_status: Blocker<BlockerPublishStatusDetails>,
    backoff: Blocker<BlockerBackoffDetails>,
    /// `details` here is shape-flexible because Python emits two
    /// different shapes:
    ///
    ///   * `{"bucket": "..."}` when the bucket is allowed (or the
    ///     run has no bucket at all).
    ///   * `{"open": <usize>, "max_open": <usize>}` when the
    ///     limiter is currently rate-limiting the bucket.
    ///
    /// Match by emitting a serde_json::Value rather than a fixed
    /// struct.
    propose_rate_limit: Blocker<serde_json::Value>,
    change_set: Blocker<BlockerChangeSetDetails>,
    previous_mp: Blocker<Vec<BlockerPreviousMpDetails>>,
}

#[derive(sqlx::FromRow)]
struct RunDetails {
    id: String,
    codebase: String,
    campaign: String,
    finish_time: chrono::DateTime<chrono::Utc>,
    run_command: String,
    publish_status: String,
    rate_limit_bucket: Option<String>,
    revision: Option<breezyshim::RevisionId>,
    policy_command: String,
    result_code: String,
    change_set_state: String,
    change_set: String,
    inactive: bool,
}

/// `GET /blockers/{run_id}`: diagnose why a run isn't being published.
///
/// Each top-level key in the response is a Blocker with a `result`
/// boolean and a `details` object explaining the value:
///
///   * success: result_code = 'success'
///   * inactive: codebase isn't marked inactive
///   * command: candidate.command matches run.command
///   * publish_status: publish_status == 'approved' (with the set
///     of reviewer entries that produced that status)
///   * backoff: now() >= calculate_next_try_time(finish_time,
///     attempt_count)
///   * propose_rate_limit: the bucket isn't currently rate-limited
///     (`details: {bucket}` on success, `details: {open, max_open}`
///     when the limiter says no)
///   * change_set: change_set state is 'publishing' or 'ready'
///   * previous_mp: no previous merge proposal for this codebase +
///     campaign was rejected or plain-closed
///
/// Python deliberately does NOT include a forge_rate_limit blocker
/// (there's a TODO comment on it), so neither do we.
async fn get_blockers(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let run_result = sqlx::query_as::<_, RunDetails>(
        r#"
SELECT
  run.id AS id,
  run.codebase AS codebase,
  run.suite::text AS campaign,
  (run.finish_time AT TIME ZONE 'UTC') AS finish_time,
  run.command AS run_command,
  run.publish_status::text AS publish_status,
  named_publish_policy.rate_limit_bucket AS rate_limit_bucket,
  run.revision AS revision,
  candidate.command AS policy_command,
  run.result_code AS result_code,
  change_set.state::text AS change_set_state,
  change_set.id AS change_set,
  codebase.inactive AS inactive
FROM run
INNER JOIN codebase ON codebase.name = run.codebase
INNER JOIN candidate ON candidate.codebase = run.codebase AND candidate.suite = run.suite
INNER JOIN named_publish_policy ON candidate.publish_policy = named_publish_policy.name
INNER JOIN change_set ON change_set.id = run.change_set
WHERE run.id = $1
"#,
    )
    .bind(&id)
    .fetch_optional(&state.conn)
    .await;

    let run = match run_result {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(
                    serde_json::to_value(NotFoundResponse {
                        reason: "No such publish-ready run".to_string(),
                        run_id: Some(id),
                        name: None,
                        url: None,
                        id: None,
                        bucket: None,
                        campaign: None,
                        codebase: None,
                    })
                    .unwrap(),
                ),
            );
        }
        Err(e) => {
            log::error!("Failed to load run {} for blockers: {}", id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_response("Database error"),
            );
        }
    };

    #[derive(sqlx::FromRow)]
    struct ReviewRow {
        reviewer: String,
        reviewed_at: chrono::DateTime<chrono::Utc>,
        comment: Option<String>,
        verdict: String,
    }

    let reviews = match sqlx::query_as::<_, ReviewRow>(
        "SELECT reviewer, reviewed_at, comment, verdict::text FROM review WHERE run_id = $1",
    )
    .bind(&id)
    .fetch_all(&state.conn)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            log::error!("Failed to load reviews for run {}: {}", id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_response("Database error"),
            );
        }
    };

    let attempt_count = if let Some(revision) = run.revision.as_ref() {
        match crate::state::get_publish_attempt_count(
            &state.conn,
            revision,
            &["differ-unreachable"],
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                log::error!("Failed to load publish attempt count for run {}: {}", id, e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error_response("Database error"),
                );
            }
        }
    } else {
        0
    };

    let last_mps =
        match crate::state::get_previous_mp_status(&state.conn, &run.codebase, &run.campaign).await
        {
            Ok(rows) => rows,
            Err(e) => {
                log::error!(
                    "Failed to load previous MP status for {}/{}: {}",
                    run.codebase,
                    run.campaign,
                    e
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    error_response("Database error"),
                );
            }
        };

    let success = Blocker {
        result: run.result_code == "success",
        details: BlockerSuccessDetails {
            result_code: run.result_code,
        },
    };

    let inactive = Blocker {
        result: !run.inactive,
        details: BlockerInactiveDetails {
            inactive: run.inactive,
        },
    };

    let command = Blocker {
        result: run.run_command == run.policy_command,
        details: BlockerCommandDetails {
            correct: run.policy_command,
            actual: run.run_command,
        },
    };

    let publish_status = Blocker {
        result: run.publish_status == "approved",
        details: BlockerPublishStatusDetails {
            status: run.publish_status,
            reviews: reviews
                .into_iter()
                .map(|row| {
                    (
                        row.reviewer,
                        ReviewSummary {
                            timestamp: row.reviewed_at,
                            comment: row.comment,
                            verdict: row.verdict,
                        },
                    )
                })
                .collect(),
        },
    };

    let next_try_time = crate::calculate_next_try_time(run.finish_time, attempt_count);

    let backoff = Blocker {
        result: chrono::Utc::now() >= next_try_time,
        details: BlockerBackoffDetails {
            attempt_count,
            next_try_time,
        },
    };

    // propose_rate_limit: take the limiter lock once and read every
    // value we need from it before releasing.
    let propose_rate_limit = match state.bucket_rate_limiter.lock() {
        Ok(limiter) => {
            if let Some(bucket) = run.rate_limit_bucket.as_deref() {
                let allowed = limiter.check_allowed(bucket).is_allowed();
                if allowed {
                    Blocker {
                        result: true,
                        details: serde_json::json!({"bucket": bucket}),
                    }
                } else {
                    let stats = limiter.get_stats();
                    let open = stats
                        .as_ref()
                        .and_then(|s| s.per_bucket.get(bucket).copied());
                    let max_open = limiter.get_max_open(bucket);
                    Blocker {
                        result: false,
                        details: serde_json::json!({
                            "open": open,
                            "max_open": max_open,
                        }),
                    }
                }
            } else {
                // No bucket configured: treat as unconstrained.
                Blocker {
                    result: true,
                    details: serde_json::json!({"bucket": serde_json::Value::Null}),
                }
            }
        }
        Err(e) => {
            log::error!("Failed to acquire rate limiter lock: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                error_response("Rate limiter unavailable"),
            );
        }
    };

    let change_set = Blocker {
        result: crate::change_set_state_allows_publish(&run.change_set_state),
        details: BlockerChangeSetDetails {
            change_set_id: run.change_set,
            change_set_state: run.change_set_state,
        },
    };

    let previous_mp = Blocker {
        // The blocker is "true" (i.e. not blocking publication) when
        // none of the prior MP statuses block. Reuse the same
        // predicate consider_publish_run uses, so the two paths
        // can't drift.
        result: !crate::previous_mp_blocks_publish(&last_mps),
        details: last_mps
            .into_iter()
            .map(|(url, status)| BlockerPreviousMpDetails { url, status })
            .collect(),
    };

    (
        StatusCode::OK,
        Json(
            serde_json::to_value(&BlockerInfo {
                success,
                previous_mp,
                change_set,
                inactive,
                command,
                publish_status,
                backoff,
                propose_rate_limit,
            })
            .unwrap(),
        ),
    )
}

/// Create the web application router with all routes.
pub fn app(state: Arc<AppState>) -> Router {
    // axum 0.8 requires `{name}` for captures; `:name` panics at route-
    // registration time ("Path segments must not start with `:`"). The
    // runner/archive routers were already migrated; migrate publish.
    Router::new()
        .route(
            "/{campaign}/merge-proposals",
            get(get_merge_proposals_by_campaign),
        )
        .route(
            "/c/{codebase}/merge-proposals",
            get(get_merge_proposals_by_codebase),
        )
        .route("/merge-proposals", get(list_merge_proposals))
        .route("/absorbed", get(absorbed))
        .route("/policy/{name}", get(get_policy))
        .route("/policy", get(get_policies))
        .route("/policy/{name}", put(put_policy))
        .route("/policy", put(put_policies))
        .route("/merge-proposal", post(update_merge_proposal))
        .route("/policy/{name}", delete(delete_policy))
        .route("/consider/{id}", post(consider))
        .route("/publish/{id}", get(get_publish_by_id))
        .route("/{campaign}/{codebase}/publish", post(publish))
        .route("/credentials", get(get_credentials))
        .route(
            "/health",
            get(health::health_check_handler::<Arc<AppState>>),
        )
        .route("/ready", get(health::readiness_handler::<Arc<AppState>>))
        .route("/scan", post(scan))
        .route("/check-stragglers", post(check_stragglers))
        .route("/refresh-status", post(refresh_status))
        .route("/autopublish", post(autopublish))
        .route("/rate-limits/{bucket}", get(get_rate_limit))
        .route("/rate-limits", get(get_all_rate_limits))
        .route("/blockers/{id}", get(get_blockers))
        .route("/metrics", get(metrics_handler))
        .layer(axum::middleware::from_fn(
            crate::middleware::metrics_middleware,
        ))
        .with_state(state)
}

async fn metrics_handler() -> impl IntoResponse {
    use axum::http::header;
    use prometheus::{Encoder, TextEncoder};
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    match encoder.encode(&metric_families, &mut buffer) {
        Ok(_) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, encoder.format_type())],
            buffer,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("metrics error: {e}"),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_mp_status_transition, project_per_branch_policy, MpStatusTransition,
        PerBranchPolicy, PolicyDocument,
    };
    use std::collections::HashMap;

    // --- classify_mp_status_transition ---

    /// Closed -> closed in any combination is a no-op update.
    /// CLOSED_STATUSES is `[closed, abandoned, rejected, applied]`,
    /// so all 16 ordered pairs should classify as NoOpUpdate.
    #[test]
    fn test_classify_mp_status_transition_closed_to_closed() {
        let closed = ["closed", "abandoned", "rejected", "applied"];
        for from in &closed {
            for to in &closed {
                assert_eq!(
                    classify_mp_status_transition(from, to),
                    MpStatusTransition::NoOpUpdate,
                    "expected NoOpUpdate for {} -> {}",
                    from,
                    to,
                );
            }
        }
    }

    /// open -> any closed status is a real forge action.
    #[test]
    fn test_classify_mp_status_transition_open_to_closed() {
        for to in &["closed", "abandoned", "rejected", "applied"] {
            assert_eq!(
                classify_mp_status_transition("open", to),
                MpStatusTransition::CloseOnForge,
                "expected CloseOnForge for open -> {}",
                to,
            );
        }
    }

    /// closed -> open is forbidden - the publisher cannot reopen
    /// proposals.
    #[test]
    fn test_classify_mp_status_transition_closed_to_open_forbidden() {
        for from in &["closed", "abandoned", "rejected", "applied"] {
            assert_eq!(
                classify_mp_status_transition(from, "open"),
                MpStatusTransition::Forbidden,
                "expected Forbidden for {} -> open",
                from,
            );
        }
    }

    /// open -> open is forbidden too - there's no DB or forge action
    /// the operator could be requesting.
    #[test]
    fn test_classify_mp_status_transition_open_to_open_forbidden() {
        assert_eq!(
            classify_mp_status_transition("open", "open"),
            MpStatusTransition::Forbidden
        );
    }

    /// Unknown status strings are classified as Forbidden, never
    /// silently coerced to one of the known buckets.
    #[test]
    fn test_classify_mp_status_transition_unknown_status_forbidden() {
        // Unknown current
        assert_eq!(
            classify_mp_status_transition("merged", "closed"),
            MpStatusTransition::Forbidden
        );
        // Unknown new
        assert_eq!(
            classify_mp_status_transition("open", "merged"),
            MpStatusTransition::Forbidden
        );
        // Both unknown
        assert_eq!(
            classify_mp_status_transition("foo", "bar"),
            MpStatusTransition::Forbidden
        );
        // Empty strings
        assert_eq!(
            classify_mp_status_transition("", "closed"),
            MpStatusTransition::Forbidden
        );
    }

    // --- project_per_branch_policy ---

    /// Empty document projects to three empty vectors. Used by the
    /// SQL composite-array insert; an empty document is valid (a
    /// policy with no per-branch entries).
    #[test]
    fn test_project_per_branch_policy_empty() {
        let doc = PolicyDocument::default();
        let (roles, modes, freqs) = project_per_branch_policy(&doc);
        assert_eq!(roles, Vec::<String>::new());
        assert_eq!(modes, Vec::<String>::new());
        assert_eq!(freqs, Vec::<Option<i32>>::new());
    }

    /// Single-entry document projects to one-element parallel
    /// vectors with the field values intact.
    #[test]
    fn test_project_per_branch_policy_single_entry() {
        let mut per_branch = HashMap::new();
        per_branch.insert(
            "main".to_string(),
            PerBranchPolicy {
                mode: "propose".to_string(),
                max_frequency_days: Some(7),
            },
        );
        let doc = PolicyDocument {
            rate_limit_bucket: None,
            per_branch,
        };
        let (roles, modes, freqs) = project_per_branch_policy(&doc);
        assert_eq!(roles, vec!["main".to_string()]);
        assert_eq!(modes, vec!["propose".to_string()]);
        assert_eq!(freqs, vec![Some(7)]);
    }

    /// max_frequency_days=None must round-trip as None, not 0 or
    /// silently dropped.
    #[test]
    fn test_project_per_branch_policy_none_frequency() {
        let mut per_branch = HashMap::new();
        per_branch.insert(
            "main".to_string(),
            PerBranchPolicy {
                mode: "push".to_string(),
                max_frequency_days: None,
            },
        );
        let doc = PolicyDocument {
            rate_limit_bucket: None,
            per_branch,
        };
        let (roles, modes, freqs) = project_per_branch_policy(&doc);
        assert_eq!(roles, vec!["main".to_string()]);
        assert_eq!(modes, vec!["push".to_string()]);
        assert_eq!(freqs, vec![None]);
    }

    /// Multi-entry document: vectors must stay parallel - index i
    /// in `roles` aligns with index i in `modes` and `freqs`. Verify
    /// by sorting on role and checking the projection.
    #[test]
    fn test_project_per_branch_policy_multi_entry_parallel() {
        let mut per_branch = HashMap::new();
        per_branch.insert(
            "main".to_string(),
            PerBranchPolicy {
                mode: "propose".to_string(),
                max_frequency_days: Some(7),
            },
        );
        per_branch.insert(
            "aux".to_string(),
            PerBranchPolicy {
                mode: "push".to_string(),
                max_frequency_days: None,
            },
        );
        let doc = PolicyDocument {
            rate_limit_bucket: Some("bucket-a".to_string()),
            per_branch,
        };
        let (roles, modes, freqs) = project_per_branch_policy(&doc);
        assert_eq!(roles.len(), 2);
        assert_eq!(modes.len(), 2);
        assert_eq!(freqs.len(), 2);

        // Re-pair and sort by role to check parallelism without
        // depending on HashMap iteration order.
        let mut paired: Vec<(String, String, Option<i32>)> = roles
            .into_iter()
            .zip(modes.into_iter())
            .zip(freqs.into_iter())
            .map(|((r, m), f)| (r, m, f))
            .collect();
        paired.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            paired,
            vec![
                ("aux".to_string(), "push".to_string(), None),
                ("main".to_string(), "propose".to_string(), Some(7)),
            ]
        );
    }
}
