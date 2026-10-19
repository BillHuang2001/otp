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
//! **Streaming equivalence**: the same stream comes out however the driver is pumped.
//!
//! [`Deflate::compress_with`] is one particular schedule of `deflate()` calls — the one
//! `zlib.erl`/`zlib_nif.c` uses (8 KiB input pieces, 8 KiB output buffers, `Z_NO_FLUSH`
//! while input remains, then `Z_FINISH` with an empty input). This file proves that the
//! *bytes* do not depend on that choice: feeding the whole input at once and draining it
//! through 1-byte, 7-byte or 1 KiB output buffers, or feeding it 1/3/1024 bytes at a time
//! into a large buffer, reproduces the one-shot stream exactly at levels `0,1,6,9` in all
//! three framings.
//!
//! One level is deliberately exempt from that invariance claim, and the exemption is part
//! of the C, not of this port: `deflate_stored` (`deflate.c:1668-1780`) sizes its stored
//! blocks from `avail_in`/`avail_out`, so at **level 0** the pump schedule *is* part of the
//! byte string once the input is large enough for a block to be emitted straight to
//! `next_out` (`left >= min_block`, i.e. 32 KiB at memLevel 8).
//! [`level_0_block_split_follows_the_pump_schedule_like_the_c`] pins that behaviour; the
//! invariance tests below use the small corpus (≤ 4 KiB, where level 0 accumulates in the
//! window and is schedule-invariant) and levels 1..=9 for the 70 000-byte input.
//!
//! # Mid-stream flushes
//!
//! `Sync`/`Full` also have **oracle goldens**: the bytes `erl` produces for
//! `deflateInit(Z, 6)` + `deflate(Z, part1, sync|full)` + `deflate(Z, part2, finish)`.
//! `Partial` and `Block` have no `zlib` binding (`arg_flush/1` names only
//! `none`/`sync`/`full`/`finish`), so they are checked structurally against the `Sync`
//! stream of the same input — which is exactly what the C's flush ladder says they are:
//!
//! ```text
//! block_done branch of deflate() (deflate.c:1548-1575):
//!   Z_PARTIAL_FLUSH  -> _tr_align(s)              (empty *static* block + align)
//!   Z_BLOCK          -> nothing at all            (stop at the block boundary)
//!   Z_SYNC/FULL      -> _tr_stored_block(s,0,0,0) (align + `00 00 FF FF`)
//! ```
//!
//! so the `Block` output is a byte prefix of the `Sync` output, the `Partial` output is the
//! `Block` output plus the aligned empty static block, and only `Sync`/`Full` carry the
//! `00 00 FF FF` marker. Every one of them still inflates to the original once the rest of
//! the stream follows.
//!
//! `deflateParams` has oracle goldens too (6 → 1 and 1 → 9), captured as
//! `deflateInit(Z, 6|1)` + `deflate(Z, part1, sync)` + `deflateParams(Z, level, strategy)`
//! + `deflate(Z, part2, finish)`.
//!
//! # Regenerating the goldens
//!
//! Nothing here calls `erl` at run time except the optional cross-checks. To regenerate
//! the four embedded images (only when the oracle itself changes):
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_deflate --test deflate_stream -- --ignored --nocapture
//! ```
//!
//! That prints them between `=== NAME ===` markers (and asserts that this port produces
//! the same bytes before printing them).

mod zsupport;

use beam_deflate::{decompress_all, Deflate, DeflateConfig, Flush, Status, Strategy};
use zsupport::hex;

/// The pump schedule a stream is driven with: how much input one `deflate()` call may take
/// (`avail_in`) and how big that call's output buffer is (`avail_out`).
#[derive(Clone, Copy, Debug)]
struct Schedule {
    in_chunk: usize,
    out_chunk: usize,
}

impl Schedule {
    const fn new(in_chunk: usize, out_chunk: usize) -> Schedule {
        Schedule {
            in_chunk,
            out_chunk,
        }
    }

    /// The `zlib` module's own schedule (`?DEFLATE_IN_CHUNKSIZE`/`?DEFLATE_OUT_CHUNKSIZE`,
    /// 8 KiB each) — what [`Deflate::compress_with`] reproduces.
    const NIF: Schedule = Schedule::new(8 << 10, 8 << 10);
}

