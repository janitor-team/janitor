DOCKER_TAG ?= latest
PYTHON ?= python3
SHA = $(shell git rev-parse HEAD)
IMAGE_BASE ?= ghcr.io/jelmer/janitor
DOCKERFILES = $(shell ls Dockerfile_* | sed 's/Dockerfile_//' | grep -v '^rust_builder$$' )
RUST_IMAGES = archive auto_upload bzr_store differ git_store publish runner
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

typing:
	$(PYTHON) -m mypy py/janitor tests

py/janitor/site/_static/pygments.css:
	pygmentize -S default -f html > $@

clean:

docker-%:
	$(MAKE) build-$*
	$(MAKE) push-$*

build-%:
	buildah build --no-cache -t $(IMAGE_BASE)/$*:$(DOCKER_TAG) -t $(IMAGE_BASE)/$*:$(SHA) -f Dockerfile_$* .

# The Rust service images take their binaries from this image.
rust-builder:
	buildah build --layers -t localhost/janitor/rust_builder:latest -f Dockerfile_rust_builder .

$(patsubst %,build-%,$(RUST_IMAGES)): rust-builder

smoke-%: build-%
	podman run --rm $(IMAGE_BASE)/$*:$(DOCKER_TAG) --help

smoke-rust: $(patsubst %,smoke-%,$(RUST_IMAGES))

rust-images:
	@echo $(RUST_IMAGES)

push-%:
	buildah push $(IMAGE_BASE)/$*:$(DOCKER_TAG)
	buildah push $(IMAGE_BASE)/$*:$(SHA)

.PHONY: docker-all build-all push-all rust-builder smoke-rust rust-images

docker-all: $(DOCKER_TARGETS)

build-all: $(BUILD_TARGETS)

push-all: $(PUSH_TARGETS)

reformat:: reformat-html

reformat-html:
	djlint --reformat py/janitor/site/templates/

codespell:
	codespell
