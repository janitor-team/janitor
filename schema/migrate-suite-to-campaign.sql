-- Rename the suite columns of run, queue and candidate to campaign.
-- Apply once to databases created from a schema/state.sql that predates the rename.
BEGIN;

DROP VIEW absorbed_runs, change_set_unpublished, change_set_todo, publish_ready, queue_positions, first_run_time, perpetual_candidates, campaigns, last_unabsorbed_runs, last_effective_runs, last_runs;

ALTER TABLE run RENAME COLUMN suite TO campaign;
ALTER TABLE run ALTER COLUMN campaign TYPE campaign_name;
ALTER TABLE queue RENAME COLUMN suite TO campaign;
ALTER TABLE queue ALTER COLUMN campaign TYPE campaign_name;
ALTER TABLE candidate RENAME COLUMN suite TO campaign;
ALTER TABLE candidate ALTER COLUMN campaign TYPE campaign_name;
DROP DOMAIN suite_name;

-- PostgreSQL 18 and later name NOT NULL constraints after the column.
DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['run', 'queue', 'candidate'] LOOP
        IF EXISTS (SELECT FROM pg_constraint WHERE conname = t || '_suite_not_null') THEN
            EXECUTE format('ALTER TABLE %I RENAME CONSTRAINT %I TO %I',
                           t, t || '_suite_not_null', t || '_campaign_not_null');
        END IF;
    END LOOP;
END;
$$;

ALTER INDEX IF EXISTS run_codebase_suite_start_time_idx RENAME TO run_codebase_campaign_start_time_idx;
ALTER INDEX IF EXISTS run_suite_start_time_idx RENAME TO run_campaign_start_time_idx;
ALTER INDEX IF EXISTS run_codebase_suite_idx RENAME TO run_codebase_campaign_idx;
ALTER INDEX IF EXISTS run_suite_idx RENAME TO run_campaign_idx;
ALTER INDEX IF EXISTS run_suite_revision_publish_status_idx RENAME TO run_campaign_revision_publish_status_idx;
ALTER INDEX IF EXISTS run_codebase_suite_start_time_result_code_idx RENAME TO run_codebase_campaign_start_time_result_code_idx;
ALTER INDEX IF EXISTS queue_codebase_suite_set RENAME TO queue_codebase_campaign_set;
ALTER INDEX IF EXISTS queue_suite_priority_id_idx RENAME TO queue_campaign_priority_id_idx;
ALTER INDEX IF EXISTS candidate_codebase_suite_set RENAME TO candidate_codebase_campaign_set;
ALTER INDEX IF EXISTS candidate_suite_idx RENAME TO candidate_campaign_idx;
ALTER INDEX IF EXISTS candidate_codebase_suite_coalesce_idx RENAME TO candidate_codebase_campaign_coalesce_idx;

CREATE OR REPLACE FUNCTION refresh_last_run(run_id text)
  RETURNS void
  LANGUAGE PLPGSQL
  AS $$
    DECLARE row RECORD;
    BEGIN

    SELECT codebase, campaign INTO row FROM run WHERE id = run_id;
    IF FOUND THEN
        perform refresh_last_run(row.codebase, row.campaign);
    END IF;
    END;
$$;

