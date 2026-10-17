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
//! Probe (temporary) — the whole `udp` corpus surface, both sides, printed.
use super::tests::{fixture, erl_eval};
use super::*;
use crate::config::RuntimeConfig;
use beam_engine::terms;
use beam_term::make_small;

const CONTROL_MAGIC: u32 = 0x03f1_a300;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rule {
    Exact,
    BoundPort,
    PackedAddress,
}

struct Probe {
    label: &'static str,
    code: u32,
    arg: Vec<u8>,
    rule: Rule,
}

fn p(label: &'static str, code: u32, arg: Vec<u8>) -> Probe {
    Probe { label, code, arg, rule: Rule::Exact }
}
fn pb(label: &'static str, code: u32, arg: Vec<u8>, rule: Rule) -> Probe {
    Probe { label, code, arg, rule }
}

/// `code + CONTROL_MAGIC`.
fn step(c: u64) -> u64 {
    c + u64::from(CONTROL_MAGIC)
}

fn rows_bare() -> Vec<Probe> {
    let fake = vec![1u8, 4, 210, 1, 2, 3, 4];
    vec![
        p("gettype_bare", 15, vec![]),
        p("getfd_bare", 14, vec![]),
        p("getstatus_bare", 16, vec![]),
        p("name_bare", 5, vec![]),
        p("peer_bare", 4, vec![]),
        p("recv8_bare", 60, vec![0; 8]),
        p("recv0_bare", 60, vec![]),
        p("tcp_recv42_bare", 42, vec![]),
        p("accept26_bare", 26, vec![]),
        p("listen27_bare", 27, vec![]),
        p("listen27_05_bare", 27, vec![0, 5]),
        p("bind0_bare", 6, vec![]),
        p("bind_any_bare", 6, vec![3, 0, 0]),
        p("connect_short_bare", 3, vec![0, 0]),
        p("connect_empty_bare", 3, vec![]),
        p("open_short_bare", 1, vec![1, 2]),
        p("fdopen0_bare", 13, vec![]),
        p("fdopen_afnosupport_bare", 13, vec![9, 2, 0, 0, 0, 1]),
        p("fdopen_type_bare", 13, vec![1, 9, 0, 0, 0, 1]),
        p("gethostname_bare", 12, vec![]),
        p("gethostname_1_bare", 12, vec![1]),
        p("getservbyname_bare", 17, vec![]),
        p("getservbyport_bare", 18, vec![]),
        p("setname_clear_bare", 19, vec![]),
        p("setpeer_clear_bare", 20, vec![]),
        p("setname_1_bare", 19, vec![1]),
        p("setpeer_1_bare", 20, vec![1]),
        p("setname_eafnosupport_bare", 19, vec![9, 1, 2]),
        p("getiflist_bare", 21, vec![]),
        p("ifget_bare", 22, vec![1]),
        p("ifset_bare", 23, vec![1]),
        p("getifaddrs_bare", 25, vec![]),
        p("ignorefd_bare", 28, vec![1]),
        p("getladdrs_bare", 29, vec![0, 0, 0, 0]),
        p("getladdrs_0_bare", 29, vec![]),
        p("getpaddrs_bare", 30, vec![0, 0, 0, 0]),
        p("getpaddrs_0_bare", 30, vec![]),
        p("setname_fake_bare", 19, fake.clone()),
        p("name_fake_bare", 5, vec![]),
        p("setname_any_bare", 19, vec![3, 4, 210]),
        p("setname_loopback_bare", 19, vec![4, 4, 210]),
        p("setname_clear2_bare", 19, vec![]),
        p("name_after_clear_bare", 5, vec![]),
    ]
}