/// Drive `stream` over `input` with `schedule`, then finish with `last`.
///
/// Input is fed with `Flush::None` in `in_chunk` pieces (every call gets a **fresh**
/// `out_chunk`-sized output buffer, so a call that fills it simply comes back for more),
/// and the terminal flush then runs with an empty input until zlib reports it complete —
/// `Z_STREAM_END` for [`Flush::Finish`], a call that produced less than a full buffer for
/// everything else. That is `zlib_flush_queue`'s shape, with the chunk sizes exposed.
///
/// Returns the terminal flush's status. A stream-level error status is a test failure, so
/// it panics with the status and the schedule instead of being folded away.
fn pump(
    stream: &mut Deflate,
    input: &[u8],
    out: &mut Vec<u8>,
    schedule: Schedule,
    last: Flush,
) -> Status {
    assert!(schedule.in_chunk > 0 && schedule.out_chunk > 0);
    let mut buf = vec![0u8; schedule.out_chunk];
    let mut at = 0;
    while at < input.len() {
        let end = (at + schedule.in_chunk).min(input.len());
        let step = stream
            .deflate(&input[at..end], &mut buf, Flush::None)
            .expect("deflate");
        assert!(
            !step.status.is_error(),
            "feeding {schedule:?}: {:?} on {end} of {} input bytes",
            step,
            input.len()
        );
        assert!(
            step.consumed > 0 || step.produced > 0,
            "feeding {schedule:?} made no progress at {at} of {}",
            input.len()
        );
        at += step.consumed;
        out.extend_from_slice(&buf[..step.produced]);
    }
    loop {
        let step = stream.deflate(&[], &mut buf, last).expect("deflate");
        assert!(
            !step.status.is_error(),
            "{last:?} flush with {schedule:?}: {step:?}"
        );
        out.extend_from_slice(&buf[..step.produced]);
        match step.status {
            Status::StreamEnd => return Status::StreamEnd,
            // A non-final flush is complete once a call fits in the buffer: zlib only
            // reports `Z_BUF_ERROR` for a repeated same-rank flush after a call that left
            // output space, which is the completed case and never re-entered here.
            Status::Ok if last != Flush::Finish => {
                if step.produced < buf.len() {
                    return Status::Ok;
                }
            }
            Status::Ok => {}
            other => panic!("{last:?} flush with {schedule:?}: {other:?}"),
        }
    }
}

/// [`pump`] on a fresh stream, finishing the stream.
fn pump_whole(config: DeflateConfig, input: &[u8], schedule: Schedule) -> Vec<u8> {
    let mut stream = Deflate::new(config).expect("a valid configuration");
    let mut out = Vec::new();
    assert_eq!(
        pump(&mut stream, input, &mut out, schedule, Flush::Finish),
        Status::StreamEnd,
        "a finished stream ends"
    );
    out
}

/// Pump `first` with `mid`, then `second` with [`Flush::Finish`]; return the bytes and the
/// length of the `mid`-flushed prefix (the split point).
fn pump_split(
    config: DeflateConfig,
    first: &[u8],
    second: &[u8],
    mid: Flush,
    schedule: Schedule,
) -> (Vec<u8>, usize) {
    let mut stream = Deflate::new(config).expect("a valid configuration");
    let mut out = Vec::new();
    pump(&mut stream, first, &mut out, schedule, mid);
    let split = out.len();
    assert_eq!(
        pump(&mut stream, second, &mut out, schedule, Flush::Finish),
        Status::StreamEnd
    );
    (out, split)
}

/// Inflate a whole stream with this crate's inflater and assert it is the original.
fn assert_round_trip(stream: &[u8], want: &[u8], what: &str) {
    let (back, used) = decompress_all(15, stream, usize::MAX)
        .unwrap_or_else(|e| panic!("{what}: the stream did not inflate ({e:?})"));
    assert_eq!(back.len(), want.len(), "{what}: inflated length");
    assert_eq!(back, want, "{what}: inflated bytes");
    assert_eq!(
        used,
        stream.len(),
        "{what}: the inflater consumed {} of {} input bytes",
        used,
        stream.len()
    );
}

/// The mid-stream inputs: `mixed_kib()` split in half.
const SPLIT: usize = 2048;

fn split_parts() -> (Vec<u8>, Vec<u8>) {
    let data = zsupport::mixed_kib();
    let (first, second) = data.split_at(SPLIT);
    (first.to_vec(), second.to_vec())
}

