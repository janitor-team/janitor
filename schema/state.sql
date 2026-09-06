CREATE TYPE vcs_type AS ENUM('bzr', 'git', 'svn', 'mtn', 'hg', 'arch', 'cvs', 'darcs');
CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE DOMAIN codebase_name AS TEXT check (value similar to '[a-z0-9][a-z0-9+-.]+');
CREATE TABLE IF NOT EXISTS codebase (
   -- Name is intentionally optional
   name codebase_name,
   branch_url text, -- DEPRECATED
   url text,
   branch text,
   -- the subpath may be unknown; it should be an empty string if it's the root
   -- path.
   subpath text,
   -- last revision, if known
   vcs_last_revision text,
   last_scanned timestamp,

   web_url text,

   -- vcs type, if known
   vcs_type vcs_type,
   value int,
   inactive boolean not null default false,
   hostname text generated always as (substring(branch_url, '.*://(?:[^/@]*@)?([^/]*)'::text)) stored,
   unique(branch_url, subpath),
   unique(name),
   check(name is not null or branch_url is not null),
   check((branch_url is null) = (url is null))
);
CREATE INDEX ON codebase (branch_url);
CREATE INDEX ON codebase (name);

CREATE TYPE merge_proposal_status AS ENUM ('open', 'closed', 'merged', 'applied', 'abandoned', 'rejected');
CREATE TABLE IF NOT EXISTS merge_proposal (
   codebase text references codebase(name) on delete set null,
   url text not null,
   target_branch_url text,
   status merge_proposal_status NULL DEFAULT NULL,
   revision text,
   merged_by text,
   merged_at timestamp,
   last_scanned timestamp,
   can_be_merged boolean,
   rate_limit_bucket text,
   primary key(url)
);
CREATE INDEX ON merge_proposal (revision);
CREATE INDEX ON merge_proposal (url);
CREATE INDEX ON merge_proposal (status);
CREATE INDEX ON merge_proposal (status, rate_limit_bucket);
CREATE DOMAIN suite_name AS TEXT check (value similar to '[a-z0-9][a-z0-9+-.]+');
CREATE DOMAIN campaign_name AS TEXT check (value similar to '[a-z0-9][a-z0-9+-.]+');
CREATE TYPE verdict AS ENUM('approved', 'rejected', 'abstained');
CREATE TABLE result_tag (
 actual_name text,
 revision text not null
);

CREATE TABLE IF NOT EXISTS worker (
   name text not null unique,
   password text not null,
   link text
);

CREATE TYPE change_set_state AS ENUM ('created', 'working', 'ready', 'publishing', 'done');

CREATE TABLE IF NOT EXISTS change_set (
  id text not null primary key,
  campaign campaign_name not null,
  state change_set_state default 'created' not null
);

