<!-- SPDX-License-Identifier: LGPL-2.1-or-later -->
# Synthetic fixtures

`generate.py` creates the PSF, miniPSF, PSF2, and library files used by the C
API and player tests.  The fixtures contain small synthetic PS-X EXE and
IOP IRX programs.  Importantly, they do not contain console firmware or
game data, which should *never* be committed.
