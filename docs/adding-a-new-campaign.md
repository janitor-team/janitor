# Adding a new campaign

## Create a new codemod script

At the core of every campaign is a script that can make changes
to a version controlled branch.

This script will be executed in a version controlled checkout of
a source codebase, and can make changes to the codebase as it sees fit.
See [this blog post](https://www.jelmer.uk/silver-platter-intro.html) for more
information about creating codemod scripts.

You can test the script on its own with silver-platter. In a checkout of a
codebase, run:

    debian-svp apply --diff /path/to/myscript

This commits the change to that checkout, so use a copy you can throw away.

## Add configuration for the campaign

In janitor.conf, add a section for the campaign. E.g.:

    campaign {
      name: "some-name"
      command: "myscript"
      branch_name: "some-name"
      debian_build {
        base_distribution: "unstable"
        build_suffix: "suf"
      }
    }

## Write the merge proposal text

The publisher writes the description of a merge proposal from a template
named after the campaign: `<campaign>.md` for forges that take Markdown, and
`<campaign>.txt` for the others. It reads them from the directory given to
`janitor-publish` with `--template-env-path`, or else from
`proposal-templates` next to the `janitor-publish-one` binary. Without a
template, the description is just `Changes for <campaign> (<run id>)`.

The templates use [minijinja](https://docs.rs/minijinja), which follows the
Jinja2 syntax. A description template can use:

- `campaign`: the campaign name.
- `log_id`: the run id.
- `role`: the branch role, such as `main`.
- `codemod`: the `context` object that the codemod wrote to `$SVP_RESULT`.
  Each of its keys is also a variable of its own, such as `applied` for
  lintian-brush.
- `debdiff`: the debdiff between the builds of the run and of its control
  run, when the differ has one.
- `external_url`: the `--external-url` given to `janitor-publish`, without
  the trailing slash.
- `debdiff_is_empty(debdiff)`: whether the debdiff shows no changes.
- `markdownify_debdiff(debdiff)`: the debdiff as Markdown.
- `parseaddr(address)`: `Name <email>` split into `[name, email]`.

For example, a `lintian-fixes.md`:

    Fix some issues reported by lintian

    {% for fix in applied %}
    * {{ fix.summary }}
    {% endfor %}

`title` and `commit_message` in the campaign's `merge_proposal` block are
templates too. They can use the keys of the codemod's `context`,
`external_url` and the three functions, but not `campaign`, `log_id`,
`role`, `codemod` or `debdiff`. Without a title, the publisher takes the
first sentence of the `.txt` description. Where the forge supports labels,
the proposal gets the campaign name as its label.

## Add script for finding candidates

Add a script that can gather candidates for the new campaign. This script should
be run regularly to find new candidates to schedule, with its JSON output
uploaded to $RUNNER_URL/candidates.
