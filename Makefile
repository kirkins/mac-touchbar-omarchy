# Fixed runtime paths match the daemon, native launcher and stock unit.
# DESTDIR stages the same installation without touching the host.
DESTDIR ?=
BINARY_DIR ?= target/release
CARGO_JOBS ?= 1

.PHONY: build install check-install-inputs
build:
	cargo build --locked --workspace --release -j$(CARGO_JOBS)

check-install-inputs:
	test -x "$(BINARY_DIR)/tiny-dfr"
	test -x "$(BINARY_DIR)/dfr-configurator"

# No build, service operation, credential setup or user config overwrite here.
install: check-install-inputs
	install -Dm755 "$(BINARY_DIR)/tiny-dfr" "$(DESTDIR)/usr/bin/tiny-dfr"
	install -Dm755 "$(BINARY_DIR)/dfr-configurator" "$(DESTDIR)/usr/local/bin/dfr-configurator"
	install -Dm755 integrations/capture/touchbar-capture-user "$(DESTDIR)/usr/local/bin/touchbar-capture-user"
	install -d "$(DESTDIR)/usr/share/tiny-dfr"
	install -m644 share/tiny-dfr/*.toml share/tiny-dfr/*.svg "$(DESTDIR)/usr/share/tiny-dfr/"
	install -Dm644 etc/udev/rules.d/99-touchbar-tiny-dfr.rules "$(DESTDIR)/etc/udev/rules.d/99-touchbar-tiny-dfr.rules"
	install -Dm644 etc/systemd/system/tiny-dfr.service "$(DESTDIR)/etc/systemd/system/tiny-dfr.service"
	install -Dm644 share/applications/org.dfrdeck.Configurator.desktop "$(DESTDIR)/usr/local/share/applications/org.dfrdeck.Configurator.desktop"
	install -d "$(DESTDIR)/usr/share/licenses/tiny-dfr" "$(DESTDIR)/usr/share/doc/tiny-dfr"
	install -m644 LICENSE LICENSE.material "$(DESTDIR)/usr/share/licenses/tiny-dfr/"
	install -m644 README.md INSTALL.md "$(DESTDIR)/usr/share/doc/tiny-dfr/"