fn rows_lifecycle() -> Vec<Probe> {
    vec![
        p("open", 1, vec![1, 2, 0]),
        p("setname_any", 19, vec![3, 4, 210]),
        p("name_any", 5, vec![]),
        p("setname_loopback", 19, vec![4, 4, 210]),
        p("name_loopback", 5, vec![]),
        p("setname_clear", 19, vec![]),
        p("name_wildcard", 5, vec![]),
        p("gettype_open", 15, vec![]),
        p("getstatus_open", 16, vec![]),
        p("listen_open", 27, vec![]),
        p("listen_open_05", 27, vec![0, 5]),
        p("getladdrs_open", 29, vec![0, 0, 0, 0]),
        p("getpaddrs_open", 30, vec![0, 0, 0, 0]),
        p("ignorefd_open", 28, vec![1]),
        p("recv_2_open", 60, vec![0, 0]),
        p("connect_2_open", 3, vec![0, 0]),
        p("open_short_open", 1, vec![1, 2]),
        pb("bind", 6, vec![4, 0, 0], Rule::BoundPort),
        pb("name_bound", 5, vec![], Rule::PackedAddress),
        pb("getladdrs_bound", 29, vec![0, 0, 0, 0], Rule::PackedAddress),
        p("getpaddrs_bound", 30, vec![0, 0, 0, 0]),
        p("setopts_broadcast", 7, vec![4, 0, 0, 0, 1]),
        p("getopts_broadcast", 8, vec![4]),
        p("getstat_bound", 11, vec![]),
        p("peer_bound", 4, vec![]),
        p("recv_4_bound", 60, vec![0, 0, 0, 0]),
    ]
}

fn rows_connected() -> Vec<Probe> {
    let fake_local = vec![1u8, 4, 210, 1, 2, 3, 4];
    vec![
        p("open_c", 1, vec![1, 2, 0]),
        pb("bind_c", 6, vec![4, 0, 0], Rule::BoundPort),
        p("connect_c", 3, vec![0, 0, 0, 0, 1, 0, 9, 127, 0, 0, 1]),
        p("getstatus_c", 16, vec![]),
        p("peer_c", 4, vec![]),
        p("getpaddrs_c", 30, vec![0, 0, 0, 0]),
        p("ignorefd_c", 28, vec![1]),
        p("setpeer_fake_c", 20, fake_local.clone()),
        p("peer_fake_c", 4, vec![]),
        p("getpaddrs_fake_c", 30, vec![0, 0, 0, 0]),
        p("setpeer_clear_c", 20, vec![]),
        p("peer_real_c", 4, vec![]),
        p("connect_dissolve_c", 3, vec![]),
        p("getstatus_dissolved_c", 16, vec![]),
        p("close_c", 2, vec![]),
        p("gettype_closed_c", 15, vec![]),
        p("getstatus_closed_c", 16, vec![]),
        p("getfd_closed_c", 14, vec![]),
        p("name_closed_c", 5, vec![]),
        p("close_again_c", 2, vec![]),
    ]
}

fn rows_scope() -> Vec<Probe> {
    vec![
        p("open_s", 1, vec![1, 2, 0]),
        p("tcp_recv42", 42, vec![]),
        p("tcp_unrecv43", 43, vec![]),
        p("tcp_shutdown44", 44, vec![]),
        p("tcp_sendfile45", 45, vec![]),
        p("accept26", 26, vec![]),
        p("wrapped_cmd0", 0, vec![1, 2, 0]),
        p("wrapped_cmd1", 1, vec![1, 2, 0]),
        p("wrapped_cmd99", 99, vec![]),
        p("unassigned_9", 9, vec![]),
        p("subscribe_empty_s", 24, vec![]),
        p("subscribe_1_s", 24, vec![1]),
        p("subscribe_2_s", 24, vec![2]),
        p("recv_wellformed_s", 60, vec![0; 8]),
        p("fdopen_wellformed_s", 13, vec![1, 2, 0, 0, 0, 9]),
        p("getservbyname_wellformed_s", 17, vec![3, 100]),
        p("getservbyport_wellformed_s", 18, vec![0, 53, 3]),
        p("getiflist_s", 21, vec![]),
        p("ifget_s", 22, vec![1]),
        p("ifset_s", 23, vec![1]),
        p("getifaddrs_s", 25, vec![]),
    ]
}