// BEGIN GENERATED SPLIT
const SYNC_SPLIT_GOLDEN: &str = concat!(
    "789c7c95694284300c85390a577374dc159971dc4eaf69967e49c11f40499397eda55d2f0fd74ff3344d37cb",
    "9d7c968fe349be8f9797b7f3acefb5e9dc2e5fd0e3fefbfd7136b128a9d0b19eaf7ebe9b86a2c8aa89fef60f",
    "a7e5f3b583854440568fcb030a812ee2574d1c32905c20fef02bd06dad668112762ad15d15ba2977ac0e0c25",
    "1273fd52d0d8dfcf4cfd95aa580225fd021c75684255194ac29425386b280c2deab66eafdeb7882a9723ec45",
    "3301d2b36c6680aa6aff0333e81c2e72102224af649d79cc7eca6e0ac81a6575ec7d758821631028e5df41dc",
    "d4c24d85e8538199711b11c9f30fe90243168aecee6a393dd51c5cce0363cd2199ea51d00bd5592e8a9b2dec",
    "0c1aa4f44546f240199a82492cb447a60cbba452c6bb962a4aefe0ec1f46ca82924f20a2034c27190c0c52a3",
    "543e609be3ca8bbebb3736648bbe33c5e22c4e470b124cc97174c643025572d2e696701a994a048f81dbec4c",
    "61d28e02eacf6a2722c1af44e2f14e5b67d5d8ee72a0e3e8e4d014c6e5db32ae1df90f9f796cfbd519b15416",
    "6068783397b6e0934890b96069fc020000ffff7d974912c3300cc3fcff57f7d25804a8e4d44e26b1ad0da4b9",
    "ff29c16c8564ff6c20cfc6a10ac554dfdcddbc38931ab1ef89824721a0379474e947c51dd200bec9acfdb336",
    "937f0d0420330abb9e2d191add207dcc27c17a9fc37b47abffd79d241c5b818cb771aa3f3bcc93aa08040d7f",
    "4a4573b442093a76cace8a8329e6044ebc48602625964f350822a2e2c2331f82f349868114e3832553a674f8",
    "8595ce6cdba135222893deae5e7274039d3462f84da5eceade35886cc20e3c42113cf8cbcea7ccb68beba012",
    "12fa7405490e2ed877956a3ca812fce116a8b06f5722b85b99b916bce143dce3966b4e16b338b1388bc5ddea",
    "a86f52337b22f86355ad4e9c65319239d11f03003e47b3cc71de4531a655519577bfcf343ffaaec782d751ba",
    "a0e5a2b699a62b39b7e03f35f98dbb",
);
const FULL_SPLIT_GOLDEN: &str = concat!(
    "789c7c95694284300c85390a577374dc159971dc4eaf69967e49c11f40499397eda55d2f0fd74ff3344d37cb",
    "9d7c968fe349be8f9797b7f3acefb5e9dc2e5fd0e3fefbfd7136b128a9d0b19eaf7ebe9b86a2c8aa89fef60f",
    "a7e5f3b583854440568fcb030a812ee2574d1c32905c20fef02bd06dad668112762ad15d15ba2977ac0e0c25",
    "1273fd52d0d8dfcf4cfd95aa580225fd021c75684255194ac29425386b280c2deab66eafdeb7882a9723ec45",
    "3301d2b36c6680aa6aff0333e81c2e72102224af649d79cc7eca6e0ac81a6575ec7d758821631028e5df41dc",
    "d4c24d85e8538199711b11c9f30fe90243168aecee6a393dd51c5cce0363cd2199ea51d00bd5592e8a9b2dec",
    "0c1aa4f44546f240199a82492cb447a60cbba452c6bb962a4aefe0ec1f46ca82924f20a2034c27190c0c52a3",
    "543e609be3ca8bbebb3736648bbe33c5e22c4e470b124cc97174c643025572d2e696701a994a048f81dbec4c",
    "61d28e02eacf6a2722c1af44e2f14e5b67d5d8ee72a0e3e8e4d014c6e5db32ae1df90f9f796cfbd519b15416",
    "6068783397b6e0934890b96069fc020000ffff7d55d942c43008eca7f4d73cd65bababebf5f52ee11a86d497",
    "86260486e1c8ba2ccbe571fb7a596fb6eff5fcf3717790e5e9e2f747d6b7d3fdd5a3080fa7e7d77757d01f91",
    "ec969a380bf2afa7bae57674ef7abb7513228ec3f1893f701d1e55502466a1c153c1d08cd380044ed16a9ca3",
    "f959482a14cce190fcab6ac65a98f59bdbe7e118ab98ed1c87efa163f765616c7135762ccccc52e260dfe353",
    "884b128c37d6d36fe0a4f043508550b30cb8f3164828d61a747d272e6b6becf4d813ada862a910b3c9ba2842",
    "7ce0d540242565a317488908b290f107087721fb4e86e99051a864ce8a1102e03959e8d2d0616bf62c962e26",
    "243bb539894e15645720978af35555d46fcf6ed84862c51a04b9d03c2a49e0c69f78262638db93a07048d0d5",
    "e920c1c62db36f1c38391866b8da19bd62cc50966e5dda2c4acba9eb2410588bb48e38544be231996d4e0ccc",
    "f565304be41fa1ee3d35e9b3049fc6186487555a123bfa9f0628f3198a25e1ec3f8ad0ad145561b2d42af50f",
    "ddeb6d01a30fed86537c823c0c6aed787222e17f35f98dbb",
);
const PARAMS_6_TO_1_GOLDEN: &str = concat!(
    "789c7c95694284300c85390a577374dc159971dc4eaf69967e49c11f40499397eda55d2f0fd74ff3344d37cb",
    "9d7c968fe349be8f9797b7f3acefb5e9dc2e5fd0e3fefbfd7136b128a9d0b19eaf7ebe9b86a2c8aa89fef60f",
    "a7e5f3b583854440568fcb030a812ee2574d1c32905c20fef02bd06dad668112762ad15d15ba2977ac0e0c25",
    "1273fd52d0d8dfcf4cfd95aa580225fd021c75684255194ac29425386b280c2deab66eafdeb7882a9723ec45",
    "3301d2b36c6680aa6aff0333e81c2e72102224af649d79cc7eca6e0ac81a6575ec7d758821631028e5df41dc",
    "d4c24d85e8538199711b11c9f30fe90243168aecee6a393dd51c5cce0363cd2199ea51d00bd5592e8a9b2dec",
    "0c1aa4f44546f240199a82492cb447a60cbba452c6bb962a4aefe0ec1f46ca82924f20a2034c27190c0c52a3",
    "543e609be3ca8bbebb3736648bbe33c5e22c4e470b124cc97174c643025572d2e696701a994a048f81dbec4c",
    "61d28e02eacf6a2722c1af44e2f14e5b67d5d8ee72a0e3e8e4d014c6e5db32ae1df90f9f796cfbd519b15416",
    "6068783397b6e0934890b96069fc020000ffff6d964b8ec3300c43ef5a0cbac8ae40ef8f21f524da71ba4964",
    "9be2474e07e372e96bd1b73271722d199001a87fd7cd42a176b3738af68c85f676620a9775588fac4431d251",
    "a4f87927b147d1818a21969c74050b6b2ceff4bf2201bc79161d8298ca12cdabeec69158efae049d6bafb769",
    "43f2f45698eef768bb8c775c88343b9eadd67e9d3e4eedfa6e106f5ee8ec4958cfad201b0e747c46fe280004",
    "d637506144fc081260dbe80c839f0fc2a740eb9bc18bf8927db9759ebef7ea8e025827a4ddc02d9fd846b54d",
    "90cc0db78d5d16cef6be0404698ece7fcdef7a24bcefb6cd6db2506c5f72cbbba70c8bfe7d375fdbf5c86822",
    "d9ee58bfb63f21a2790ed2e8c3c93141b5ed29b48c14ce3d7d5b1eaf83ae37dcb87cdeaec8e00036d3013e7c",
    "91eb19d37133de63ffa11cbb2868d92c1945f74685020b57dd635a510ee566f336740f44a07233c3818367a4",
    "ea8315325ed036597f036f953b0024c2da7796986aec0ce1302bb49382367bbb0ccca710fb74868f60bb31a4",
    "2ed16b526c4c87fe6eb5fb1f7997a6ecadf05a34d96972c22d5b047a33a739fec61fc799387d33f782575260",
    "7f19c120c0c79c0ae6952968675271a20d9beec4ec8dad4e734c63fa1d19a33cc7437823faa97f065ef5db96",
    "5ec588d34dd22640a9fa0735f98dbb",
);
const PARAMS_1_TO_9_GOLDEN: &str = concat!(
    "78015c514962c34008cb53fcb52ee9deba499b6eaf2f4242061f3cc38024043e5d1e6f9e97c3e170bbdee35a",
    "bf8e67dc4f97d7f78f85e7293177eb0f0ac2f5fae7c371511a2096029a5a2f577fbf0b10544194a9a85f9fd7",
    "ef37689ac10c44882e91b89d60e027292569a54aa05fb0eb09e98c49b38a79ccb0ca64517b25143130533c3d",
    "58e1770b759df050a8553bc17eec1d753e3580410e8a2e00699924a47c78b43e32cce98736a2c64a913ca04d",
    "35bb2a59fb6005c821e8012240710aeca17a97b8c1bd796b5138964146b1b2886b3b7da7db428721fd28e4b4",
    "00ca9644c97ae24cd8e236bf268daba81acbbfc0ce52428d054a0e52f8664ff742c91a08a85ced8c932410f5",
    "6de6e61c82f6a6dd50d0ec1e9878a7b73c00dcf7cc421aca435a5b3604dc8b5cfa61977d2fb80f06d3434415",
    "e9e3d56d273488ca59d6b30fdb5e7d2db26ed033264f0bc065450609eae30c42b9717792a06e1f4d5bbefb3c",
    "b36a9220539f559ee2751c56c5fe5a611b303a6fc311d4d56a2d52ed4fa4e62f2953fb5f63f3824bccbb713d",
    "05b2c77438016dff524a1ed39a27f9a46113c0096b2de35bafae27255dda27003566dd2a354470f04a0018ee",
    "99ada2ba39a30b7b014d42894d09a289e1d24a477edb456c4bc8cb36c63f000000ffff7d974b12c3200c43af",
    "d2fb9fb21b62e9c982553b9904906d7df8617f89c46564cf0b9a9f26e43e38742193daa9ddd4252b19147b33",
    "0a1985024d7d0f3b8ce51b2409fc56e6d8df7ba356074b213272d87a36d7509b86f0477f7260aa4b75e49256",
    "675d15614501c7bbe534fe7431775505100cfc7651a79639c1c64edba972a0660a38e52572954a92492c0604",
    "88ac0b99996d8b2f85d96963519be4ecca29881dbe68655676c7a18a08ce146faf594a74121d0f62f875a7dc",
    "dd9d3528d9143be8119a90c42f3b47254a364a502e12f1691512272eb46fb286326814f89116e8b0b72b11d2",
    "6d84b96d78d207bbc7a186a200af2c369173cb933394741b47bd598df604787a579b442d0b4a3aa31f04803e",
    "dbb0e838775334b606aa95dde759f027bedbb4e0759429a85cd45a681acb9986ff0135f98dbb",
);
// END GENERATED SPLIT

