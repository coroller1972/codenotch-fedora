# Included from the root Makefile on Linux, or usable with make -C linux.
LINUX_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
REPO_DIR := $(abspath $(LINUX_DIR)/..)
CARGO_TARGET_DIR ?= $(REPO_DIR)/windows/target
export CARGO_TARGET_DIR
.DEFAULT_GOAL := build

.PHONY: deps build test run install uninstall clean rpm
deps:
	sudo dnf install -y rust cargo gcc gcc-c++ make pkgconf-pkg-config openssl-devel \
		webkit2gtk4.1-devel gtk3-devel libappindicator-gtk3-devel librsvg2-devel \
		libxdo-devel xorg-x11-server-Xwayland python3 nodejs desktop-file-utils

build:
	cd "$(REPO_DIR)/windows" && cargo build --release --locked --workspace

test:
	cd "$(REPO_DIR)/windows" && cargo test --locked --workspace
	cd "$(REPO_DIR)/windows" && node scripts/check-ui-scripts.mjs
	cd "$(REPO_DIR)/windows" && node --test test-*.cjs scripts/test-*.cjs
	python3 -m unittest discover -s "$(LINUX_DIR)" -p 'test_*.py'

run: build
	"$(REPO_DIR)/windows/scripts/run-linux.sh"

install: build
	python3 "$(LINUX_DIR)/install.py" --bin-dir "$(CARGO_TARGET_DIR)/release"

uninstall:
	python3 "$(LINUX_DIR)/install.py" --uninstall

clean:
	cd "$(REPO_DIR)/windows" && cargo clean

# Pin the CLI, include the helper beside /usr/bin/codenotch, and build an RPM.
rpm:
	cd "$(REPO_DIR)/windows" && cargo build --release --locked -p codenotch-hook --target-dir target/hook
	cd "$(REPO_DIR)/windows/codenotch" && npm exec --yes --package=@tauri-apps/cli@2.11.4 -- tauri build --bundles rpm -- --locked