CREATE INDEX ON result_tag (revision);
CREATE TYPE publish_status AS ENUM ('unknown', 'blocked', 'needs-manual-review', 'rejected', 'approved', 'ignored');
CREATE TABLE IF NOT EXISTS run (
   id text not null primary key,
   command text,
   description text,
   start_time timestamp,
   finish_time timestamp,
   -- Disabled for now: requires postgresql > 12
   duration interval generated always as (finish_time - start_time) stored,
   result_code text not null,
   instigated_context text,
   -- Some codemod-specific indication of what we attempted to do
   context text,
   -- Main branch revision
   main_branch_revision text,
   revision text,
   result json,
   suite suite_name not null, -- DEPRECATED
   vcs_type vcs_type,
   branch_url text,
   logfilenames text[] not null,
   publish_status publish_status not null default 'unknown',
   value integer,
   -- Name of the worker that executed this run.
   worker text references worker(name),
   worker_link text,
   result_tags result_tag[],
   subpath text,
   -- Failure stage, if this run failed
   failure_stage text,
   -- Failure details, if this run failed
   failure_details json,
   target_branch_url text,
   failure_transient boolean,
   -- The run this one resumed from
   resume_from text references run (id) on delete set null,
   change_set text not null references change_set(id),
   codebase text not null references codebase(name),
   check(finish_time >= start_time),
   -- nothing-new-to-do always requires resume_from
   check(result_code != 'nothing-new-to-do' or resume_from is not null),
   check(publish_status != 'approved' or revision is not null)
);
CREATE INDEX ON run (codebase, suite, start_time DESC);
CREATE INDEX ON run (start_time);
CREATE INDEX ON run (suite, start_time);
CREATE INDEX ON run (codebase, suite);
CREATE INDEX ON run (suite);
CREATE INDEX ON run (result_code);
CREATE INDEX ON run (revision);
CREATE INDEX ON run (main_branch_revision);
CREATE INDEX ON run (change_set);
CREATE INDEX ON run (publish_status);
CREATE TYPE publish_mode AS ENUM('push', 'attempt-push', 'propose', 'build-only', 'push-derived', 'skip', 'bts');
CREATE TYPE review_policy AS ENUM('not-required', 'required');
CREATE TABLE IF NOT EXISTS publish (
   id text not null,
   change_set text not null references change_set(id),
   source_branch_url text,
   source_branch_web_url text,
   target_branch_url text,
   target_branch_web_url text,
   subpath text,
   branch_name text,
   main_branch_revision text,
   revision text,
   role text,
   mode publish_mode not null,
   merge_proposal_url text references merge_proposal(url),
   result_code text not null,
   description text,
   requester text,
   codebase text references codebase(name) on delete set null,
   timestamp timestamp default now()
);
CREATE INDEX ON publish (revision);
CREATE INDEX ON publish (merge_proposal_url);
CREATE INDEX ON publish (timestamp);
CREATE TYPE queue_bucket AS ENUM(
    'update-existing-mp', 'manual', 'control', 'hook', 'reschedule', 'update-new-mp', 'missing-deps', 'default');
CREATE TABLE IF NOT EXISTS queue (
   id serial,
   bucket queue_bucket not null default 'default',
   codebase text not null references codebase(name) on delete cascade,
   branch_url text,
   suite suite_name not null,
   command text,
   priority bigint default 0 not null,
   -- Some codemod-specific indication of what we are expecting to do.
   context text,
   estimated_duration interval,
   refresh boolean default false,
   requester text,
   change_set text references change_set(id) on delete cascade,
   check (command != '')
);
CREATE UNIQUE INDEX queue_codebase_suite_set ON queue(codebase, suite, coalesce(change_set, ''));
CREATE INDEX ON queue (change_set);
CREATE INDEX ON queue (priority ASC, id ASC);
CREATE INDEX ON queue (bucket ASC, priority ASC, id ASC);
CREATE TABLE IF NOT EXISTS branch_publish_policy (
   role text not null,
   mode publish_mode default 'build-only',
   frequency_days int,
   unique(role)
);
CREATE TABLE IF NOT EXISTS named_publish_policy (
   name text not null primary key,
   per_branch_policy branch_publish_policy[],
   rate_limit_bucket text
);

CREATE TABLE IF NOT EXISTS candidate (
   suite suite_name not null,
   context text,
   value integer,
   success_chance float,
   command text not null,
   publish_policy text references named_publish_policy (name),
   change_set text references change_set(id) on delete cascade,
   codebase text not null,
   id serial primary key not null,
   check (command != ''),
   check (value > 0),
   constraint candidate_codebase_fkey foreign key(codebase) references codebase(name) on delete cascade
);
CREATE UNIQUE INDEX candidate_codebase_suite_set ON candidate (codebase, suite, coalesce(change_set, ''));
CREATE INDEX ON candidate (suite);
CREATE INDEX ON candidate(change_set);

CREATE TABLE last_run (
   codebase text not null references codebase(name),
   campaign campaign_name not null,
   last_run_id text references run (id),
   last_effective_run_id text references run (id),
   last_unabsorbed_run_id text references run (id),
   unique (codebase, campaign)
);



-- The last run per codebase/campaign
CREATE OR REPLACE VIEW last_runs AS
  SELECT
  run.*
  FROM last_run
  INNER JOIN run on last_run.last_run_id = run.id;

