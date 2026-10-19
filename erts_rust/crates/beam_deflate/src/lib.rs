// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%
//! zlib-faithful `deflate`/`inflate` in safe Rust — the workspace's one zlib port.
//!
//! Ground truth is the C this repository already ships (`../../erts/emulator/zlib/`,
//! zlib 1.3.2) plus the two Erlang-facing layers that consume it
//! (`../../erts/emulator/nifs/common/zlib_nif.c` and `../../erts/preloaded/src/zlib.erl`).
//! The port exists because `zlib` output is an **observable**: `zlib:compress/1`,
//! `zlib:zip/1`, `zlib:gzip/1`, the `zlib` stream API and ETF's `COMPRESSED` tag all
//! hand zlib's byte string to the user, so a merely conforming encoder is not enough.
//!
//! This crate owns bytes only — no terms, no BIF registry, no host state. The
//! consumers are `beam_codec` (ETF `COMPRESSED`) and `beam_bifs` (`zlib` BIFs).
