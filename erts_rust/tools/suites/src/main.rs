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
//! The `suites` binary: parse the args, dispatch, map the outcome onto the exit code.
#![forbid(unsafe_code)]
use std::process::exit;
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match suites::cli::parse(&args) {
        Ok(action) => match suites::run::dispatch(action) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("suites: {err}");
                err.exit_code()
            }
        },
        Err(err) => {
            eprintln!("suites: {err}");
            eprintln!();
            eprintln!("{}", suites::cli::usage());
            err.exit_code()
        }
    };
    exit(code);
}
