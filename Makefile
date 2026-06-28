.PHONY: test build package clean serve install bootstrap install-service uninstall-service restart status logs install-clab-service uninstall-clab-service restart-clab status-clab logs-clab

PREFIX ?= $(CURDIR)/dist
BIN_DIR := $(CURDIR)/bin
RUNTIME_ROOT ?= $(HOME)/.clab
PLIST_DIR ?= $(CURDIR)/launchd
PORT ?= 7777
HOST ?= 127.0.0.1


test:
	cargo test

build:
	cargo build --release -p clab
	mkdir -p $(BIN_DIR)
	cp target/release/clab $(BIN_DIR)/clab

serve: build
	CLAB_HOME=$(HOME)/.clab CLAB_SKILLS_DIR=$(HOME)/.clab/skills CLAB_CODEBASE_PROJECT_PATH=$(CURDIR) \
	$(BIN_DIR)/clab serve --host $(HOST) --port $(PORT)

install: bootstrap install-service

bootstrap: build

install-service: install-clab-service

uninstall-service: uninstall-clab-service

restart: build
	mkdir -p $(RUNTIME_ROOT)/logs
	$(MAKE) uninstall-service
	sleep 2
	$(MAKE) install-service

status: status-clab

logs: logs-clab

install-clab-service: build
	mkdir -p $(RUNTIME_ROOT)/logs
	-launchctl unload $(PLIST_DIR)/dev.clab.plist
	launchctl load $(PLIST_DIR)/dev.clab.plist

uninstall-clab-service:
	-launchctl unload $(PLIST_DIR)/dev.clab.plist

restart-clab: build
	mkdir -p $(RUNTIME_ROOT)/logs
	$(MAKE) uninstall-clab-service
	sleep 2
	launchctl load $(PLIST_DIR)/dev.clab.plist

status-clab:
	launchctl list dev.clab

logs-clab:
	tail -f $(RUNTIME_ROOT)/logs/clab.err.log

package: build
	mkdir -p $(PREFIX)
	tar -C $(CURDIR) -czf $(PREFIX)/clab-$$(uname -s)-$$(uname -m).tar.gz bin/clab Cargo.toml Cargo.lock Makefile .gitignore crates launchd

clean:
	rm -rf target dist bin/clab
