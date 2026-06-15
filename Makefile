# SPDX-License-Identifier: LGPL-2.1-or-later

PREFIX ?= /usr/local
EXEC_PREFIX ?= $(PREFIX)
LIBDIR ?= $(EXEC_PREFIX)/lib
INCLUDEDIR ?= $(PREFIX)/include
PKGCONFIGDIR ?= $(LIBDIR)/pkgconfig
DESTDIR ?=
BUILD_DIR ?= build
CARGO ?= cargo
CBINDGEN ?= cbindgen
CC ?= cc
INSTALL ?= install

VERSION := 0.1.0
ABI_MAJOR := 0
RUST_STATIC := target/release/libupse_ng.a
STATIC_LIBRARY := $(BUILD_DIR)/libupse-ng.a
SHARED_LIBRARY := $(BUILD_DIR)/libupse-ng.so.$(VERSION)
SHARED_SONAME := libupse-ng.so.$(ABI_MAJOR)
PKGCONFIG_FILE := $(BUILD_DIR)/libupse-ng.pc

.PHONY: all check fmt clippy test doc header check-header rust-library \
	libraries check-exports pkgconfig upse123 check-c-api \
	check-c-api-sanitize install install-upse123 uninstall clean

all: libraries pkgconfig

check: fmt clippy test doc check-header check-exports check-c-api

fmt:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --workspace --all-targets --all-features --locked -- -D warnings

test:
	$(CARGO) test --workspace --all-targets --all-features --locked

doc:
	RUSTDOCFLAGS='-D warnings' $(CARGO) doc --workspace --all-features --no-deps --locked

header:
	$(CBINDGEN) --config cbindgen.toml crates/api/ffi -o include/upse.h

check-header:
	$(CBINDGEN) --config cbindgen.toml --verify crates/api/ffi -o include/upse.h
	$(CC) -std=c11 -Wall -Wextra -Werror -pedantic -fsyntax-only -x c include/upse.h
	$(CXX) -std=c++17 -Wall -Wextra -Werror -pedantic -fsyntax-only -x c++ include/upse.h

rust-library:
	$(CARGO) build -p upse-ffi --release --locked

$(BUILD_DIR):
	mkdir -p $@

$(STATIC_LIBRARY): rust-library | $(BUILD_DIR)
	cp $(RUST_STATIC) $@

$(SHARED_LIBRARY): rust-library crates/api/ffi/libupse-ng.map | $(BUILD_DIR)
	$(CC) -shared -Wl,-soname,$(SHARED_SONAME) \
		-Wl,--version-script=crates/api/ffi/libupse-ng.map \
		-Wl,--whole-archive $(RUST_STATIC) -Wl,--no-whole-archive \
		-o $@ -ldl -lpthread -lm
	ln -sf $(notdir $(SHARED_LIBRARY)) $(BUILD_DIR)/$(SHARED_SONAME)
	ln -sf $(SHARED_SONAME) $(BUILD_DIR)/libupse-ng.so

libraries: $(STATIC_LIBRARY) $(SHARED_LIBRARY)

check-exports: $(SHARED_LIBRARY)
	sh tools/check-exports.sh $(SHARED_LIBRARY) crates/api/ffi

$(PKGCONFIG_FILE): pkgconfig/libupse-ng.pc.in | $(BUILD_DIR)
	sed -e 's|@PREFIX@|$(PREFIX)|g' \
		-e 's|@EXEC_PREFIX@|$(EXEC_PREFIX)|g' \
		-e 's|@LIBDIR@|$(LIBDIR)|g' \
		-e 's|@INCLUDEDIR@|$(INCLUDEDIR)|g' \
		-e 's|@VERSION@|$(VERSION)|g' $< >$@

pkgconfig: $(PKGCONFIG_FILE)

upse123: libraries
	$(MAKE) -C examples \
		UPSE_CFLAGS='-I$(CURDIR)/include' \
		UPSE_LIBS='-L$(CURDIR)/$(BUILD_DIR) -Wl,-rpath,$(CURDIR)/$(BUILD_DIR) -lupse-ng'

check-c-api:
	sh tests/c-api/run.sh

check-c-api-sanitize:
	sh tests/c-api/run.sh sanitize

install: all
	$(INSTALL) -d $(DESTDIR)$(LIBDIR) $(DESTDIR)$(INCLUDEDIR) \
		$(DESTDIR)$(PKGCONFIGDIR) \
		$(DESTDIR)$(PREFIX)/share/licenses/libupse-ng
	$(INSTALL) -m 644 $(STATIC_LIBRARY) $(DESTDIR)$(LIBDIR)/libupse-ng.a
	$(INSTALL) -m 755 $(SHARED_LIBRARY) $(DESTDIR)$(LIBDIR)/$(notdir $(SHARED_LIBRARY))
	ln -sf $(notdir $(SHARED_LIBRARY)) $(DESTDIR)$(LIBDIR)/$(SHARED_SONAME)
	ln -sf $(SHARED_SONAME) $(DESTDIR)$(LIBDIR)/libupse-ng.so
	$(INSTALL) -m 644 include/upse.h $(DESTDIR)$(INCLUDEDIR)/upse.h
	$(INSTALL) -m 644 $(PKGCONFIG_FILE) $(DESTDIR)$(PKGCONFIGDIR)/libupse-ng.pc
	$(INSTALL) -m 644 LICENSES/LGPL-2.1-or-later.txt \
		$(DESTDIR)$(PREFIX)/share/licenses/libupse-ng/LGPL-2.1-or-later.txt

install-upse123: upse123
	$(MAKE) -C examples install DESTDIR='$(DESTDIR)' PREFIX='$(PREFIX)'

uninstall:
	rm -f $(DESTDIR)$(LIBDIR)/libupse-ng.a \
		$(DESTDIR)$(LIBDIR)/libupse-ng.so \
		$(DESTDIR)$(LIBDIR)/$(SHARED_SONAME) \
		$(DESTDIR)$(LIBDIR)/$(notdir $(SHARED_LIBRARY)) \
		$(DESTDIR)$(INCLUDEDIR)/upse.h \
		$(DESTDIR)$(PKGCONFIGDIR)/libupse-ng.pc \
		$(DESTDIR)$(PREFIX)/share/licenses/libupse-ng/LGPL-2.1-or-later.txt

clean:
	$(MAKE) -C examples clean
	$(CARGO) clean
	rm -rf $(BUILD_DIR)
