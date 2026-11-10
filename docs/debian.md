# Debian

## The apt archive

The archiver, `janitor-archive`, makes an apt repository of the packages built
by runs, so people can install and try a change before it is merged.

Each `apt_repository` block in `janitor.conf` is one suite:

```
apt_repository {
  name: "lintian-fixes"
  description: "Builds of lintian fixes"
  base: "unstable"
  select {
    campaign: "lintian-fixes"
  }
}
```

The suite holds the newest build of each package from each campaign in its
`select` blocks. Runs rejected in review are left out. `base` names the
`distribution` block whose components the suite uses.

The archiver serves `/dists/` on its `--public-port` (9915 by default). Have
your web server send `/dists/` there and everything else to the site, which
serves the packages themselves.

### Signing

The archiver signs the archive with the default key in the GnuPG keyring of
the user it runs as; `--gpg-key-id` picks another key. When the site runs with
`--archiver-url`, it serves the public key as `/archive-keyring.gpg` and
`/archive-keyring.asc`.

If the archiver's user has no key, create one as that user, without a
passphrase:

```console
$ gpg --batch --passphrase '' \
    --quick-generate-key 'Janitor archive <archive@janitor.example.org>' default default never
```

### Installing packages

```console
$ sudo curl -o /etc/apt/keyrings/debian-janitor.gpg https://janitor.example.org/archive-keyring.gpg
$ echo 'deb [signed-by=/etc/apt/keyrings/debian-janitor.gpg] https://janitor.example.org/ lintian-fixes main' \
    | sudo tee /etc/apt/sources.list.d/janitor.list
$ sudo apt update
$ sudo apt install -t lintian-fixes <package>
```

Each changeset's page, `/cupboard/cs/<id>/`, shows the lines for a suite
with only that changeset's packages.
