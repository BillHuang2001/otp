%%
%% %CopyrightBegin%
%%
%% SPDX-License-Identifier: Apache-2.0
%%
%% Copyright Ericsson AB 2025-2026. All Rights Reserved.
%%
%% Licensed under the Apache License, Version 2.0 (the "License");
%% you may not use this file except in compliance with the License.
%% You may obtain a copy of the License at
%%
%%     http://www.apache.org/licenses/LICENSE-2.0
%%
%% Unless required by applicable law or agreed to in writing, software
%% distributed under the License is distributed on an "AS IS" BASIS,
%% WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
%% See the License for the specific language governing permissions and
%% limitations under the License.
%%
%% %CopyrightEnd%
%%
%% The **NIF module** half of the A.3 `enif_monitor_process` / `down` fixture for
%% `beam_rt`'s `tests/nif_monitor.rs`: one stub per entry of `tests/fixtures/nif_monitor.c`,
%% loaded by the driver module through `init/1` (`erlang:load_nif/2`).
%%
%% Kept in its own module (not the caller's) so every call below is an **external** call,
%% which is where the runtime's NIF dispatch lives; a local call would run the stub and raise
%% `nif_error`.
-module(nif_monitor).
-export([init/1, make_res/0, arm/2, demonitor/1, downs/0, down_pid_matches/1, alive/1,
         arm_undefined/0]).

init(Path) -> erlang:load_nif(Path, 0).

make_res() -> erlang:nif_error(nif_not_loaded).
arm(_Res, _Pid) -> erlang:nif_error(nif_not_loaded).
demonitor(_Res) -> erlang:nif_error(nif_not_loaded).
downs() -> erlang:nif_error(nif_not_loaded).
down_pid_matches(_Pid) -> erlang:nif_error(nif_not_loaded).
alive(_Pid) -> erlang:nif_error(nif_not_loaded).
arm_undefined() -> erlang:nif_error(nif_not_loaded).
