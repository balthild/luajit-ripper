luajit-ripper
=============

A LuaJIT 2.1 bytecode decompiler library, written in Rust.

This crate is a port of LJD (LuaJIT Raw-Bytecode Decompiler) by Andrian Nord
and contributors. The original project is licensed under the MIT license; the
fork this port is based on is distributed under the GNU General Public License
version 3, which is why this crate is licensed under the GPLv3 as well.

The bytecode dump format implemented here is the one documented in LuaJIT's
`lj_bcdump.h`; LuaJIT itself is Copyright (C) 2005-2026 Mike Pall and released
under the MIT license.
