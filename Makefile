DOCKER_TAG ?= latest
PYTHON ?= python3
SHA = $(shell git rev-parse HEAD)
DOCKERFILES = $(shell ls Dockerfile_* | sed 's/Dockerfile_//' )
DOCKER_TARGETS := $(patsubst %,docker-%,$(DOCKERFILES))
BUILD_TARGETS := $(patsubst %,build-%,$(DOCKERFILES))
PUSH_TARGETS := $(patsubst %,push-%,$(DOCKERFILES))

.PHONY: all check

build-inplace:
	$(PYTHON) setup.py build_ext -i

all: core

core: py/janitor/site/_static/pygments.css build-inplace

check:: typing

check:: test

check:: style

check:: ruff

check:: check-format

check-format:: check-ruff-format

check-ruff-format:
	ruff format --check py tests

check-format:: check-cargo-format

check-cargo-format:
	cargo fmt --check --all

ruff:
	ruff check py tests

fix:: ruff-fix

fix:: clippy-fix

fix:: reformat

clippy-fix:
	cargo clippy --fix --allow-dirty --allow-staged

ruff-fix:
	ruff check --fix .

reformat-ruff:
	ruff format py tests

reformat:: reformat-ruff

reformat::
	cargo fmt --all

suite-references:
	git grep "\\(lintian-brush\|lintian-fixes\|debianize\|fresh-releases\|fresh-snapshots\\)" | grep -v .example

test:: build-inplace
	PYTHONPATH=$(shell pwd)/py:$(PYTHONPATH) PROTOCOL_BUFFERS_PYTHON_IMPLEMENTATION=python $(PYTHON) -m pytest -vv tests

test::
	cargo test

style:: yamllint

yamllint:
	yamllint -s .github/

style:: djlint

check-format:: check-html-format

check-html-format:
	djlint --check py/janitor/site/templates/

djlint:
	djlint py/janitor/site/templates

style:: shellcheck

shellcheck:
	shellcheck container-tests/*.sh

typing:
	$(PYTHON) -m mypy py/janitor tests

py/janitor/site/_static/pygments.css:
	pygmentize -S default -f html > $@

clean:

docker-%:
	$(MAKE) build-$*
	$(MAKE) push-$*

build-%:
	buildah build --no-cache -t ghcr.io/jelmer/janitor/$*:$(DOCKER_TAG) -t ghcr.io/jelmer/janitor/$*:$(SHA) -f Dockerfile_$* .

push-%:
	buildah push ghcr.io/jelmer/janitor/$*:$(DOCKER_TAG)
	buildah push ghcr.io/jelmer/janitor/$*:$(SHA)

.PHONY: docker-all build-all push-all

docker-all: $(DOCKER_TARGETS)

build-all: $(BUILD_TARGETS)

push-all: $(PUSH_TARGETS)

reformat:: reformat-html

reformat-html:
	djlint --reformat py/janitor/site/templates/

codespell:
	codespell
