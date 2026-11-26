-- Drop the second copies of indexes that state.sql used to create twice.

DROP INDEX IF EXISTS candidate_codebase_campaign_coalesce_idx, publish_merge_proposal_url_idx1, publish_revision_idx1, publish_timestamp_idx1, run_change_set_idx1, run_result_code_idx1, run_start_time_idx1;
