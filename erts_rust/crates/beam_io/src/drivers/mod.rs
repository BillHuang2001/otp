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

//! Built-in drivers of the Rust ERTS rewrite.
//!
//! Milestone-1 ships the built-in byte-stream driver [`FdDriver`] (registered under the
//! name `"fd"`), which is enough to prove the poller→port→driver path end to end, plus
//! the [`EfileDriver`] (`"efile"`) built on top of it, the OS-child [`SpawnDriver`]
//! (`"spawn"`, the Rust counterpart of the C `spawn` driver) and the TCP socket
//! [`TcpInetDriver`] (`"tcp_inet"`, the Rust counterpart of the C TCP driver
//! `gen_tcp` sits on), plus the in-memory [`RamFileDriver`] (`"ram_file_drv"`, the Rust
//! counterpart of the C driver `ram_file.erl` talks to). The legacy C's `forker` driver and
//! the `inet` driver's UDP/SCTP/IPv6 halves (`erts/emulator/drivers/common/*.c`) are
//! documented follow-ups (see the crate `CONTEXT.md`, Known Issues).
//!
//! `SpawnDriver` is constructed **directly** from a [`SpawnSpec`] rather than through the
//! [`crate::DriverRegistry`]: a [`DriverFactory`](crate::DriverFactory) is a no-argument
//! function, so it cannot carry the command/argv/options a spawn port needs.
//!
//! `FdDriver` also carries the two framing facilities a real port driver needs, in
//! dedicated modules:
//!
//! * [`line_buffer`] — the `{line, N}` line-oriented buffering of `open_port/2`;
//! * [`binary_io`] — the ERTS port *phase change* between list/text and raw binary
//!   framing (`ERTS_PORT_SFLG_BINARY_IO`); and
//! * [`efile`] — the `efile` driver: `FdDriver` in read-only/write-only form plus the
//!   `efile` control family; and
//! * [`inet`] — the `tcp_inet` socket driver: one TCP socket behind one port, with the
//!   `gen_tcp` control vocabulary (`open`/`bind`/`listen`/`accept`/`connect`/`send`/
//!   `recv`/`shutdown`/`close`) and the socket-option verbs
//!   (`setopts`/`getopts`: `nodelay`/`keepalive`/`recbuf`/`sndbuf`); and
//! * [`ram_file`] — the `ram_file_drv` in-memory file driver: a command-driven port holding
//!   a byte buffer, with the packed `port_command/2` opcode protocol
//!   `lib/kernel/src/ram_file.erl` drives (`open`/`read`/`write`/`pread`/`pwrite`/
//!   `lseek`/`truncate`/`get`/`size`/…).

pub mod binary_io;
pub mod efile;
pub mod fd;
pub mod inet;
pub mod line_buffer;
pub mod ram_file;
pub mod spawn;

pub use crate::drivers::binary_io::{Phase, PhaseChange};
pub use crate::drivers::efile::{make_efile, EfileDriver, FileDataMode};
pub use crate::drivers::fd::FdDriver;
pub use crate::drivers::inet::{
    make_tcp_inet, InetState, TcpInetDriver, DRIVER_NAME, INET_AF_ANY, INET_AF_INET,
    INET_AF_LOOPBACK, INET_OPT_KEEPALIVE, INET_OPT_RCVBUF, INET_OPT_SNDBUF, INET_PROTO_DEFAULT,
    INET_PROTO_TCP, INET_REP_ERROR, INET_REP_OK, INET_TYPE_STREAM, TCP_OPT_NODELAY, TCP_SHUT_RD,
    TCP_SHUT_RDWR, TCP_SHUT_WR,
};
pub use crate::drivers::line_buffer::{LineBuffer, DEFAULT_MAX_LINE_LEN};
pub use crate::drivers::ram_file::{
    make_ram_file, RamFileDriver, DRIVER_NAME as RAM_FILE_DRIVER_NAME,
};
pub use crate::drivers::spawn::{
    build_command, SpawnDriver, SpawnOptions, SpawnSpec, SpawnStream, StreamOptions,
};
