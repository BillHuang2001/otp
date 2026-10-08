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
 * A shared object that references a symbol NOTHING defines, for the `beam_io` dynamic-library
 * primitive (`src/ddll.rs`) integration tests.
 *
 * A shared object may leave a symbol undefined at link time; with `RTLD_NOW` the loader resolves
 * every undefined symbol eagerly, so `dlopen` fails with
 *   <path>: undefined symbol: beam_io_ddll_undefined_probe
 * unless the loading process exports it. The probe name is unique to this fixture, so no host in
 * this workspace ever defines it — making the "unresolved symbol" failure shape hermetic (it does
 * not depend on whether the test binary was linked with `-Wl,--export-dynamic`).
 */

extern int beam_io_ddll_undefined_probe(void);

int beam_io_ddll_fixture_caller(void)
{
    return beam_io_ddll_undefined_probe();
}
