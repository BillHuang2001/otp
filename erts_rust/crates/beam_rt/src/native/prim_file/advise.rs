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

//! The `prim_file:advise/4` family of the runtime-native `prim_file:*` floor (contract A.7).
//!
//! Serves both the **exported** wrapper's key (`advise/4`, reached by `call_ext`) and the
//! **non-exported** NIF stub `advise_nif/4` that `prim_file.erl`'s own `advise/4` body chains
//! into locally — the second is what `file:sendfile/8` calls before it sends
//! (`prim_file.erl:432-434`).
