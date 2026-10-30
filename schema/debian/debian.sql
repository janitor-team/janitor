CREATE EXTENSION IF NOT EXISTS debversion;
CREATE TABLE debian_build (
 run_id text not null references run (id),
 -- Debian version text of the built package
 version debversion not null,
 -- Distribution the package was built for (e.g. "lintian-fixes")
 distribution text not null,
 source text not null,
 binary_packages text[],
 changes_filenames text[],
 lintian_result json
);
CREATE INDEX ON debian_build (run_id);
CREATE INDEX ON debian_build (distribution, source, version);



