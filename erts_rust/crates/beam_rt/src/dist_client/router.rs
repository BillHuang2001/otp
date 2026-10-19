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

//! [`DistRouter`]: the [`DistSignalRouter`] implementation the runtime installs on its **process
//! table**, which routes the process layer's outbound link/monitor/exit signals onto the client's
//! outbound queue.
//!
//! The router lives inside the process table, which the runtime moves out of itself for the
//! duration of a scheduler pass, so it cannot borrow the runtime (nor its atom table): it shares
//! the client's `Rc<RefCell<…>>` cell and only *materialises* the signal
//! ([`DistClientInner::queue_signal`](super::DistClientInner)). Encoding and the socket
//! write happen in `DistClient::pump`, the one place the runtime's atom table is live — see
//! [`super`]'s module docs.

use std::cell::RefCell;
use std::rc::Rc;

use beam_common::Result;
use beam_process::{DistSignal, DistSignalRouter};

use crate::error;

use super::DistClientInner;

/// The [`DistSignalRouter`] implementation over a shared [`DistClientInner`].
pub(crate) struct DistRouter {
    /// The shared client state installed on the runtime's process table.
    pub(super) inner: Rc<RefCell<DistClientInner>>,
}

impl DistSignalRouter for DistRouter {
    fn route(&mut self, signal: DistSignal) -> Result<()> {
        let mut inner = self
            .inner
            .try_borrow_mut()
            .map_err(|_| error::message("distribution: the client is already in use"))?;
        inner.queue_signal(signal)
    }
}
