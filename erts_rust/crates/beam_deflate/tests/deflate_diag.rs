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
//! Temporary diagnostic: sweep the parameter space on the 70 000-byte input.

mod zsupport;

use beam_deflate::{Deflate, DeflateConfig, Method, Strategy};
use zsupport::{hex, oracle_batch, Case};

fn large_input(n: usize) -> Vec<u8> {
    let mut data = Vec::new();
    while data.len() < n {
        data.extend_from_slice(&zsupport::mixed_kib());
        data.extend_from_slice(&[0u8; 37]);
    }
    data.truncate(n);
    data
}

fn strategy_of(code: u8) -> Strategy {
    match code {
        0 => Strategy::Default,
        1 => Strategy::Filtered,
        2 => Strategy::HuffmanOnly,
        _ => Strategy::Rle,
    }
}

fn run(label: &str, cases: Vec<Case>) {
    let oracle = match oracle_batch(&cases) {
        Some(v) => v,
        None => {
            println!("SKIP {label}: no oracle");
            return;
        }
    };
    let mut bad = 0;
    for (case, want) in cases.iter().zip(oracle.iter()) {
        let want = want.as_ref().unwrap();
        let config = DeflateConfig::new(
            case.level,
            Method::Deflated,
            case.window_bits,
            case.mem_level,
            strategy_of(case.strategy),
        );
        match Deflate::compress_with(config, &case.data) {
            Ok(got) if &got == want => {}
            Ok(got) => {
                bad += 1;
                let at = got.iter().zip(want.iter()).position(|(a, b)| a != b);
                println!(
                    "{label}: MISMATCH level {} wb {} ml {} strat {} in {} -> got {} want {} first {:?}",
                    case.level,
                    case.window_bits,
                    case.mem_level,
                    case.strategy,
                    case.data.len(),
                    got.len(),
                    want.len(),
                    at
                );
                if let Some(p) = at {
                    let lo = p.saturating_sub(6);
                    let hi = (p + 14).min(got.len().min(want.len()));
                    println!("   got  {}\n   want {}", hex(&got[lo..hi]), hex(&want[lo..hi]));
                }
            }
            Err(e) => {
                bad += 1;
                println!("{label}: ERROR {:?} for {case:?}", e);
            }
        }
    }
    println!("{label}: {} cases, {bad} bad", cases.len());
}

#[test]
fn sweep_large() {
    if !zsupport::have_oracle() {
        println!("SKIP: no erl");
        return;
    }
    let data = large_input(70_000);
    let mut cases = Vec::new();
    for level in [0i8, 1, 2, 3, 4, 5, 6, 7, 8, 9] {
        for wb in [15i8, -15, 16 + 15] {
            cases.push(Case {
                window_bits: wb,
                ..Case::zlib(level, &data)
            });
        }
    }
    run("large levels x framings", cases);

    let mut cases = Vec::new();
    for level in [0i8, 1, 6, 9] {
        for strategy in 0..=3u8 {
            for wb in [15i8, -15, 16 + 15] {
                cases.push(Case {
                    window_bits: wb,
                    strategy,
                    ..Case::zlib(level, &data)
                });
            }
        }
    }
    run("large strategies", cases);

    let mut cases = Vec::new();
    for mem_level in 1..=9u8 {
        for level in [0i8, 1, 4, 6, 9] {
            for wb in [15i8, -15] {
                cases.push(Case {
                    window_bits: wb,
                    mem_level,
                    ..Case::zlib(level, &data)
                });
            }
        }
    }
    run("large memLevels", cases);

    let mut cases = Vec::new();
    for wb in 8..=15i8 {
        for level in [0i8, 1, 6, 9] {
            cases.push(Case {
                window_bits: wb,
                ..Case::zlib(level, &data)
            });
        }
    }
    for wb in -15..=-9i8 {
        for level in [0i8, 1, 6, 9] {
            cases.push(Case {
                window_bits: wb,
                ..Case::zlib(level, &data)
            });
        }
    }
    for wb in (16 + 9)..=(16 + 15) {
        for level in [0i8, 1, 6, 9] {
            cases.push(Case {
                window_bits: wb,
                ..Case::zlib(level, &data)
            });
        }
    }
    run("large windowBits", cases);
}

#[test]
fn dump_level0() {
    let data = large_input(70_000);
    for (name, cfg) in [
        ("z0", DeflateConfig::zlib(0)),
        ("r0", DeflateConfig::raw(0)),
        ("g0", DeflateConfig::gzip(0)),
    ] {
        let s = Deflate::compress_with(cfg, &data).unwrap();
        std::fs::write(format!("{}/{name}.bin", std::env::temp_dir().display()), &s).unwrap();
        println!("{name}: {} bytes", s.len());
    }
}
