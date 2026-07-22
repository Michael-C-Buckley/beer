# - make            # build the release binary
# - make install    # install under $(PREFIX), honouring $(DESTDIR)
# - make uninstall
#
# Override PREFIX/DESTDIR as usual, e.g. `make install PREFIX=/usr DESTDIR=pkg`.

PREFIX ?= /usr/local
DESTDIR ?=

BINDIR := $(DESTDIR)$(PREFIX)/bin
MANDIR := $(DESTDIR)$(PREFIX)/share/man
TERMINFODIR := $(DESTDIR)$(PREFIX)/share/terminfo
APPDIR := $(DESTDIR)$(PREFIX)/share/applications
ICONDIR := $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps
DOCDIR := $(DESTDIR)$(PREFIX)/share/doc/beer

CARGO ?= cargo
SCDOC ?= scdoc
TIC ?= tic
INSTALL ?= install

APPID := dev.notashelf.beer

.PHONY: all build man install install-bin install-man install-terminfo \
        install-desktop install-doc uninstall clean

all: build

build:
	$(CARGO) build --release --locked

man: beer.1 beer.toml.5 beer-themes.7

beer.1: doc/beer.1.scd
	$(SCDOC) < $< > $@
beer.toml.5: doc/beer.toml.5.scd
	$(SCDOC) < $< > $@
beer-themes.7: doc/beer-themes.7.scd
	$(SCDOC) < $< > $@

install: install-bin install-man install-terminfo install-desktop install-doc

install-bin: build
	$(INSTALL) -Dm755 target/release/beer $(BINDIR)/beer

install-man: man
	$(INSTALL) -Dm644 beer.1 $(MANDIR)/man1/beer.1
	$(INSTALL) -Dm644 beer.toml.5 $(MANDIR)/man5/beer.toml.5
	$(INSTALL) -Dm644 beer-themes.7 $(MANDIR)/man7/beer-themes.7

install-terminfo:
	$(TIC) -x -o $(TERMINFODIR) terminfo/beer.info

install-desktop:
	$(INSTALL) -Dm644 contrib/$(APPID).desktop $(APPDIR)/$(APPID).desktop
	$(INSTALL) -Dm644 contrib/$(APPID).svg $(ICONDIR)/$(APPID).svg

install-doc:
	$(INSTALL) -Dm644 contrib/beer.toml $(DOCDIR)/beer.toml.example

uninstall:
	$(RM) $(BINDIR)/beer
	$(RM) $(MANDIR)/man1/beer.1 $(MANDIR)/man5/beer.toml.5 $(MANDIR)/man7/beer-themes.7
	$(RM) $(APPDIR)/$(APPID).desktop $(ICONDIR)/$(APPID).svg
	$(RM) $(DOCDIR)/beer.toml.example

clean:
	$(RM) beer.1 beer.toml.5 beer-themes.7
	$(CARGO) clean
