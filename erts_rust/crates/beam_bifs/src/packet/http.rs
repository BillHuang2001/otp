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

//! The http packet family — `http`, `httph`, `http_bin` and `httph_bin`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/packet_parser.c:390-450`
//! (`TCP_PB_HTTP`/`TCP_PB_HTTP_BIN`, the `data \r\n(SP data\r\n)*` rule, and
//! `TCP_PB_HTTPH`/`TCP_PB_HTTPH_BIN`, the header field) plus the http-atom and
//! URI tables in `../../erts/emulator/beam/packet_parser.h:52-100`.
//!
//! # Current subset
//!
//! **All four of these packet types are a typed `unsupported_bif` capability gap
//! at present**: `parse` reports each as a valid packet type whose rules are not
//! implemented yet, and never returns a fabricated packet. Their arguments are
//! still validated by `super::decode_packet_3` before this point, so a bad
//! `Bin`/`Opts` answers `badarg` exactly as the oracle does. The signature below
//! is the frozen family seam — the packet type, the aligned bytes of `Bin` and
//! the parsed options in, an `Outcome` out; a term-building family returns its
//! packet through [`Body::Term`](super::Body::Term).

use beam_common::Result;

use crate::context::BifContext;
use crate::error::unsupported_bif;

use super::{Options, Outcome, PacketType};

/// The frozen http-family entry point (see the module docs).
pub(super) fn parse(
    _ctx: &mut BifContext<'_>,
    _ptype: PacketType,
    _bytes: &[u8],
    _opts: &Options,
) -> Result<Outcome> {
    Err(unsupported_bif())
}
