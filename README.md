<!-- SPDX-License-Identifier: LGPL-2.1-or-later -->
# UPSE-NG

UPSE-NG is a PSF and PSF2 music player written in Rust focused on accuracy.

It provides a Rust library and a C API which deliver stereo 32-bit
floating-point samples through a callback.

No firmware is required or supported: all firmware is emulated using HLE.

The source tree also includes `upse123`, a small libao player, an Audacious
input plugin, and some synthesized test fixtures which have been used to
compare our behavioral model against real hardware.

## Building

UPSE-NG requires:

- Rust >= 1.85
- any C11-capable compiler
- GNU make

If you are modifying UPSE-NG, you also need:

- cbindgen >= 0.29.4

Build the library with:

```sh
make
```

Run the complete test suite with:

```sh
make check
```

The optional players require their development packages:

- `make upse123` requires libao.
- `make audacious` requires Audacious 4.6.

## Installing

Build as an ordinary user before running the installation targets:

```sh
make
doas make install
```

The optional players are installed separately:

```sh
make upse123
doas make install-upse123

make audacious
doas make install-audacious
```

`PREFIX`, `LIBDIR`, `INCLUDEDIR`, `BINDIR`, and `DESTDIR` may be overridden as
usual.

A pkg-config module is also included named `libupse-ng`.

## License

The libraries and Audacious plugin are licensed under LGPL-2.1-or-later.

`upse123`, which is derived from the original UPSE player, is licensed under
GPL-2.0-or-later.
