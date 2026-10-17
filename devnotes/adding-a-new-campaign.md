# Adding a new campaign

## Create a new codemod script

At the core of every {term}`campaign` is a script (a {term}`codemod`) that
can make changes to a version-controlled branch.

This script will be executed in a version-controlled checkout of a source
codebase, and can make changes to the codebase as it sees fit. See
[this blog post](https://www.jelmer.uk/silver-platter-intro.html) for more
information about creating codemod scripts.

You can test the script on its own with {term}`silver-platter`: `svp` (or
`debian-svp` for Debian packages) runs it in a checkout and shows the diff:

```console
$ cd checkout-of-some-codebase
$ svp apply --diff myscript
```

The script must also be installed in your worker image, see
{ref}`custom-worker-tooling`.

## Add configuration for the campaign

In `janitor.conf`, add a section for the campaign. `command` is the script to
run, unless a candidate gives its own. For plain code, use `generic_build`:

```text
campaign {
  name: "some-name"
  command: "myscript"
  branch_name: "some-name"
  generic_build {}
}
```

For Debian packages, use `debian_build` instead. Its `base_distribution` must
name a `distribution { ... }` section of `janitor.conf`:

```text
campaign {
  name: "some-name"
  command: "myscript"
  branch_name: "some-name"
  debian_build {
    base_distribution: "unstable"
    build_distribution: "some-name"
    build_suffix: "suf"
  }
}
```

`janitor.conf.example` shows all the fields.

## Add a publish policy

A {term}`candidate` is only published if it names a {term}`publish policy`.
Create one with the publisher's private API (port 9912 in the container
image). `main` is the role of the main branch:

```console
$ cat policy.json
{"rate_limit_bucket": "default",
 "per_branch": {"main": {"mode": "propose"}}}
$ curl -X PUT -H 'Content-Type: application/json' \
    --data @policy.json http://localhost:9912/policy/some-name
```

The modes are listed in {ref}`the-publisher`.

## Add a script for finding candidates

Add a script that can gather candidates for the new campaign. Run this script
regularly to find new candidates to schedule, and POST its JSON output to the
runner's private `/candidates` endpoint (port 9911 in the container image).
Each item names a known codebase and campaign, and `value` must not be 0:

```console
$ cat candidates.json
[{"codebase": "hello", "campaign": "some-name", "value": 10,
  "success_chance": 0.5, "publish-policy": "some-name"}]
$ curl -X POST -H 'Content-Type: application/json' \
    --data @candidates.json http://localhost:9911/candidates
```

The codebases must be uploaded first, see
{ref}`uploading codebases <codebases>`. The runner skips candidates for
unknown codebases.

The new runs then show up in the queue: `/cupboard/queue` on the site, or
`janitor-admin queue list`.
