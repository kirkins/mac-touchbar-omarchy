# Fixed runtime paths match the daemon, native launcher and stock unit.
# DESTDIR stages the same installation without touching the host.
DESTDIR ?=
BINARY_DIR ?= target/release
CARGO_JOBS ?= 1
# Explicit destination for the ordinary-user receiver; never infer from sudo.
USER_HOME ?=

.PHONY: build install check-install-inputs install-user-session check-user-session-inputs
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
	install -Dm644 etc/udev/rules.d/99-touchbar-seat.rules "$(DESTDIR)/etc/udev/rules.d/99-touchbar-seat.rules"
	install -Dm644 etc/systemd/system/tiny-dfr.service "$(DESTDIR)/etc/systemd/system/tiny-dfr.service"
	install -Dm644 share/applications/org.dfrdeck.Configurator.desktop "$(DESTDIR)/usr/local/share/applications/org.dfrdeck.Configurator.desktop"
	install -d "$(DESTDIR)/usr/share/licenses/tiny-dfr" "$(DESTDIR)/usr/share/doc/tiny-dfr"
	install -m644 LICENSE LICENSE.material "$(DESTDIR)/usr/share/licenses/tiny-dfr/"
	install -m644 README.md INSTALL.md LICENSE LICENSE.material "$(DESTDIR)/usr/share/doc/tiny-dfr/"
	set -e; for name in audio brightness capture now-playing viz voice; do \
		install -Dm644 "integrations/$$name/README.md" "$(DESTDIR)/usr/share/doc/tiny-dfr/integrations/$$name/README.md"; \
	done
	install -d "$(DESTDIR)/usr/share/doc/tiny-dfr/share/tiny-dfr"
	install -m644 share/tiny-dfr/*.toml "$(DESTDIR)/usr/share/doc/tiny-dfr/share/tiny-dfr/"

# A separate copy-only step, run as the desktop user, with no enable/start.
# Refuse existing helpers/units (including dangling symlinks) before any copy.
check-user-session-inputs:
	test "$$(id -u)" != 0
	case "$(USER_HOME)" in /*) ;; *) echo 'Set USER_HOME to the desktop user absolute home path' >&2; exit 1;; esac
	case "$(USER_HOME)/" in /|*//*|*/../*|*/./*) echo 'USER_HOME must be a normalized non-root home path' >&2; exit 1;; esac
	test ! -e "$(DESTDIR)$(USER_HOME)/.local/bin/touchbar-capture" && test ! -L "$(DESTDIR)$(USER_HOME)/.local/bin/touchbar-capture"
	test ! -e "$(DESTDIR)$(USER_HOME)/.config/systemd/user/touchbar-capture.service" && test ! -L "$(DESTDIR)$(USER_HOME)/.config/systemd/user/touchbar-capture.service"

install-user-session: check-user-session-inputs
	install -Dm755 integrations/capture/touchbar-capture "$(DESTDIR)$(USER_HOME)/.local/bin/touchbar-capture"
	install -Dm644 integrations/capture/touchbar-capture.service "$(DESTDIR)$(USER_HOME)/.config/systemd/user/touchbar-capture.service"
