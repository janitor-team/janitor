#!/bin/bash
# Start a service image next to postgres and redis and check that it answers.
# Usage: startup-check.sh SUFFIX IMAGE
# Uses podman, or docker if there is no podman. Set CONTAINER_ENGINE to pick.
set -eu -o pipefail

suffix="$1"
image="$2"
engine="${CONTAINER_ENGINE:-$(command -v podman || echo docker)}"
here="$(cd "$(dirname "$0")" && pwd)"
id="startup-check-$$"
config="--config=/ci/janitor.conf"

c() { "${engine}" "$@"; }

cleanup() {
  [ -z "${stub:-}" ] || kill "${stub}" 2> /dev/null || true
  for x in "${id}" "${id}-redis" "${id}-db"; do
    c rm -f "${x}" > /dev/null 2>&1 || true
  done
  c network rm "${id}" > /dev/null 2>&1 || true
}
trap cleanup EXIT

fail() {
  echo "FAIL: ${suffix}: $*"
  c logs "${logs_of:-${id}}" || true
  exit 1
}

running() {
  [ "$(c inspect -f '{{.State.Running}}' "${id}")" = true ]
}

# Try PORT/PATH up to 60 times until it gives a successful HTTP response.
probe() {
  url="http://127.0.0.1:$1"
  for _ in $(seq 60); do
    if curl -fs -o /dev/null --max-time 5 "${url}"; then
      echo "OK: ${url}"
      return
    fi
    running || fail "exited before ${url} answered"
    sleep 1
  done
  fail "no successful HTTP response from ${url} after 60 tries"
}

if [ "${suffix}" = mail_filter ]; then
  # A stdin filter, not a server: feed it a merge email and check it posts
  # the merge proposal URL to a stub refresh endpoint on the host.
  exec 3< <(python3 "${here}/refresh-stub.py")
  stub=$!
  read -r port <&3 || fail "refresh stub did not start"
  c run -i --name "${id}" --network host "${image}" \
    --refresh-url="http://127.0.0.1:${port}/" \
    < "${here}/../mail-filter/tests/data/github-merged-email.txt" \
    || fail "exited with an error"
  got="$(cat <&3)"
  grep -F https://github.com/UbuntuBudgie/budgie-desktop/pull/78 <<< "${got}" \
    || fail "no merge proposal URL posted, the stub got: ${got:-nothing}"
  echo "OK: merge proposal URL posted"
  exit 0
fi

case "${suffix}" in
  archive)
    args=("${config}" --dists-directory=/tmp/dists --no-gpg)
    urls=(9914/health 9914/ready) ;;
  auto_upload)
    args=("${config}")
    urls=(9933/metrics) ;;
  bzr_store)
    args=("${config}" --vcs-path=/bzr)
    urls=(9929/health 9929/ready 9930/) ;;
  differ)
    args=("${config}" --cache-path=/tmp/cache)
    urls=(9920/health 9920/ready) ;;
  git_store)
    args=("${config}" --vcs-path=/git)
    urls=(9923/health 9923/ready 9924/) ;;
  ognibuild_dep)
    args=()
    urls=(9934/health 9934/ready) ;;
  publish)
    args=("${config}")
    urls=(9912/health 9912/ready) ;;
  runner)
    args=("${config}" --public-vcs-location=http://127.0.0.1:9924/)
    urls=(9911/health 9911/ready 9919/health/live) ;;
  site)
    args=("${config}")
    urls=(8080/health 8090/) ;;
  worker)
    args=(--base-url=http://127.0.0.1:9/ --loop)
    urls=(9821/health) ;;
  *)
    echo "No start-up check for ${suffix}"
    exit 1 ;;
esac

c network create "${id}" > /dev/null
c run -d --name "${id}-db" --network "${id}" --network-alias db \
  -e POSTGRES_HOST_AUTH_METHOD=trust docker.io/library/postgres:16 > /dev/null
c run -d --name "${id}-redis" --network "${id}" --network-alias redis \
  docker.io/library/redis:7 > /dev/null
c exec -e DEBIAN_FRONTEND=noninteractive "${id}-db" sh -c 'apt-get update -qq \
  && apt-get install -qqy --no-install-recommends postgresql-16-debversion' \
  > /dev/null
for i in $(seq 60); do
  c exec "${id}-db" pg_isready -q -h 127.0.0.1 && break
  [ "${i}" -lt 60 ] \
    || logs_of="${id}-db" fail "postgres not ready after 60 tries"
  sleep 1
done
cat "${here}/../schema/state.sql" "${here}/../schema/debian/debian.sql" \
  | c exec -i "${id}-db" psql -q -v ON_ERROR_STOP=1 -h 127.0.0.1 -U postgres \
  > /dev/null

ports=()
for p in $(printf '%s\n' "${urls[@]%%/*}" | sort -u); do
  ports+=(-p "127.0.0.1:${p}:${p}")
done
c run -d --name "${id}" --network "${id}" "${ports[@]}" \
  -v "${here}/startup-check.conf:/ci/janitor.conf:ro,z" \
  "${image}" "${args[@]}" > /dev/null
for u in "${urls[@]}"; do
  probe "${u}"
done
sleep 2
running || fail "exited after answering"