-- The last effective run per codebase/campaign; i.e. the last run that
-- wasn't an attempt to incrementally improve things that yielded no new
-- changes.
CREATE OR REPLACE VIEW last_effective_runs AS
  SELECT
  run.*
  FROM last_run
  INNER JOIN run on last_run.last_effective_run_id = run.id;

CREATE TABLE new_result_branch (
 run_id text not null references run (id) on delete cascade,
 role text not null,
 remote_name text,
 base_revision text,
 revision text,
 absorbed boolean default false,
 UNIQUE(run_id, role)
);

CREATE INDEX ON new_result_branch (revision);
CREATE INDEX ON new_result_branch (absorbed);

CREATE OR REPLACE FUNCTION refresh_last_run(run_id text)
  RETURNS void
  LANGUAGE PLPGSQL
  AS $$
    DECLARE row RECORD;
    BEGIN

    SELECT codebase, suite INTO row FROM run WHERE id = run_id;
    IF FOUND THEN
        perform refresh_last_run(row.codebase, row.suite);
    END IF;
    END;
$$;

-- Triggered when:
-- - New successful publish created
-- - New run created
-- - Result branch changed to 'absorbed'

CREATE OR REPLACE FUNCTION refresh_change_set_state(change_set_id text)
  RETURNS text
  LANGUAGE PLPGSQL
  AS $$
    DECLARE row RECORD;
    DECLARE _state change_set_state;
    BEGIN
    SELECT change_set.state INTO STRICT row FROM change_set WHERE id = change_set_id;
    _state := row.state;
    IF _state = 'created' THEN
       PERFORM FROM run WHERE change_set = change_set_id;
       IF FOUND THEN
          _state := 'working';
       END IF;
    END IF;
    IF _state = 'working' THEN
       PERFORM FROM run WHERE change_set = change_set_id AND result_code = 'success';
       IF FOUND THEN
           PERFORM FROM change_set_todo WHERE change_set = change_set_id;
           IF NOT FOUND THEN
               _state := 'ready';
           END IF;
        END IF;
    END IF;
    IF _state = 'ready' THEN
       PERFORM FROM publish WHERE result_code = 'success' AND change_set = change_set_id;
       IF FOUND THEN
           _state := 'publishing';
       END IF;
    END IF;
    IF _state = 'publishing' THEN
       PERFORM FROM change_set_unpublished WHERE change_set = change_set_id;
       IF NOT FOUND THEN
          _state := 'done';
       END IF;
    END IF;
    IF row.state != _state THEN
        UPDATE change_set SET state = _state WHERE id = change_set_id;
    END IF;
    RETURN _state;
    END;
$$;

CREATE OR REPLACE FUNCTION new_result_branch_trigger_refresh_change_set_state()
  RETURNS TRIGGER
  LANGUAGE PLPGSQL
  AS $$
    DECLARE change_set_id TEXT;
    BEGIN

    if (TG_OP = 'INSERT' AND NEW.absorbed) then
        SELECT change_set INTO STRICT change_set_id FROM run WHERE id = OLD.run_id;
        perform refresh_change_set_state(change_set_id);
    end if;

    if (TG_OP = 'UPDATE' AND NEW.absorbed != OLD.absorbed) then
        SELECT change_set INTO STRICT change_set_id FROM run WHERE id = OLD.run_id;
        perform refresh_change_set_state(change_set_id);
        IF old.run_id != new.run_id THEN
            SELECT change_set INTO STRICT change_set_id FROM run WHERE id = NEW.run_id;
            perform refresh_change_set_state(change_set_id);
        END IF;
    end if;

    RETURN NEW;
    END;
$$;

CREATE OR REPLACE TRIGGER new_result_branch_refresh_change_set_state
  AFTER INSERT OR UPDATE OR DELETE
  ON new_result_branch
  FOR EACH ROW
  EXECUTE FUNCTION new_result_branch_trigger_refresh_change_set_state();

