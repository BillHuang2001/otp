/*
 * %CopyrightBegin%
 *
 * SPDX-License-Identifier: Apache-2.0
 *
 * Copyright Ericsson AB 2025-2026. All Rights Reserved.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
 * %CopyrightEnd%
 */

/*
 * A tiny, self-contained shared object for the `beam_io` dynamic-library primitive
 * (`src/ddll.rs`) integration tests.
 *
 * Unlike `host_driver.c` this fixture imports NOTHING — it only exports — so `dlopen(RTLD_NOW)`
 * always succeeds and the test exercises the *successful* path of the primitive (open, resolve a
 * symbol, call it, close) as well as the honest `dlsym` miss for a name it does not carry.
 *
 * The compiled object is a plain shared library, NOT a driver: it has no `driver_init`, which is
 * deliberate — `DynLib` is the raw `dlopen`/`dlsym`/`dlclose` primitive, so it must not care what
 * the object is, only whether the loader can bind it.
 */

/* A data symbol the test resolves with `DynLib::symbol`. */
int beam_io_ddll_fixture_value = 42;

/* A function symbol the test resolves and calls through a transmuted pointer. */
int beam_io_ddll_fixture_double(int x)
{
    return x * 2;
}