CREATE OR REPLACE FUNCTION refresh_last_run(_codebase text, _campaign text)
  RETURNS void
  LANGUAGE PLPGSQL
  AS $$
    DECLARE last_run RECORD;
    DECLARE last_effective_run RECORD;
    DECLARE last_unabsorbed_run RECORD;
    DECLARE last_run_id TEXT;
    DECLARE last_effective_run_id TEXT;
    DECLARE last_effective_run_result_code TEXT;
    DECLARE last_unabsorbed_run_id TEXT;

    BEGIN
    SELECT id, result_code, failure_transient, resume_from INTO last_run FROM run WHERE run.codebase = _codebase AND campaign = _campaign ORDER BY start_time DESC LIMIT 1;
    IF FOUND THEN
        last_run_id := last_run.id;
    ELSE
        DELETE FROM last_run WHERE codebase = _codebase AND campaign = _campaign;
        RETURN;
    END IF;

    IF last_run.result_code = 'nothing-new-to-do' THEN
        last_effective_run_id := last_run.resume_from;
        last_effective_run_result_code := 'success';
    ELSIF last_run.failure_transient IS TRUE THEN
        SELECT id, result_code INTO last_effective_run FROM run WHERE run.codebase = _codebase AND run.campaign = _campaign AND result_code != 'nothing-new-to-do' AND not coalesce(failure_transient, False) ORDER BY start_time DESC limit 1;
        IF FOUND THEN
           last_effective_run_id := last_effective_run.id;
           last_effective_run_result_code := last_effective_run.result_code;
        ELSE
           last_effective_run_id := NULL;
           last_effective_run_result_code := NULL;
        END IF;
    ELSE
        last_effective_run_id := last_run.id;
        last_effective_run_result_code := last_run.result_code;
    END IF;

    IF last_effective_run_result_code = 'nothing-to-do' THEN
        last_unabsorbed_run_id := NULL;
    ELSIF last_effective_run_result_code != 'success' THEN
        last_unabsorbed_run_id := last_effective_run_id;
    ELSE
       PERFORM from new_result_branch WHERE run_id = last_effective_run_id and not absorbed;
       if FOUND then
           last_unabsorbed_run_id := last_effective_run_id;
       else
          last_unabsorbed_run_id := null;
       end if;
     END IF;

    INSERT INTO last_run (codebase, campaign, last_run_id, last_effective_run_id, last_unabsorbed_run_id) VALUES (
          _codebase, _campaign, last_run.id, last_effective_run_id, last_unabsorbed_run_id)
         ON CONFLICT (codebase, campaign) DO UPDATE SET last_run_id = EXCLUDED.last_run_id, last_effective_run_id = EXCLUDED.last_effective_run_id, last_unabsorbed_run_id = EXCLUDED.last_unabsorbed_run_id;
    END;
$$;

CREATE OR REPLACE FUNCTION run_trigger_refresh_last_run()
  RETURNS TRIGGER
  LANGUAGE PLPGSQL
  AS $$
    DECLARE row RECORD;
    BEGIN
    -- Checking the Operation Type
    IF (TG_OP = 'DELETE') THEN
      row = OLD;
    ELSE
      row = NEW;
    END IF;

    PERFORM refresh_last_run(row.codebase, row.campaign::text);
    RETURN NEW;
    END;
$$;

CREATE OR REPLACE VIEW last_runs AS
  SELECT
  run.*
  FROM last_run
  INNER JOIN run on last_run.last_run_id = run.id;

CREATE OR REPLACE VIEW last_effective_runs AS
  SELECT
  run.*
  FROM last_run
  INNER JOIN run on last_run.last_effective_run_id = run.id;

CREATE OR REPLACE VIEW last_unabsorbed_runs AS
  SELECT
     run.*
  FROM last_run
  INNER JOIN run on last_run.last_unabsorbed_run_id = run.id;

create or replace view campaigns as select distinct campaign as name from run;

CREATE OR REPLACE VIEW perpetual_candidates AS
  select campaign, codebase from candidate union select campaign, codebase from run;

CREATE OR REPLACE VIEW first_run_time AS
 SELECT DISTINCT ON (run.codebase, run.campaign) run.codebase, run.campaign, run.start_time
 FROM run ORDER BY run.codebase, run.campaign;

CREATE OR REPLACE VIEW queue_positions AS SELECT
    id,
    codebase,
    campaign,
    row_number() OVER (ORDER BY bucket ASC, priority ASC, id ASC) AS position,
    SUM(estimated_duration) OVER (ORDER BY bucket ASC, priority ASC, id ASC)
        - coalesce(estimated_duration, interval '0') AS wait_time
FROM
    queue
ORDER BY bucket ASC, priority ASC, id ASC;

