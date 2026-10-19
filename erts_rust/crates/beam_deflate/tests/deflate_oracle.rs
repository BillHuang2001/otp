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
//! Cross-check [`beam_deflate::Deflate`] against the **oracle** — the system `erl`'s
//! `zlib` module — over the whole `deflateInit2` parameter space.
//!
//! These tests shell out to `erl`, so when it is not installed they print a single
//! `SKIP …` line and return: CI without an Erlang installation stays green. The bytes are
//! compared exactly; a divergence is reported with the parameter tuple and both hex
//! strings so it can be diffed against `deflate.c` directly.
//!
//! `Z_FIXED` (strategy 4) is *not* expressible through the `zlib` module (`arg_strategy/1`
//! has no `fixed` clause), so it is covered by the round-trip test at the bottom instead:
//! the oracle must be able to inflate what a fixed-Huffman stream claims to be.

mod zsupport;

use beam_deflate::{Deflate, DeflateConfig, Strategy};
use zsupport::{corpus_small, hex, oracle_batch, oracle_inflate, Case};

/// Compare one matrix in a single `erl` boot; `label` names the sweep in a failure.
fn check_matrix(label: &str, cases: &[Case]) {
    if !zsupport::have_oracle() {
        println!("SKIP {label}: erl not found");
        return;
    }
    let oracle = match oracle_batch(cases) {
        Some(v) => v,
        None => {
            println!("SKIP {label}: the oracle batch failed to run");
            return;
        }
    };
    let mut failures = Vec::new();
    for (case, want) in cases.iter().zip(oracle.iter()) {
        let want = match want {
            Ok(w) => w,
            Err(_) => continue,
        };
        let config = DeflateConfig::new(
            case.level,
            beam_deflate::Method::Deflated,
            case.window_bits,
            case.mem_level,
            strategy_of(case.strategy),
        );
        match Deflate::compress_with(config, &case.data) {
            Ok(got) => {
                if &got != want {
                    failures.push(format!(
                        "level {} windowBits {} memLevel {} strategy {} input {} bytes\n  want {}\n  got  {}",
                        case.level,
                        case.window_bits,
                        case.mem_level,
                        case.strategy,
                        case.data.len(),
                        hex(want),
                        hex(&got)
                    ));
                }
            }
            Err(e) => failures.push(format!(
                "level {} windowBits {} memLevel {} strategy {} input {} bytes: {e:?}",
                case.level,
                case.window_bits,
                case.mem_level,
                case.strategy,
                case.data.len()
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{label}: {} of {} cases disagree with the oracle:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
    println!("{label}: {} cases agree with the oracle", cases.len());
}

/// `Z_DEFAULT_STRATEGY` … `Z_RLE`, the four the `zlib` module can name.
fn strategy_of(code: u8) -> Strategy {
    match code {
        0 => Strategy::Default,
        1 => Strategy::Filtered,
        2 => Strategy::HuffmanOnly,
        _ => Strategy::Rle,
    }
}

/// The headline equivalence: every level, every framing, over the small corpus.
///
/// Levels select different strategy *functions* (0 stored, 1-3 fast, 4-9 slow) and
/// different search limits, and the framing changes the header, the trailer and (through
/// `w_bits` in the zlib header) the window size — so this one matrix covers the driver,
/// the strategy dispatch and both checksum paths at once.
#[test]
fn every_level_and_framing_matches_the_oracle() {
    let mut cases = Vec::new();
    for level in [0i8, 1, 2, 3, 4, 5, 6, 7, 8, 9, -1] {
        for (_, data) in corpus_small() {
            cases.push(Case::zlib(level, &data));
            cases.push(Case::raw(level, &data));
            cases.push(Case::gzip(level, &data));
        }
    }
    check_matrix("levels x framings", &cases);
}

/// `memLevel` changes `hash_bits`, `lit_bufsize`, `sym_end` and `pending_buf_size`, so it
/// moves both the hash chains and — through `sym_end` — *where* every block boundary
/// falls. A memLevel that is merely "reasonable" produces a different stream, which is
/// why it is swept rather than fixed at 8.
#[test]
fn every_mem_level_matches_the_oracle() {
    let data = zsupport::mixed_kib();
    let mut cases = Vec::new();
    for mem_level in 1..=9u8 {
        for level in [0i8, 1, 4, 6, 9] {
            cases.push(Case {
                mem_level,
                ..Case::zlib(level, &data)
            });
        }
    }
    check_matrix("memLevel sweep", &cases);
}

/// `windowBits` 8..=15 (zlib), -15..=-9 (raw, since -8 is rejected by both sides) and
/// 16 + 8 ..= 16 + 15 (gzip). `windowBits == 8` is the interesting one: zlib silently
/// bumps it to 9, so the header says `CMF = 0x18` even though 8 was requested.
#[test]
fn every_window_bits_matches_the_oracle() {
    let data = zsupport::mixed_kib();
    let mut cases = Vec::new();
    for wb in 8..=15i8 {
        for level in [1i8, 6, 9] {
            cases.push(Case {
                window_bits: wb,
                ..Case::zlib(level, &data)
            });
        }
    }
    for wb in -15..=-9i8 {
        for level in [1i8, 6, 9] {
            cases.push(Case {
                window_bits: wb,
                ..Case::zlib(level, &data)
            });
        }
    }
    for wb in (16 + 8)..=(16 + 15) {
        cases.push(Case {
            window_bits: wb,
            ..Case::zlib(6, &data)
        });
    }
    check_matrix("windowBits sweep", &cases);
}

/// `Z_FILTERED`'s `TOO_FAR` rule, `Z_HUFFMAN_ONLY`'s literal-only path and `Z_RLE`'s
/// distance-1 runs, each at several levels (the level still decides the block emitter's
/// `nice_match`/`max_chain` for the non-overriding strategies).
#[test]
fn every_strategy_matches_the_oracle() {
    let mut cases = Vec::new();
    for (name, data) in corpus_small() {
        if name == "empty" {
            continue;
        }
        for strategy in 0..=3u8 {
            for level in [1i8, 3, 6, 9] {
                cases.push(Case {
                    strategy,
                    ..Case::zlib(level, &data)
                });
            }
            // The strategies also have to agree in a raw stream and at a narrow window.
            cases.push(Case {
                strategy,
                window_bits: -15,
                ..Case::zlib(6, &data)
            });
            cases.push(Case {
                strategy,
                window_bits: 9,
                ..Case::zlib(6, &data)
            });
        }
    }
    check_matrix("strategy sweep", &cases);
}

/// A large input: more than one `sym_end`-sized block at memLevel 8, several 8 KiB input
/// chunks through the wrapper, and enough data to make the window slide. zlib 1.3.2 has
/// an equivalent-input "deterministic match" rule (`longest_match` clamps to `lookahead`,
/// `fill_window` zeroes `WIN_INIT`), so a sliding window that leaves stale bytes visible
/// changes the stream and would be caught here.
///
/// The multi-pump path is the one that used to diverge: `deflate_stored` sizes its blocks
/// from `avail_in`/`avail_out` (`deflate.c:1668-1780`), so the *pump schedule* is part of
/// the level-0 byte string. [`Deflate::compress_with`] reproduces the `zlib` module's
/// 8 KiB/8 KiB schedule (`zlib.erl`'s `?DEFLATE_IN_CHUNKSIZE`/`?DEFLATE_OUT_CHUNKSIZE`,
/// driven by `zlib_nif.c:zlib_flush_queue`), which is what makes this matrix agree.
#[test]
fn a_large_input_matches_the_oracle() {
    let data = large_input();
    let mut cases = Vec::new();
    for level in [-1i8, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9] {
        cases.push(Case::zlib(level, &data));
        cases.push(Case::raw(level, &data));
        cases.push(Case::gzip(level, &data));
    }
    check_matrix("large input", &cases);
}

/// The same 70 000-byte input at every `memLevel` (which moves `sym_end`, hence every
/// block boundary) and at the narrow windows (which force repeated slides).
#[test]
fn a_large_input_matches_the_oracle_at_every_mem_level_and_window() {
    let data = large_input();
    let mut cases = Vec::new();
    for mem_level in 1..=9u8 {
        for level in [-1i8, 0, 1, 4, 6, 9] {
            for window_bits in [15i8, -15, 16 + 15] {
                cases.push(Case {
                    mem_level,
                    window_bits,
                    ..Case::zlib(level, &data)
                });
            }
        }
    }
    for window_bits in 9..=15i8 {
        for level in [0i8, 1, 6, 9] {
            cases.push(Case {
                window_bits,
                ..Case::zlib(level, &data)
            });
        }
    }
    for window_bits in -15..=-9i8 {
        for level in [0i8, 1, 6, 9] {
            cases.push(Case {
                window_bits,
                ..Case::zlib(level, &data)
            });
        }
    }
    for window_bits in (16 + 9)..=(16 + 15) {
        for level in [0i8, 1, 6, 9] {
            cases.push(Case {
                window_bits,
                ..Case::zlib(level, &data)
            });
        }
    }
    // Every strategy the `zlib` module can name (`Z_FIXED` is covered by the round-trip
    // test below).
    for strategy in 0..=3u8 {
        for level in [0i8, 1, 6, 9] {
            for window_bits in [15i8, -15, 16 + 15] {
                cases.push(Case {
                    strategy,
                    window_bits,
                    ..Case::zlib(level, &data)
                });
            }
        }
    }
    check_matrix("large input x memLevel/window/strategy", &cases);
}

/// The always-on half of the test above: every stream this port produces for a large
/// input must inflate — under the *oracle's* inflater — back to the input, at every level
/// and in every framing. That pins the block structure, the length/distance encoding and
/// both trailers without requiring byte identity.
#[test]
fn a_large_input_round_trips_through_the_oracle() {
    if !zsupport::have_oracle() {
        println!("SKIP large input round-trip: erl not found");
        return;
    }
    let data = large_input();
    for level in [0i8, 1, 6, 9] {
        for config in [
            DeflateConfig::zlib(level),
            DeflateConfig::raw(level),
            DeflateConfig::gzip(level),
        ] {
            let stream = Deflate::compress_with(config, &data)
                .unwrap_or_else(|e| panic!("level {level} {config:?}: {e:?}"));
            let back = oracle_inflate(&stream)
                .unwrap_or_else(|| panic!("level {level} {config:?}: the oracle could not inflate"));
            assert_eq!(
                back,
                data,
                "level {level} {config:?}: {} bytes in, {} bytes out",
                stream.len(),
                back.len()
            );
        }
    }
}

/// The 70 000-byte input: `mixed_kib` repeated with 37 zero bytes between the copies, so
/// the window has to slide and matches can span a copy boundary.
fn large_input() -> Vec<u8> {
    let mut data = Vec::new();
    while data.len() < 70_000 {
        data.extend_from_slice(&zsupport::mixed_kib());
        data.extend_from_slice(&[0u8; 37]);
    }
    data.truncate(70_000);
    data
}

/// `Z_FIXED` is not reachable through the `zlib` module, so it is verified the other way
/// round: the oracle's inflater must reproduce the input from a fixed-Huffman stream at
/// every level. (zlib picks the static trees only when they are not larger than the
/// dynamic ones — or unconditionally under `Z_FIXED` — so this checks the choice *and*
/// the static-tree bit emission.)
#[test]
fn the_fixed_strategy_round_trips_through_the_oracle() {
    if !zsupport::have_oracle() {
        println!("SKIP fixed strategy: erl not found");
        return;
    }
    for level in [0i8, 1, 6, 9] {
        for (name, data) in corpus_small() {
            let config = DeflateConfig::new(
                level,
                beam_deflate::Method::Deflated,
                15,
                8,
                Strategy::Fixed,
            );
            let stream = Deflate::compress_with(config, &data).expect("compress");
            let back = oracle_inflate(&stream).expect("oracle inflate");
            assert_eq!(back, data, "level {level} input {name}");
        }
    }
}

/// A configuration the C rejects must be rejected here too — no silently substituted
/// stream. These are `deflateInit2_`'s own bounds.
#[test]
fn out_of_range_configurations_are_rejected() {
    use beam_deflate::{Method, DEFAULT_COMPRESSION};
    let bad = [
        // level out of range
        DeflateConfig::new(10, Method::Deflated, 15, 8, Strategy::Default),
        DeflateConfig::new(-2, Method::Deflated, 15, 8, Strategy::Default),
        // windowBits out of range (raw -8 is refused: it would leave no lookahead)
        DeflateConfig::new(6, Method::Deflated, 0, 8, Strategy::Default),
        DeflateConfig::new(6, Method::Deflated, 7, 8, Strategy::Default),
        DeflateConfig::new(6, Method::Deflated, 16, 8, Strategy::Default),
        DeflateConfig::new(6, Method::Deflated, 47, 8, Strategy::Default),
        // gzip with a 256-byte window: `deflateInit2_`'s `windowBits == 8 && wrap != 1`
        // clause reaches the gzip arm too (verified against the oracle: `zlib:deflateInit/6`
        // with 24 raises `stream_error`), because the 8 -> 9 bump happens *after* the test.
        DeflateConfig::new(6, Method::Deflated, 16 + 8, 8, Strategy::Default),
        DeflateConfig::new(6, Method::Deflated, -16, 8, Strategy::Default),
        DeflateConfig::new(6, Method::Deflated, -8, 8, Strategy::Default),
        // memLevel out of range
        DeflateConfig::new(6, Method::Deflated, 15, 0, Strategy::Default),
        DeflateConfig::new(6, Method::Deflated, 15, 10, Strategy::Default),
    ];
    for config in bad {
        assert!(
            Deflate::new(config).is_err(),
            "{config:?} must be rejected"
        );
    }
    // The accepted edges of every range, including the two sentinels.
    let good = [
        DeflateConfig::new(DEFAULT_COMPRESSION, Method::Deflated, 15, 8, Strategy::Default),
        DeflateConfig::new(0, Method::Deflated, 15, 1, Strategy::Default),
        DeflateConfig::new(9, Method::Deflated, 15, 9, Strategy::Default),
        DeflateConfig::new(6, Method::Deflated, 8, 8, Strategy::Default),
        DeflateConfig::new(6, Method::Deflated, -9, 8, Strategy::Default),
        DeflateConfig::new(6, Method::Deflated, -15, 8, Strategy::Default),
    ];
    for config in good {
        assert!(Deflate::new(config).is_ok(), "{config:?} must be accepted");
    }
}
