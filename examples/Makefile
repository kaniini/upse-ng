# SPDX-License-Identifier: GPL-2.0-or-later

PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
LICENSEDIR ?= $(PREFIX)/share/licenses/upse123
DESTDIR ?=
PKG_CONFIG ?= pkg-config
CC ?= cc
INSTALL ?= install

UPSE_CFLAGS ?= $(shell $(PKG_CONFIG) --cflags libupse-ng)
UPSE_LIBS ?= $(shell $(PKG_CONFIG) --libs libupse-ng)
AO_CFLAGS ?= $(shell $(PKG_CONFIG) --cflags ao)
AO_LIBS ?= $(shell $(PKG_CONFIG) --libs ao)

CPPFLAGS += $(UPSE_CFLAGS) $(AO_CFLAGS)
CFLAGS += -std=c11 -O2 -Wall -Wextra -Werror -pedantic
LDLIBS += $(UPSE_LIBS) $(AO_LIBS) -lm

.PHONY: all clean install

all: upse123

upse123: upse123.c
	$(CC) $(CPPFLAGS) $(CFLAGS) -o $@ $< $(LDFLAGS) $(LDLIBS)

install: upse123
	$(INSTALL) -d $(DESTDIR)$(BINDIR) $(DESTDIR)$(LICENSEDIR)
	$(INSTALL) -m 755 upse123 $(DESTDIR)$(BINDIR)/upse123
	$(INSTALL) -m 644 ../LICENSES/GPL-2.0-or-later.txt \
		$(DESTDIR)$(LICENSEDIR)/GPL-2.0-or-later.txt

clean:
	rm -f upse123