/// The headline invariance: the output-buffer size is not part of the stream.
///
/// A 1-byte buffer forces one `deflate()` call per output byte (and, at level 0, per header
/// byte while the pending block drains), which is where a port that confused `avail_out`
/// with a block boundary would show up.
#[test]
fn the_stream_does_not_depend_on_the_output_buffer_size() {
    for level in [0i8, 1, 6, 9] {
        for (framing, config) in [
            ("zlib", DeflateConfig::zlib(level)),
            ("raw", DeflateConfig::raw(level)),
            ("gzip", DeflateConfig::gzip(level)),
        ] {
            for (name, data) in zsupport::corpus_small() {
                let one_shot = Deflate::compress_with(config, &data).expect("compress");
                for out_chunk in [1usize, 7, 1024] {
                    let schedule = Schedule::new(data.len().max(1), out_chunk);
                    let streamed = pump_whole(config, &data, schedule);
                    assert_eq!(
                        hex(&streamed),
                        hex(&one_shot),
                        "{framing} level {level} input {name}: {schedule:?} disagrees with the \
                         one-shot stream"
                    );
                }
            }
        }
    }
    println!("output-buffer sizes 1/7/1024 reproduce the one-shot stream");
}

/// The other half: the *input* piece size is not part of the stream either. A 1-byte piece
/// makes every call end with `lookahead < MIN_LOOKAHEAD`, so `fill_window` (and the
/// match-availability carry across calls) is what has to keep the state identical.
#[test]
fn the_stream_does_not_depend_on_the_input_piece_size() {
    for level in [0i8, 1, 6, 9] {
        for (framing, config) in [
            ("zlib", DeflateConfig::zlib(level)),
            ("raw", DeflateConfig::raw(level)),
            ("gzip", DeflateConfig::gzip(level)),
        ] {
            for (name, data) in zsupport::corpus_small() {
                let one_shot = Deflate::compress_with(config, &data).expect("compress");
                for in_chunk in [1usize, 3, 1024] {
                    let schedule = Schedule::new(in_chunk, 64 << 10);
                    let streamed = pump_whole(config, &data, schedule);
                    assert_eq!(
                        hex(&streamed),
                        hex(&one_shot),
                        "{framing} level {level} input {name}: {schedule:?} disagrees with the \
                         one-shot stream"
                    );
                }
            }
        }
    }
    println!("input pieces of 1/3/1024 bytes reproduce the one-shot stream");
}

