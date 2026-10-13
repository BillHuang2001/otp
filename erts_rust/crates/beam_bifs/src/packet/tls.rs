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

//! The `ssl_tls` (`TCP_PB_SSL_TLS`) packet family.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/packet_parser.c:465-476`
//! (the 5-byte `<<ContentType:8, Version:16, Length:16>>` header, with the
//! `Ssl-v2 Client hello` form `<<1:1, Len:15, 1:8, Version:16>>` when the first
//! byte's top bit is set and byte 2 is `1`) and `:486-494` (the shared
//! `remain:` label).
//!
//! # Current subset
//!
//! **This packet type is a typed `unsupported_bif` capability gap at present**:
//! `parse` reports it as a valid packet type whose rules are not implemented
//! yet, and never returns a fabricated packet. Its arguments are still validated
//! by `super::decode_packet_3` before this point, so a bad `Bin`/`Opts` answers
//! `badarg` exactly as the oracle does. The signature below is the frozen family
//! seam — the packet type, the aligned bytes of `Bin` and the parsed options in,
//! an `Outcome` out.

use beam_common::Result;

use crate::context::BifContext;
use crate::error::unsupported_bif;

use super::{Options, Outcome, PacketType};

/// The frozen `ssl_tls` entry point (see the module docs).
pub(super) fn parse(
    _ctx: &mut BifContext<'_>,
    _ptype: PacketType,
    _bytes: &[u8],
    _opts: &Options,
) -> Result<Outcome> {
    Err(unsupported_bif())
}
