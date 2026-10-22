# tiny-dfr deck installs alongside the distribution's tiny-dfr package: its
# daemon goes to LIBEXECDIR and a systemd drop-in points the packaged
# tiny-dfr.service at it. Nothing the package owns (/usr/bin/tiny-dfr, its
# unit, udev rules or /usr/share/tiny-dfr) is replaced, so `make uninstall`
# or removing the drop-in returns to stock tiny-dfr.
#
# DESTDIR stages the same installation without touching the host. Packagers
# set PREFIX=/usr UNITDIR=/usr/lib/systemd/system SYSUSERSDIR=/usr/lib/sysusers.d.
DESTDIR ?=
PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
LIBEXECDIR ?= $(PREFIX)/libexec
DATADIR ?= $(PREFIX)/share
DOCDIR ?= $(DATADIR)/doc/tiny-dfr-deck
LICENSEDIR ?= $(DATADIR)/licenses/tiny-dfr-deck
SYSCONFDIR ?= /etc
UNITDIR ?= $(SYSCONFDIR)/systemd/system
SYSUSERSDIR ?= $(SYSCONFDIR)/sysusers.d

# Seed /etc/tiny-dfr/config.toml with the full deck when there is none.
# Packages set 0 and seed it from their install script instead, since a
# package can't own a file the user may already have.
SEED_CONFIG ?= 1

BINARY_DIR ?= target/release
CARGO_JOBS ?= 1
# Explicit destination for the desktop user's helpers; never inferred from sudo.
USER_HOME ?=

DAEMON := $(LIBEXECDIR)/tiny-dfr-multibar
DROPIN := $(UNITDIR)/tiny-dfr.service.d/multibar.conf
SYSUSERS := $(SYSUSERSDIR)/tiny-dfr-deck.conf
DECK_DATA := $(DATADIR)/tiny-dfr-deck
USER_CONFIG := $(SYSCONFDIR)/tiny-dfr/config.toml
DESKTOP_ENTRY := $(DATADIR)/applications/org.dfrdeck.Configurator.desktop

.PHONY: build install install-docs uninstall check-install-inputs \
	install-user-session uninstall-user-session check-user-session-inputs

build:
	cargo build --locked --workspace --release -j$(CARGO_JOBS)

check-install-inputs:
	test -x "$(BINARY_DIR)/tiny-dfr"
	test -x "$(BINARY_DIR)/dfr-configurator"

install-docs:
	install -d "$(DESTDIR)$(DOCDIR)/assets"
	install -m644 README.md INSTALL.md CHANGELOG.md LICENSE LICENSE.material "$(DESTDIR)$(DOCDIR)/"
	install -m644 assets/*.png assets/README.md "$(DESTDIR)$(DOCDIR)/assets/"
	install -d "$(DESTDIR)$(DOCDIR)/docs"
	install -m644 docs/*.md "$(DESTDIR)$(DOCDIR)/docs/"
	set -e; for name in audio brightness capture now-playing viz voice; do \
		install -Dm644 "integrations/$$name/README.md" "$(DESTDIR)$(DOCDIR)/integrations/$$name/README.md"; \
	done

# Copies files only: no build, service restart or user-config overwrite. On a
# real install (no DESTDIR) it also creates the daemon's account.
install: check-install-inputs install-docs
	install -Dm755 "$(BINARY_DIR)/tiny-dfr" "$(DESTDIR)$(DAEMON)"
	@# `tiny-dfr-deck check FILE` validates a configuration with the deck's parser.
	install -d "$(DESTDIR)$(BINDIR)"
	ln -sf "$(DAEMON)" "$(DESTDIR)$(BINDIR)/tiny-dfr-deck"
	install -Dm755 "$(BINARY_DIR)/dfr-configurator" "$(DESTDIR)$(BINDIR)/dfr-configurator"
	install -Dm755 integrations/capture/touchbar-capture-user "$(DESTDIR)$(BINDIR)/touchbar-capture-user"
	install -d "$(DESTDIR)$(DECK_DATA)"
	install -m644 share/tiny-dfr/multibar.toml share/tiny-dfr/multibar-voice.toml \
		share/tiny-dfr/themes.toml share/tiny-dfr/config.toml "$(DESTDIR)$(DECK_DATA)/"
	install -d "$(DESTDIR)$(dir $(DESKTOP_ENTRY))"
	sed 's|^Exec=.*|Exec=$(BINDIR)/dfr-configurator|' share/applications/org.dfrdeck.Configurator.desktop \
		> "$(DESTDIR)$(DESKTOP_ENTRY)"
	chmod 644 "$(DESTDIR)$(DESKTOP_ENTRY)"
	install -d "$(DESTDIR)$(dir $(DROPIN))"
	sed 's|@DAEMON@|$(DAEMON)|' etc/systemd/tiny-dfr.service.d/multibar.conf.in > "$(DESTDIR)$(DROPIN)"
	chmod 644 "$(DESTDIR)$(DROPIN)"
	install -Dm644 etc/sysusers.d/tiny-dfr-deck.conf "$(DESTDIR)$(SYSUSERS)"
	install -d "$(DESTDIR)$(LICENSEDIR)"
	install -m644 LICENSE LICENSE.material "$(DESTDIR)$(LICENSEDIR)/"
	@# The full deck becomes the configuration only when there is none yet.
	if [ "$(SEED_CONFIG)" = 1 ] && [ ! -e "$(DESTDIR)$(USER_CONFIG)" ]; then \
		install -Dm644 share/tiny-dfr/multibar.toml "$(DESTDIR)$(USER_CONFIG)"; \
	fi
	if [ -z "$(DESTDIR)" ]; then systemd-sysusers "$(SYSUSERS)"; fi
	@echo "Installed. Apply with: sudo systemctl daemon-reload && sudo systemctl restart tiny-dfr.service"

# Leaves /etc/tiny-dfr/config.toml, /var/lib/tiny-dfr and the tiny-dfr
# account in place; the packaged tiny-dfr takes over again after a reload.
uninstall:
	rm -f "$(DESTDIR)$(DROPIN)" "$(DESTDIR)$(DAEMON)" "$(DESTDIR)$(SYSUSERS)" \
		"$(DESTDIR)$(BINDIR)/dfr-configurator" "$(DESTDIR)$(BINDIR)/touchbar-capture-user" \
		"$(DESTDIR)$(BINDIR)/tiny-dfr-deck" \
		"$(DESTDIR)$(DESKTOP_ENTRY)"
	-rmdir "$(DESTDIR)$(dir $(DROPIN))" 2>/dev/null
	rm -rf "$(DESTDIR)$(DECK_DATA)" "$(DESTDIR)$(DOCDIR)" "$(DESTDIR)$(LICENSEDIR)"
	@echo "Removed. Apply with: sudo systemctl daemon-reload && sudo systemctl restart tiny-dfr.service"

# Run as the desktop user: copies every desktop-session helper except voice
# (see integrations/user-setup). USER_HOME must be given explicitly.
check-user-session-inputs:
	case "$(USER_HOME)" in /*) ;; *) echo 'Set USER_HOME to the desktop user absolute home path' >&2; exit 1;; esac

install-user-session: check-user-session-inputs
	USER_HOME="$(USER_HOME)" DESTDIR="$(DESTDIR)" integrations/user-setup install

uninstall-user-session: check-user-session-inputs
	USER_HOME="$(USER_HOME)" DESTDIR="$(DESTDIR)" integrations/user-setup uninstall