fn rows_tcp() -> Vec<Probe> {
    vec![
        p("tcp_bare_42", 42, vec![1, 2]),
        p("tcp_bare_42_empty", 42, vec![]),
        p("tcp_bare_15", 15, vec![]),
        p("tcp_bare_16", 16, vec![]),
        p("tcp_bare_60", 60, vec![0; 8]),
        p("tcp_bare_27", 27, vec![0, 5]),
        p("tcp_bare_99", 99, vec![]),
    ]
}

struct Rig {
    runtime: Runtime,
    fx: tests::Fixture,
    hook: RuntimePorts,
    port: Option<PortId>,
}

impl Rig {
    fn new() -> Rig {
        let fx = fixture();
        let runtime = Runtime::new(RuntimeConfig::default()).expect("runtime");
        let owner = runtime.self_pid();
        let hook = fx.hook_over(Rc::clone(&runtime.ports), owner);
        Rig { runtime, fx, hook, port: None }
    }
    fn open(&mut self, driver: &str) -> PortId {
        let name = self.fx.string(driver);
        let name = self.fx.name("spawn_driver", name);
        let binary = self.fx.atom("binary");
        let opts = self.fx.list(&[binary]);
        let port = self.hook.open_port(name, opts).expect("open");
        self.port = Some(port);
        port
    }
    fn control(&mut self, code: u64, arg: &[u8]) -> Result<Vec<u8>> {
        let port = self.port.expect("a port");
        let data = terms::make_heap_binary(
            &mut crate::context::RuntimeContext::new(&mut self.runtime),
            arg,
        )
        .expect("binary");
        let reply = crate::native::test_support::call(
            &mut self.runtime,
            "erlang",
            "port_control",
            &[port.to_eterm(), make_small(code as i64), data],
        )?;
        if let Ok(bytes) = terms::binary_bytes(reply) {
            return Ok(bytes.to_vec());
        }
        Ok(terms::list_elements(reply)
            .expect("a list")
            .into_iter()
            .map(|e| u8::try_from(signed_value(e)).expect("byte"))
            .collect())
    }
}

fn oracle_verdicts(driver: &str, rows: &[Probe]) -> Option<Vec<String>> {
    let mut body = String::new();
    for r in rows {
        let arg = r.arg.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
        body.push_str(&format!(
            "io:format(\"{} ~w~n\",[F(P,{},<<{}>>)]),",
            r.label,
            step(u64::from(r.code)),
            arg
        ));
    }
    let expr = format!(
        "begin F = fun(P,V,D) -> try erlang:port_control(P,V,D) of R -> R catch error:badarg -> badarg end end, P = open_port({{spawn_driver,\"{driver}\"}},[binary]), {body} erlang:port_close(P) end"
    );
    let out = erl_eval(&expr)?;
    Some(
        out.lines()
            .map(|line| line.split_once(' ').expect("a label and a verdict").1.to_string())
            .collect(),
    )
}

#[test]
fn probe_all() {
    for (name, rows, driver) in [
        ("bare", rows_bare(), "udp_inet"),
        ("lifecycle", rows_lifecycle(), "udp_inet"),
        ("connected", rows_connected(), "udp_inet"),
        ("scope", rows_scope(), "udp_inet"),
        ("tcp", rows_tcp(), "tcp_inet"),
    ] {
        let mut rig = Rig::new();
        rig.open(driver);
        let rust: Vec<Result<Vec<u8>>> = rows.iter().map(|r| rig.control(step(u64::from(r.code)), &r.arg)).collect();
        let oracle = oracle_verdicts(driver, &rows);
        println!("===== {name} =====");
        match oracle {
            None => println!("NO ORACLE"),
            Some(o) => {
                assert_eq!(o.len(), rows.len(), "oracle lines");
                for (i, r) in rows.iter().enumerate() {
                    let rust_s = match &rust[i] {
                        Ok(b) => format!("{b:?}"),
                        Err(e) => match e {
                            BeamError::BadArg => "badarg".to_string(),
                            other => format!("ERR {other}"),
                        },
                    };
                    let flag = if rust_s == o[i] { " " } else { "!" };
                    println!("{flag} {:<28} oracle={:<40} rust={}", r.label, o[i], rust_s);
                }
            }
        }
    }
}