/// Small pieces on **both** sides at once — the tightest buffer pressure (every call has at
/// most a few bytes to give both ways).
#[test]
fn small_input_pieces_and_small_output_buffers_agree() {
    for level in [0i8, 1, 6, 9] {
        for (framing, config) in [
            ("zlib", DeflateConfig::zlib(level)),
            ("raw", DeflateConfig::raw(level)),
            ("gzip", DeflateConfig::gzip(level)),
        ] {
            let data = zsupport::mixed_kib();
            let one_shot = Deflate::compress_with(config, &data).expect("compress");
            for in_chunk in [1usize, 3] {
                for out_chunk in [1usize, 7] {
                    let schedule = Schedule::new(in_chunk, out_chunk);
                    let streamed = pump_whole(config, &data, schedule);
                    assert_eq!(
                        hex(&streamed),
                        hex(&one_shot),
                        "{framing} level {level}: {schedule:?} disagrees with the one-shot stream"
                    );
                }
            }
        }
    }
    println!("small in and out chunks reproduce the one-shot stream");
}

/// The same invariance on the 70 000-byte input at levels `1..=9`, where the window slides
/// and every block boundary is decided by `sym_next` rather than by a buffer.
///
/// Level 0 is excluded on purpose — see
/// [`level_0_block_split_follows_the_pump_schedule_like_the_c`].
#[test]
fn the_stream_does_not_depend_on_the_schedule_for_a_large_input() {
    let data = zsupport::large_input(70_000);
    for level in [1i8, 6, 9] {
        for (framing, config) in [
            ("zlib", DeflateConfig::zlib(level)),
            ("raw", DeflateConfig::raw(level)),
            ("gzip", DeflateConfig::gzip(level)),
        ] {
            let one_shot = Deflate::compress_with(config, &data).expect("compress");
            for schedule in [
                // one input call, tiny output buffers
                Schedule::new(data.len(), 7),
                // tiny input pieces, one big output buffer
                Schedule::new(3, 64 << 10),
                // both small
                Schedule::new(1024, 1024),
            ] {
                let streamed = pump_whole(config, &data, schedule);
                assert_eq!(
                    hex(&streamed),
                    hex(&one_shot),
                    "{framing} level {level} on {} bytes: {schedule:?} disagrees with the \
                     one-shot stream",
                    data.len()
                );
            }
        }
    }
    println!("a 70 000-byte input streams to the one-shot bytes at levels 1/6/9");
}

