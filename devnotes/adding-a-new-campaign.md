# Adding a new campaign

## Create a new codemod script

At the core of every campaign is a script that can make changes
to a version controlled branch.

This script will be executed in a version controlled checkout of
a source codebase, and can make changes to the codebase as it sees fit.
See [this blog post](https://www.jelmer.uk/silver-platter-intro.html) for more
information about creating codemod scripts.

The silver-platter
[codemod protocol](https://github.com/jelmer/silver-platter/blob/master/codemod-protocol.md)
describes how to write a codemod, and the
[example recipes](https://github.com/jelmer/silver-platter/tree/master/examples/recipes)
show some real ones.

[`examples/strip-trailing-whitespace`](../examples/strip-trailing-whitespace)
is a small codemod for a generic campaign.

You can test the script on its own with silver-platter. In a checkout of a
codebase, run:

    svp apply --diff /path/to/janitor/examples/strip-trailing-whitespace

For a Debian package, run `debian-svp apply` instead. Either command commits
the change to that checkout.

## Add configuration for the campaign

In janitor.conf, add a section for the campaign. E.g.:

    campaign {
      name: "strip-trailing-whitespace"
      command: "strip-trailing-whitespace"
      branch_name: "strip-trailing-whitespace"
      generic_build {}
    }

The command runs on the worker, so the script must be on the worker's
`PATH`; see [Custom worker tooling](../docs/containers.md#custom-worker-tooling).
A Debian campaign uses a `debian_build` section instead.
[`janitor.conf.example`](../janitor.conf.example) has both kinds.

## Add script for finding candidates

Add a script that can gather candidates for the new campaign. This script should
be run regularly to find new candidates to schedule, with its JSON output
uploaded to $RUNNER_URL/candidates.
