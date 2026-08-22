# SPDX-License-Identifier: LGPL-2.1-or-later

PREFIX ?= /usr/local
DESTDIR ?=
PKG_CONFIG ?= pkg-config
CXX ?= c++
INSTALL ?= install

PLUGIN := upse-ng.so
TEST := test-plugin
AUDACIOUS_PLUGIN_DIR ?= $(shell $(PKG_CONFIG) --variable=plugin_dir audacious)
INPUT_PLUGIN_DIR ?= $(AUDACIOUS_PLUGIN_DIR)/Input
LICENSE_DIR ?= $(PREFIX)/share/licenses/audacious-upse-ng
AUDACIOUS_CFLAGS ?= $(shell $(PKG_CONFIG) --cflags audacious)
AUDACIOUS_LIBS ?= $(shell $(PKG_CONFIG) --libs audacious)
UPSE_CFLAGS ?= $(shell $(PKG_CONFIG) --cflags libupse-ng)
UPSE_LIBS ?= $(shell $(PKG_CONFIG) --libs libupse-ng)

CPPFLAGS += $(AUDACIOUS_CFLAGS) $(UPSE_CFLAGS)
CXXFLAGS += -std=c++17 -O2 -Wall -Wextra -Werror -pedantic
PLUGIN_CXXFLAGS := -fPIC -fvisibility=hidden
PLUGIN_LDFLAGS := -shared -Wl,-z,defs

.PHONY: all check install uninstall clean

all: $(PLUGIN)

$(PLUGIN): upse-ng.cc
	$(CXX) $(CPPFLAGS) $(CXXFLAGS) $(PLUGIN_CXXFLAGS) \
		$(LDFLAGS) $(PLUGIN_LDFLAGS) -o $@ $< \
		$(UPSE_LIBS) $(AUDACIOUS_LIBS)

$(TEST): test.cc
	$(CXX) $(CPPFLAGS) $(CXXFLAGS) $(LDFLAGS) -o $@ $< \
		$(AUDACIOUS_LIBS) -ldl

check: $(PLUGIN) $(TEST)
	@test -n "$(FIXTURE_DIR)" || { \
		echo 'FIXTURE_DIR must name the synthetic fixture directory' >&2; \
		exit 1; \
	}
	mkdir -p $(FIXTURE_DIR)/audacious-config
	XDG_CONFIG_HOME='$(FIXTURE_DIR)/audacious-config' \
		./$(TEST) ./$(PLUGIN) $(FIXTURE_DIR)/synthetic.psf \
		$(FIXTURE_DIR)/synthetic.minipsf $(FIXTURE_DIR)/synthetic.psf2

install: $(PLUGIN)
	$(INSTALL) -d $(DESTDIR)$(INPUT_PLUGIN_DIR) $(DESTDIR)$(LICENSE_DIR)
	$(INSTALL) -m 755 $(PLUGIN) $(DESTDIR)$(INPUT_PLUGIN_DIR)/$(PLUGIN)
	$(INSTALL) -m 644 ../../LICENSES/LGPL-2.1-or-later.txt \
		$(DESTDIR)$(LICENSE_DIR)/LGPL-2.1-or-later.txt

uninstall:
	rm -f $(DESTDIR)$(INPUT_PLUGIN_DIR)/$(PLUGIN) \
		$(DESTDIR)$(LICENSE_DIR)/LGPL-2.1-or-later.txt

clean:
	rm -f $(PLUGIN) $(TEST)