/// **Level 0 is exactly as schedule-sensitive as the C is** — the stored blocks are sized
/// from `avail_in`/`avail_out` (`deflate.c:1668-1780`), so a tiny pump produces a different
/// (equally valid) arrangement of stored blocks for a large input.
///
/// This is a property of `deflate_stored`, not a defect: the one-shot stream is the one the
/// oracle produces (its 8 KiB schedule), and every schedule still inflates to the input.
/// Note that the *length* is not a usable signal here — two different arrangements can have
/// the same number of blocks — so the assertion is on the bytes.
#[test]
fn level_0_block_split_follows_the_pump_schedule_like_the_c() {
    let data = zsupport::large_input(70_000);
    let config = DeflateConfig::zlib(0);
    let one_shot = Deflate::compress_with(config, &data).expect("compress");
    let tiny_input = pump_whole(config, &data, Schedule::new(3, 64 << 10));
    let tiny_output = pump_whole(config, &data, Schedule::new(data.len(), 1));
    assert_ne!(
        hex(&tiny_input),
        hex(&one_shot),
        "3-byte input pieces at level 0 must behave like deflate_stored (block sizes from \
         avail_in), not like a schedule-invariant compressor"
    );
    assert_ne!(
        hex(&tiny_output),
        hex(&one_shot),
        "1-byte output buffers at level 0 must behave like deflate_stored (block sizes from \
         avail_out)"
    );
    for (what, stream) in [
        ("one-shot", &one_shot),
        ("3-byte input pieces", &tiny_input),
        ("1-byte output buffers", &tiny_output),
    ] {
        assert_round_trip(stream, &data, &format!("level 0, {what}"));
    }
    println!(
        "level 0: one-shot {} bytes, 3-byte input pieces {} bytes, 1-byte output buffers {} \
         bytes (all valid)",
        one_shot.len(),
        tiny_input.len(),
        tiny_output.len()
    );
}

/// `Z_SYNC_FLUSH` and `Z_FULL_FLUSH` mid-stream, against the oracle's own image.
///
/// Both end the current block and append the empty stored block that is the alignment
/// marker — its last four bytes are `00 00 FF FF` (`LEN = 0`, `NLEN = 0xFFFF`) — so the
/// split point is visible in the bytes. `Full` additionally forgets the compression history
/// (`clear_hash`), which is why its tail after the marker differs from `Sync`'s while the
/// marker itself is identical.
#[test]
fn a_sync_or_full_flush_mid_stream_matches_the_oracle() {
    let (first, second) = split_parts();
    let data = zsupport::mixed_kib();
    let config = DeflateConfig::zlib(6);
    let mut streams = Vec::new();
    for (flush, want) in [
        (Flush::Sync, SYNC_SPLIT_GOLDEN),
        (Flush::Full, FULL_SPLIT_GOLDEN),
    ] {
        let (stream, split) = pump_split(config, &first, &second, flush, Schedule::NIF);
        assert_eq!(
            hex(&stream),
            want,
            "{flush:?} flush of the first {SPLIT} bytes disagrees with the oracle image"
        );
        assert_eq!(
            &stream[split - 4..split],
            &[0x00, 0x00, 0xff, 0xff],
            "{flush:?} flush must end on the `00 00 FF FF` alignment marker"
        );
        // The stream's own end is the Adler-32 trailer of the whole input, so the marker
        // seen at the split is not the last thing in the stream.
        assert_eq!(
            &stream[stream.len() - 4..],
            &beam_deflate::adler32(1, &data).to_be_bytes(),
            "{flush:?} split stream must end with the input's Adler-32"
        );
        assert_round_trip(
            &stream,
            &data,
            &format!("{flush:?} split flush"),
        );
        // ... and the oracle itself agrees that the concatenation is one valid stream.
        match zsupport::oracle_inflate_wbits(&stream, 15) {
            Some(back) => assert_eq!(back, data, "{flush:?} split stream as inflated by the oracle"),
            None => println!("SKIP {flush:?} split flush oracle inflate: erl not found"),
        }
        streams.push((stream, split));
    }
    // `Sync` and `Full` close the same block identically — the marker and everything before
    // it are byte-identical — and then diverge, because `Full` also clears the hash so the
    // second half cannot match anything from the first (`deflate.c`'s `clear_hash` in the
    // `Z_FULL_FLUSH` arm).
    let (sync, sync_split) = &streams[0];
    let (full, full_split) = &streams[1];
    assert_eq!(
        sync_split, full_split,
        "Sync and Full flush the same prefix to the same length"
    );
    assert_eq!(
        hex(&sync[..*sync_split]),
        hex(&full[..*full_split]),
        "Sync and Full must agree up to and including the alignment marker"
    );
    assert_ne!(
        hex(&sync[*sync_split..]),
        hex(&full[*full_split..]),
        "Full's cleared history must change the rest of the stream"
    );
    println!("Sync and Full split-flush images match the oracle");
}