CREATE OR REPLACE FUNCTION new_result_branch_trigger_refresh_last_run()
  RETURNS TRIGGER
  LANGUAGE PLPGSQL
  AS $$
    BEGIN

    if (TG_OP = 'INSERT' AND NEW.absorbed) then
        perform refresh_last_run(new.run_id);
    end if;

    if (TG_OP = 'UPDATE' AND NEW.absorbed AND NOT OLD.absorbed) then
        perform refresh_last_run(new.run_id);
    end if;

    IF (TG_OP = 'DELETE' AND old.absorbed) THEN
        perform refresh_last_run(old.run_id);
    END IF;

    RETURN NEW;
    END;
$$;

CREATE OR REPLACE TRIGGER new_result_branch_refresh_last_run
  AFTER INSERT OR UPDATE OR DELETE
  ON new_result_branch
  FOR EACH ROW
  EXECUTE FUNCTION new_result_branch_trigger_refresh_last_run();

-- The last "unabsorbed" change. An unabsorbed change is the last change that
-- was not yet merged or pushed.
CREATE OR REPLACE VIEW last_unabsorbed_runs AS
  SELECT
     run.*
  FROM last_run
  INNER JOIN run on last_run.last_unabsorbed_run_id = run.id;


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
    SELECT id, result_code, failure_transient, resume_from INTO last_run FROM run WHERE run.codebase = _codebase AND suite = _campaign ORDER BY start_time DESC LIMIT 1;
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
        SELECT id, result_code INTO last_effective_run FROM run WHERE run.codebase = _codebase AND run.suite = _campaign AND result_code != 'nothing-new-to-do' AND not coalesce(failure_transient, False) ORDER BY start_time DESC limit 1;
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

    PERFORM refresh_last_run(row.codebase, row.suite::text);
    RETURN NEW;
    END;
$$;

CREATE OR REPLACE TRIGGER run_refresh_last_run
  AFTER INSERT OR UPDATE OR DELETE
  ON run
  FOR EACH ROW
  EXECUTE FUNCTION run_trigger_refresh_last_run();

CREATE OR REPLACE FUNCTION run_trigger_refresh_change_set_state()
  RETURNS TRIGGER
  LANGUAGE PLPGSQL
  AS $$
    BEGIN
    IF TG_OP = 'DELETE' THEN
      PERFORM refresh_change_set_state(OLD.change_set);
    ELSIF TG_OP = 'UPDATE' THEN
      PERFORM refresh_change_set_state(OLD.change_set);
      IF OLD.change_set != NEW.change_set THEN
         PERFORM refresh_change_set_state(NEW.change_set);
      END IF;
    ELSE
      PERFORM refresh_change_set_state(NEW.change_set);
    END IF;

    RETURN NEW;
    END;
$$;

CREATE OR REPLACE TRIGGER run_refresh_change_set_state
  AFTER INSERT OR UPDATE OR DELETE
  ON run
  FOR EACH ROW
  EXECUTE FUNCTION run_trigger_refresh_change_set_state();

create or replace view campaigns as select distinct suite as name from run;

CREATE OR REPLACE VIEW perpetual_candidates AS
  select suite, codebase from candidate union select suite, codebase from run;

CREATE OR REPLACE VIEW first_run_time AS
 SELECT DISTINCT ON (run.codebase, run.suite) run.codebase, run.suite, run.start_time
 FROM run ORDER BY run.codebase, run.suite;

CREATE OR REPLACE FUNCTION publish_trigger_refresh_change_set_state()
  RETURNS TRIGGER
  LANGUAGE PLPGSQL
  AS $$
    BEGIN

    if old.result_code = 'success' and old.change_set is not null then
        perform refresh_change_set_state(new.change_set);
        if new.result_code = 'success' and new.change_set is not null then
            perform refresh_change_set_state(new.change_set);
        end if;
    elsif new.result_code = 'success' and new.change_set is not null then
        perform refresh_change_set_state(new.change_set);
    end if;

    RETURN NEW;
    END;
$$;

CREATE OR REPLACE TRIGGER publish_refresh_change_set_state
  AFTER INSERT OR UPDATE
  ON publish
  FOR EACH ROW
  EXECUTE FUNCTION publish_trigger_refresh_change_set_state();


