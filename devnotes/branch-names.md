# Branch names

## Goal

For runs:
* runs/$uuid/$function tags
* runs/$uuid/tags/$tag tags for new/updated upstream tags

There is a symref at refs/$campaign/$function that points at said tag and updated

Also, track this information in the database.

For related repositories, create remotes:

* remotes/origin/ for Debian packaging
* remotes/upstream/ for Upstream

Names for functions:

* "main" for the main branch (packaging or otherwise)
* "upstream" for the upstream import branch for Debian packages
* "pristine-tar" for the pristine-tar branch

## Roadmap

* Update publisher to use new tag names

* Send requests to publisher to mirror origin/upstream repositories.
  To start off with just:
   * name of remote ("origin", "upstream")
   * URL of remote

* Push symrefs (refs/$campaign/$function => refs/tags/$uuid/$function)
  + needs to be done by publisher