/// `Z_PARTIAL_FLUSH` and `Z_BLOCK` have no `zlib` binding, so they are pinned structurally
/// against the `Sync` stream of the same input (see the module docs for the `deflate.c`
/// flush ladder).
///
/// * `Z_BLOCK` adds nothing to the block: its output is a **strict byte prefix** of the
///   `Sync` output, and it does not carry the marker.
/// * `Z_PARTIAL_FLUSH` adds the aligned empty *static* block (`_tr_align`), so it is longer
///   than the `Z_BLOCK` output by at most the two bytes that block can occupy once aligned,
///   and it too has no `00 00 FF FF`.
///   (It is *not* a prefix of the `Sync` output: the static block's bits and the stored
///   block's header diverge at the split.)
/// * Both stay inflatable once the rest of the stream is appended, which is what "ends on a
///   block boundary" means operationally.
#[test]
fn a_block_flush_stops_at_a_block_boundary_and_a_partial_flush_aligns() {
    let (first, second) = split_parts();
    let data = zsupport::mixed_kib();
    let config = DeflateConfig::zlib(6);
    let (block, block_split) = pump_split(config, &first, &second, Flush::Block, Schedule::NIF);
    let (partial, partial_split) =
        pump_split(config, &first, &second, Flush::Partial, Schedule::NIF);
    let (sync, sync_split) = pump_split(config, &first, &second, Flush::Sync, Schedule::NIF);

    // `Z_BLOCK`: a strict prefix of `Sync`, no marker, and not even byte-aligned to it.
    assert_eq!(
        &sync[..block_split],
        &block[..block_split],
        "the Z_BLOCK output must be a byte prefix of the Z_SYNC output"
    );
    assert!(
        block_split < sync_split,
        "Z_BLOCK ({block_split}) must emit fewer bytes than Z_SYNC ({sync_split})"
    );
    assert_ne!(
        &block[block_split - 4..block_split],
        &[0x00, 0x00, 0xff, 0xff],
        "Z_BLOCK must not append the Z_SYNC alignment marker"
    );

    // `Z_PARTIAL_FLUSH`: the block bytes plus the aligned empty static block.
    assert_eq!(
        &partial[..block_split],
        &block[..block_split],
        "the Z_PARTIAL output must extend the Z_BLOCK output"
    );
    let added = partial_split - block_split;
    assert!(
        (1..=2).contains(&added),
        "an empty static block plus alignment adds 1..=2 bytes, not {added}"
    );
    assert_ne!(
        &partial[partial_split - 4..partial_split],
        &[0x00, 0x00, 0xff, 0xff],
        "Z_PARTIAL must not append the Z_SYNC alignment marker"
    );

    for (what, stream, want) in [
        ("Z_BLOCK", &block, &data),
        ("Z_PARTIAL", &partial, &data),
        ("Z_SYNC", &sync, &data),
    ] {
        assert_round_trip(stream, want, what);
        // The real zlib agrees that each of these split points is a legal block boundary.
        match zsupport::oracle_inflate_wbits(stream, 15) {
            Some(back) => assert_eq!(back, *want, "{what}: the oracle's inflater"),
            None => println!("SKIP {what} oracle inflate: erl not found"),
        }
    }

    // The second half of a Partial-flushed stream must start on a byte boundary: zlib
    // aligned the bit stream, so the following block header begins in a fresh byte.
    // (Checked through the inflater above: a misaligned continuation cannot inflate.)
    println!(
        "Z_BLOCK {block_split} bytes (prefix of Z_SYNC's {sync_split}), Z_PARTIAL \
         {partial_split} (+{added}), Z_SYNC {sync_split}"
    );
}