CREATE OR REPLACE VIEW publish_ready AS
WITH publishable AS (
  SELECT
  run.id AS id,
  run.command AS command,
  run.start_time AS start_time,
  run.finish_time AS finish_time,
  run.finish_time - run.start_time AS duration,
  run.description AS description,
  run.result_code AS result_code,
  run.main_branch_revision AS main_branch_revision,
  run.revision AS revision,
  run.context AS context,
  run.result AS result,
  run.campaign AS campaign,
  run.instigated_context AS instigated_context,
  run.vcs_type AS vcs_type,
  run.branch_url AS branch_url,
  run.logfilenames AS logfilenames,
  run.worker AS worker,
  array(SELECT ROW(role, remote_name, base_revision, revision)::result_branch FROM new_result_branch WHERE new_result_branch.run_id = run.id) as result_branches,
  run.result_tags AS result_tags,
  run.value AS value,
  candidate.command AS policy_command,
  named_publish_policy.name AS publish_policy_name,
  named_publish_policy.rate_limit_bucket AS rate_limit_bucket,
  run.publish_status AS publish_status,
  ARRAY(
   SELECT row(rb.role, remote_name, base_revision, revision, mode, frequency_days)::result_branch_with_policy
   FROM new_result_branch rb
    LEFT JOIN UNNEST(named_publish_policy.per_branch_policy) pp ON pp.role = rb.role
   WHERE rb.run_id = run.id AND not COALESCE(absorbed, False)
   ORDER BY rb.role != 'main' DESC
  ) AS unpublished_branches,
  target_branch_url,
  run.change_set AS change_set,
  change_set.state AS change_set_state,
  run.failure_transient AS failure_transient,
  run.failure_stage AS failure_stage,
  run.codebase AS codebase
FROM
  last_effective_runs AS run
INNER JOIN candidate ON
    candidate.codebase = run.codebase AND candidate.campaign = run.campaign
INNER JOIN named_publish_policy ON
    candidate.publish_policy = named_publish_policy.name
INNER JOIN change_set ON change_set.id = run.change_set
WHERE
  result_code = 'success')
SELECT * FROM publishable WHERE ARRAY_LENGTH(unpublished_branches, 1) > 0;

CREATE OR REPLACE VIEW change_set_todo AS
  SELECT * FROM candidate WHERE change_set is not NULL AND NOT EXISTS (
        SELECT FROM last_effective_runs WHERE
                change_set = candidate.change_set AND
                codebase = candidate.codebase AND
                campaign = candidate.campaign AND
                result_code in ('success', 'nothing-to-do'));

CREATE OR REPLACE VIEW change_set_unpublished AS
  SELECT change_set, last_unabsorbed_runs.id, new_result_branch.role FROM last_unabsorbed_runs
  INNER JOIN new_result_branch ON new_result_branch.run_id = last_unabsorbed_runs.id
  WHERE not coalesce(new_result_branch.absorbed, False) and result_code = 'success';

CREATE OR REPLACE VIEW absorbed_runs AS
    SELECT
       'propose' AS mode,
       run.change_set,
       run.codebase,
       merge_proposal.merged_at - run.finish_time as delay,
       run.campaign AS campaign,
       run.result::jsonb AS result,
       run.id,
       merge_proposal.merged_at AS absorbed_at,
       merge_proposal.merged_by,
       merge_proposal.url AS merge_proposal_url,
       run.revision
    FROM merge_proposal
    INNER JOIN run ON merge_proposal.revision = run.revision
    WHERE run.result_code = 'success'
    AND run.campaign not in ('unchanged', 'control')
 UNION
    SELECT
        'push' AS mode,
        run.change_set,
       run.codebase,
        publish.timestamp - run.finish_time AS delay,
        run.campaign AS campaign,
        run.result::jsonb AS result,
        run.id, timestamp AS absorbed_at,
        NULL AS merged_by,
        NULL AS merge_proposal_url,
        run.revision
    FROM publish
    INNER JOIN run ON publish.revision = run.revision
    WHERE mode = 'push'
    AND run.result_code = 'success'
    AND publish.result_code = 'success'
    AND run.campaign not in ('unchanged', 'control');

COMMIT;