CREATE TABLE site_session (
  id text primary key,
  timestamp timestamp not null default now(),
  userinfo json
);


CREATE FUNCTION expire_site_session_delete_old_rows() RETURNS trigger
  LANGUAGE PLPGSQL
  AS
$$
BEGIN
  DELETE FROM site_session WHERE timestamp < NOW() - INTERVAL '1 week';
  RETURN NEW;
END;
$$;

CREATE OR REPLACE TRIGGER expire_site_session_delete_old_rows_trigger
   AFTER INSERT ON site_session
   EXECUTE FUNCTION expire_site_session_delete_old_rows();

CREATE OR REPLACE VIEW queue_positions AS SELECT
    id,
    codebase,
    suite,
    row_number() OVER (ORDER BY bucket ASC, priority ASC, id ASC) AS position,
    SUM(estimated_duration) OVER (ORDER BY bucket ASC, priority ASC, id ASC)
        - coalesce(estimated_duration, interval '0') AS wait_time
FROM
    queue
ORDER BY bucket ASC, priority ASC, id ASC;

CREATE TABLE result_branch (
 role text not null,
 remote_name text not null,
 base_revision text not null,
 revision text not null
);

CREATE TYPE result_branch_with_policy AS (
  role text,
  remote_name text,
  base_revision text,
  revision text,
  mode publish_mode,
  frequency_days integer);

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
  run.suite AS suite,
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
    candidate.codebase = run.codebase AND candidate.suite = run.suite
INNER JOIN named_publish_policy ON
    candidate.publish_policy = named_publish_policy.name
INNER JOIN change_set ON change_set.id = run.change_set
WHERE
  result_code = 'success')
SELECT * FROM publishable WHERE ARRAY_LENGTH(unpublished_branches, 1) > 0;

CREATE TABLE IF NOT EXISTS review (
 run_id text not null references run (id),
 comment text,
 reviewer text,
 verdict verdict not null,
 reviewed_at timestamp not null default now()
);
CREATE INDEX ON review (run_id);
CREATE UNIQUE INDEX ON review (run_id, reviewer);

CREATE OR REPLACE VIEW change_set_todo AS
  SELECT * FROM candidate WHERE change_set is not NULL AND NOT EXISTS (
        SELECT FROM last_effective_runs WHERE
                change_set = candidate.change_set AND
                codebase = candidate.codebase AND
                suite = candidate.suite AND
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
       run.suite AS campaign,
       run.result::jsonb AS result,
       run.id,
       merge_proposal.merged_at AS absorbed_at,
       merge_proposal.merged_by,
       merge_proposal.url AS merge_proposal_url,
       run.revision
    FROM merge_proposal
    INNER JOIN run ON merge_proposal.revision = run.revision
    WHERE run.result_code = 'success'
    AND run.suite not in ('unchanged', 'control')
 UNION
    SELECT
        'push' AS mode,
        run.change_set,
       run.codebase,
        publish.timestamp - run.finish_time AS delay,
        run.suite AS campaign,
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
    AND run.suite not in ('unchanged', 'control');

CREATE TABLE followup (
    origin text not null references run(id),
    candidate int references candidate (id),
    unique (origin, candidate)
);

CREATE INDEX ON codebase (inactive);
CREATE INDEX ON merge_proposal (can_be_merged);
CREATE INDEX ON run (suite, revision, publish_status);
CREATE INDEX ON run (codebase, suite, start_time, result_code);
CREATE INDEX ON run (start_time);
CREATE INDEX ON run (result_code);
CREATE INDEX ON run (change_set);
CREATE INDEX ON publish (merge_proposal_url);
CREATE INDEX ON publish (revision);
CREATE INDEX ON publish (timestamp);
CREATE INDEX ON queue (bucket);
CREATE INDEX ON queue (codebase);
CREATE INDEX ON queue (suite, priority, id);
CREATE UNIQUE INDEX ON candidate (codebase, suite, coalesce(change_set, ''));
