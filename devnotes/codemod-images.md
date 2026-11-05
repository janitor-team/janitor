# Codemods as images

For https://github.com/janitor-team/janitor/issues/58. This covers
codemods. Builds and lintian could follow the same way later.

## Why

Every codemod has to be installed in the worker image, and each instance
builds its own worker image for that. So adding or upgrading a codemod
means redeploying every worker. All codemods share one set of packages,
so two codemods that need different versions of a library cannot run on
the same worker. One campaign cannot stay on an older codemod, or try a
new one, while the others move on. If a campaign could name an image for
its codemod, updating it would be a config change.

## Design

In 2022 the tricky part was docker-in-docker. `Dockerfile_worker`
already sets up sbuild's unshare mode, so the worker can use
unprivileged user namespaces. The image is only a way to ship the
codemod's files. No container engine runs on the worker.

- A campaign gets an optional `codemod_image`, pinned by digest. The
  runner passes it in the `codemod` part of the assignment.
- The worker pulls the image once per digest and flattens it into a root
  tarball. It drops the `./dev/*` entries, since an unprivileged unpack
  cannot create device nodes and the session sets up its own `/dev`. So
  fakeroot should not be needed.
- The codemod runs in an ognibuild `UnshareSession` made from that
  tarball, with the working tree bound at `/code` and the network on. It
  runs as root in the session, and that root is the worker's own user
  outside. Committing and reading the result stay in silver-platter on
  the host, so most existing codemods should work unchanged (see open
  questions 3 and 4).
- The worker records the digest with the run. It also echoes the image
  reference in its metadata. An older worker would ignore the image in
  the assignment and run the codemod on the host, so the runner fails a
  result without the echo.

As far as I can see this needs three changes outside this repository:

- in ognibuild, a bind option for `UnshareSession`.
- in ognibuild, a setup error instead of a panic in `from_tarball` when
  the image has no `groupadd` or `useradd`.
- in silver-platter, a way for the callers of both `script_runner`
  functions to pick the `SVP_RESULT` and `SVP_RESUME` paths, so they can
  be bound into the root.

## Open questions

1. Is unshare with a flattened image what you had in mind, or would you
   rather have rootless podman where it is available?
2. OCI images, or plain root tarballs at a URL with a hash to start
   with? For OCI, which tool should pull and flatten, for example
   `skopeo` with `umoci`?
3. Codemods that commit themselves, such as `lintian-brush`, need breezy
   in the image. Is that fine?
4. `deb-new-upstream` makes its tarball with `janitor-dist` through
   `DIST`, and that tool is not in the image. Should such a codemod ship
   its own tool in the image, or call something on the worker?
