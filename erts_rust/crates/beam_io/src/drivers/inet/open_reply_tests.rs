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
//! The `INET_REQ_OPEN`(1) reply on a port that is **not** closed — the C's `exbadseq` ladder.
//!
//! `inet_ctl_open` (`inet_drv.c:5118`-`:5132`) opens with
//! `if (desc->state != INET_STATE_CLOSED) return ctl_xerror(EXBADSEQ, rbuf, rsize);`, so a second
//! `open` is a sequence error and not the generic `einval` an argument fault would answer. The
//! rows here are cross-checked byte-for-byte against the system `erl`; see [`open_reply_rows`].
