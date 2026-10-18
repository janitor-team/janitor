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

## Add script for finding candidates

Add a script that can gather candidates for the new campaign. This script should
be run regularly to find new candidates to schedule, with its JSON output
uploaded to $RUNNER_URL/candidates.
