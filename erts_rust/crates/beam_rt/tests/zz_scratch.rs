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

//! SCRATCH debugging — remove before finishing.
use beam_rt::Runtime;

#[test]
fn scratch_port_receive_term() {
    let mut rt = Runtime::with_defaults().expect("rt");
    let expr = "begin P = open_port({spawn,\"echo hi\"}, [binary]), \
                receive M -> M after 500 -> no_data end end";
    let t0 = std::time::Instant::now();
    let message = rt.eval(expr).expect("eval ok");
    eprintln!("elapsed={:?}", t0.elapsed());
    eprintln!("term={:?}", message);
    eprintln!("formatted={:?}", rt.format_term(message, None));
    let printed = rt.eval(
        "begin P = open_port({spawn,\"echo hi\"}, [binary]), \
         receive M -> io:format(\"GOT ~p~n\",[M]) after 500 -> io:format(\"NODATA~n\") end end",
    );
    eprintln!("printed={:?}", printed);
    let direct = rt.eval(
        "begin P = open_port({spawn,\"echo hi\"}, [binary]), \
         receive M -> M after 500 -> no_data end end",
    );
    match direct {
        Ok(t) => eprintln!("direct=Ok({t:?}) fmt={:?}", rt.format_term(t, None)),
        Err(e) => eprintln!("direct=Err({e:?})"),
    }
}