/// `deflateParams` mid-stream, against the oracle.
///
/// The recipe is the one `zlib:deflateParams/3` uses: flush to a byte boundary with an empty
/// `sync` call first (the NIF refuses anything else — `zlib_nif.c:zlib_deflateParams` hands
/// `deflateParams` a one-byte dummy buffer and reports `Z_STREAM_ERROR` if it comes back
/// full), then change the level/strategy, then finish. Both changes cross a strategy
/// *function* boundary (6 → 1 is `deflate_slow` → `deflate_fast`, 1 → 9 the reverse), which
/// is the case that makes the C close the current block first.
///
/// The returned [`Status`] is `deflateParams`'s own return code, so it must be `Ok` here;
/// the inner `deflate(Z_BLOCK)`'s `Z_BUF_ERROR` is deliberately not reported (see
/// [`Deflate::set_params`]).
#[test]
fn set_params_mid_stream_matches_the_oracle() {
    let (first, second) = split_parts();
    for (init_level, new_level, new_strategy, want) in [
        (6i8, 1i8, Strategy::Default, PARAMS_6_TO_1_GOLDEN),
        (1, 9, Strategy::Default, PARAMS_1_TO_9_GOLDEN),
    ] {
        let mut stream = Deflate::new(DeflateConfig::zlib(init_level)).expect("valid config");
        let mut out = Vec::new();
        pump(&mut stream, &first, &mut out, Schedule::NIF, Flush::Sync);
        let mut buf = vec![0u8; 8 << 10];
        let step = stream
            .set_params(new_level, new_strategy, &mut buf)
            .expect("deflateParams");
        assert_eq!(
            step.status,
            Status::Ok,
            "deflateParams {init_level} -> {new_level} reports the C's Z_OK"
        );
        assert_eq!(
            (step.consumed, step.produced),
            (0, 0),
            "after a sync flush the stream is on a byte boundary with nothing pending, so \
             deflateParams has nothing to flush"
        );
        out.extend_from_slice(&buf[..step.produced]);
        assert_eq!(
            pump(&mut stream, &second, &mut out, Schedule::NIF, Flush::Finish),
            Status::StreamEnd
        );
        assert_eq!(
            hex(&out),
            want,
            "deflateParams {init_level} -> {new_level} disagrees with the oracle image"
        );
        assert_round_trip(
            &out,
            &zsupport::mixed_kib(),
            &format!("deflateParams {init_level} -> {new_level}"),
        );
        // The level really changed: the same input streamed at the two levels alone must
        // differ, otherwise the golden above would not prove anything.
        let at_init = Deflate::compress_with(DeflateConfig::zlib(init_level), &zsupport::mixed_kib())
            .expect("compress");
        let at_new = Deflate::compress_with(DeflateConfig::zlib(new_level), &zsupport::mixed_kib())
            .expect("compress");
        assert_ne!(
            hex(&at_init),
            hex(&at_new),
            "levels {init_level} and {new_level} must produce different streams"
        );
    }
    println!("deflateParams 6 -> 1 and 1 -> 9 match the oracle");
}

/// Print the four split-flush/`deflateParams` goldens above, asserting first that this port
/// already produces the oracle's bytes — the regeneration route documented at the top.
///
/// It needs the oracle (`erl`), so it is `#[ignore]`d and never runs in a normal
/// `cargo test`.
#[test]
#[ignore = "regenerates the embedded split-flush goldens; needs erl, paste the output back"]
fn regenerate_split_goldens() {
    let (first, second) = split_parts();
    if !zsupport::have_oracle() {
        println!("SKIP regenerate_split_goldens: erl not found");
        return;
    }
    let data = zsupport::mixed_kib();
    for (name, mid) in [
        ("SYNC_SPLIT_GOLDEN", zsupport::MidFlush::Sync),
        ("FULL_SPLIT_GOLDEN", zsupport::MidFlush::Full),
    ] {
        let oracle = zsupport::oracle_deflate_split(6, 15, &first, &second, mid)
            .expect("the oracle produced a split image");
        let flush = match mid {
            zsupport::MidFlush::Sync => Flush::Sync,
            zsupport::MidFlush::Full => Flush::Full,
        };
        let (ours, _) = pump_split(DeflateConfig::zlib(6), &first, &second, flush, Schedule::NIF);
        assert_eq!(hex(&ours), hex(&oracle), "{name}: this port diverges");
        print_string_const(name, &hex(&oracle));
    }
    for (name, init_level, new_level) in [
        ("PARAMS_6_TO_1_GOLDEN", 6i8, 1i8),
        ("PARAMS_1_TO_9_GOLDEN", 1, 9),
    ] {
        let oracle = zsupport::oracle_deflate_params(init_level, &first, new_level, 0, &second)
            .expect("the oracle produced a deflateParams image");
        let mut stream = Deflate::new(DeflateConfig::zlib(init_level)).expect("valid config");
        let mut out = Vec::new();
        pump(&mut stream, &first, &mut out, Schedule::NIF, Flush::Sync);
        let mut buf = vec![0u8; 8 << 10];
        let step = stream
            .set_params(new_level, Strategy::Default, &mut buf)
            .expect("deflateParams");
        out.extend_from_slice(&buf[..step.produced]);
        pump(&mut stream, &second, &mut out, Schedule::NIF, Flush::Finish);
        assert_eq!(hex(&out), hex(&oracle), "{name}: this port diverges");
        print_string_const(name, &hex(&oracle));
    }
    assert_eq!(data.len(), SPLIT * 2, "the split halves cover the input");
}

/// Print one `const NAME: &str = concat!(...)` between `=== NAME ===` markers, wrapping the
/// hex at 88 characters.
fn print_string_const(name: &str, image_hex: &str) {
    println!("=== {name} ===");
    println!("const {name}: &str = concat!(");
    for chunk in image_hex.as_bytes().chunks(88) {
        let chunk = std::str::from_utf8(chunk).expect("hex is ASCII");
        println!("    \"{chunk}\",");
    }
    println!(");");
}
