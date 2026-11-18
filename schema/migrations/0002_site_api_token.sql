-- Site-issued API tokens.

CREATE TABLE IF NOT EXISTS site_api_token (
  hash text primary key,
  owner text not null,
  userinfo json not null,
  created timestamp not null default now(),
  expires_at timestamp not null
);

CREATE OR REPLACE FUNCTION expire_site_api_token_delete_old_rows() RETURNS trigger
  LANGUAGE PLPGSQL
  AS
$$
BEGIN
  DELETE FROM site_api_token WHERE expires_at < NOW();
  RETURN NEW;
END;
$$;

CREATE OR REPLACE TRIGGER expire_site_api_token_delete_old_rows_trigger
   AFTER INSERT OR DELETE ON site_api_token
   FOR EACH STATEMENT WHEN (pg_trigger_depth() < 1)
   EXECUTE FUNCTION expire_site_api_token_delete_old_rows();
