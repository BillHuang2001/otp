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
//! `inffixed.h` — the fixed-Huffman decoding tables (`buildtables` output).
//!
//! Byte-for-byte the tables `inftrees.c`'s `makefixed` program emits into
//! `inffixed.h` (zlib 1.3.2), so a fixed block decodes without building a table at
//! run time. They are `inflate_table(LENS, …)` over the RFC 1951 fixed lengths
//! (0–143 → 8 bits, 144–255 → 9, 256–279 → 7, 280–287 → 8; root 9) and
//! `inflate_table(DISTS, …)` over 32 symbols of 5 bits (root 5) — with one
//! deliberate deviation `makefixed` applies: every entry whose index has
//! `index & 127 == 99` gets `op = 64` (the "invalid code" marker) instead of the
//! `199`/`75` the builder produces for the two reserved length symbols 286/287.
//! Both values are invalid-code markers and every consumer tests `op & 16`,
//! `op & 32` and `op & 64`, all of which agree for `199`/`75`/`64` (bits 4 and 5
//! clear, bit 6 set), so the substitution cannot change an observable.
//! `inftrees::tests::the_fixed_tables_match_inffixed_h` proves the builder agrees
//! with this data after that one substitution.

use super::inftrees::Code;

/// `lenfix` — the 512-entry fixed literal/length table (root 9).
pub(crate) const LENFIX: [Code; 512] = [
    Code { op: 96, bits: 7, val: 0 }, Code { op: 0, bits: 8, val: 80 }, Code { op: 0, bits: 8, val: 16 }, Code { op: 20, bits: 8, val: 115 }, Code { op: 18, bits: 7, val: 31 }, Code { op: 0, bits: 8, val: 112 }, Code { op: 0, bits: 8, val: 48 },
    Code { op: 0, bits: 9, val: 192 }, Code { op: 16, bits: 7, val: 10 }, Code { op: 0, bits: 8, val: 96 }, Code { op: 0, bits: 8, val: 32 }, Code { op: 0, bits: 9, val: 160 }, Code { op: 0, bits: 8, val: 0 }, Code { op: 0, bits: 8, val: 128 },
    Code { op: 0, bits: 8, val: 64 }, Code { op: 0, bits: 9, val: 224 }, Code { op: 16, bits: 7, val: 6 }, Code { op: 0, bits: 8, val: 88 }, Code { op: 0, bits: 8, val: 24 }, Code { op: 0, bits: 9, val: 144 }, Code { op: 19, bits: 7, val: 59 },
    Code { op: 0, bits: 8, val: 120 }, Code { op: 0, bits: 8, val: 56 }, Code { op: 0, bits: 9, val: 208 }, Code { op: 17, bits: 7, val: 17 }, Code { op: 0, bits: 8, val: 104 }, Code { op: 0, bits: 8, val: 40 }, Code { op: 0, bits: 9, val: 176 },
    Code { op: 0, bits: 8, val: 8 }, Code { op: 0, bits: 8, val: 136 }, Code { op: 0, bits: 8, val: 72 }, Code { op: 0, bits: 9, val: 240 }, Code { op: 16, bits: 7, val: 4 }, Code { op: 0, bits: 8, val: 84 }, Code { op: 0, bits: 8, val: 20 },
    Code { op: 21, bits: 8, val: 227 }, Code { op: 19, bits: 7, val: 43 }, Code { op: 0, bits: 8, val: 116 }, Code { op: 0, bits: 8, val: 52 }, Code { op: 0, bits: 9, val: 200 }, Code { op: 17, bits: 7, val: 13 }, Code { op: 0, bits: 8, val: 100 },
    Code { op: 0, bits: 8, val: 36 }, Code { op: 0, bits: 9, val: 168 }, Code { op: 0, bits: 8, val: 4 }, Code { op: 0, bits: 8, val: 132 }, Code { op: 0, bits: 8, val: 68 }, Code { op: 0, bits: 9, val: 232 }, Code { op: 16, bits: 7, val: 8 },
    Code { op: 0, bits: 8, val: 92 }, Code { op: 0, bits: 8, val: 28 }, Code { op: 0, bits: 9, val: 152 }, Code { op: 20, bits: 7, val: 83 }, Code { op: 0, bits: 8, val: 124 }, Code { op: 0, bits: 8, val: 60 }, Code { op: 0, bits: 9, val: 216 },
    Code { op: 18, bits: 7, val: 23 }, Code { op: 0, bits: 8, val: 108 }, Code { op: 0, bits: 8, val: 44 }, Code { op: 0, bits: 9, val: 184 }, Code { op: 0, bits: 8, val: 12 }, Code { op: 0, bits: 8, val: 140 }, Code { op: 0, bits: 8, val: 76 },
    Code { op: 0, bits: 9, val: 248 }, Code { op: 16, bits: 7, val: 3 }, Code { op: 0, bits: 8, val: 82 }, Code { op: 0, bits: 8, val: 18 }, Code { op: 21, bits: 8, val: 163 }, Code { op: 19, bits: 7, val: 35 }, Code { op: 0, bits: 8, val: 114 },
    Code { op: 0, bits: 8, val: 50 }, Code { op: 0, bits: 9, val: 196 }, Code { op: 17, bits: 7, val: 11 }, Code { op: 0, bits: 8, val: 98 }, Code { op: 0, bits: 8, val: 34 }, Code { op: 0, bits: 9, val: 164 }, Code { op: 0, bits: 8, val: 2 },
    Code { op: 0, bits: 8, val: 130 }, Code { op: 0, bits: 8, val: 66 }, Code { op: 0, bits: 9, val: 228 }, Code { op: 16, bits: 7, val: 7 }, Code { op: 0, bits: 8, val: 90 }, Code { op: 0, bits: 8, val: 26 }, Code { op: 0, bits: 9, val: 148 },
    Code { op: 20, bits: 7, val: 67 }, Code { op: 0, bits: 8, val: 122 }, Code { op: 0, bits: 8, val: 58 }, Code { op: 0, bits: 9, val: 212 }, Code { op: 18, bits: 7, val: 19 }, Code { op: 0, bits: 8, val: 106 }, Code { op: 0, bits: 8, val: 42 },
    Code { op: 0, bits: 9, val: 180 }, Code { op: 0, bits: 8, val: 10 }, Code { op: 0, bits: 8, val: 138 }, Code { op: 0, bits: 8, val: 74 }, Code { op: 0, bits: 9, val: 244 }, Code { op: 16, bits: 7, val: 5 }, Code { op: 0, bits: 8, val: 86 },
    Code { op: 0, bits: 8, val: 22 }, Code { op: 64, bits: 8, val: 0 }, Code { op: 19, bits: 7, val: 51 }, Code { op: 0, bits: 8, val: 118 }, Code { op: 0, bits: 8, val: 54 }, Code { op: 0, bits: 9, val: 204 }, Code { op: 17, bits: 7, val: 15 },
    Code { op: 0, bits: 8, val: 102 }, Code { op: 0, bits: 8, val: 38 }, Code { op: 0, bits: 9, val: 172 }, Code { op: 0, bits: 8, val: 6 }, Code { op: 0, bits: 8, val: 134 }, Code { op: 0, bits: 8, val: 70 }, Code { op: 0, bits: 9, val: 236 },
    Code { op: 16, bits: 7, val: 9 }, Code { op: 0, bits: 8, val: 94 }, Code { op: 0, bits: 8, val: 30 }, Code { op: 0, bits: 9, val: 156 }, Code { op: 20, bits: 7, val: 99 }, Code { op: 0, bits: 8, val: 126 }, Code { op: 0, bits: 8, val: 62 },
    Code { op: 0, bits: 9, val: 220 }, Code { op: 18, bits: 7, val: 27 }, Code { op: 0, bits: 8, val: 110 }, Code { op: 0, bits: 8, val: 46 }, Code { op: 0, bits: 9, val: 188 }, Code { op: 0, bits: 8, val: 14 }, Code { op: 0, bits: 8, val: 142 },
    Code { op: 0, bits: 8, val: 78 }, Code { op: 0, bits: 9, val: 252 }, Code { op: 96, bits: 7, val: 0 }, Code { op: 0, bits: 8, val: 81 }, Code { op: 0, bits: 8, val: 17 }, Code { op: 21, bits: 8, val: 131 }, Code { op: 18, bits: 7, val: 31 },
    Code { op: 0, bits: 8, val: 113 }, Code { op: 0, bits: 8, val: 49 }, Code { op: 0, bits: 9, val: 194 }, Code { op: 16, bits: 7, val: 10 }, Code { op: 0, bits: 8, val: 97 }, Code { op: 0, bits: 8, val: 33 }, Code { op: 0, bits: 9, val: 162 },
    Code { op: 0, bits: 8, val: 1 }, Code { op: 0, bits: 8, val: 129 }, Code { op: 0, bits: 8, val: 65 }, Code { op: 0, bits: 9, val: 226 }, Code { op: 16, bits: 7, val: 6 }, Code { op: 0, bits: 8, val: 89 }, Code { op: 0, bits: 8, val: 25 },
    Code { op: 0, bits: 9, val: 146 }, Code { op: 19, bits: 7, val: 59 }, Code { op: 0, bits: 8, val: 121 }, Code { op: 0, bits: 8, val: 57 }, Code { op: 0, bits: 9, val: 210 }, Code { op: 17, bits: 7, val: 17 }, Code { op: 0, bits: 8, val: 105 },
    Code { op: 0, bits: 8, val: 41 }, Code { op: 0, bits: 9, val: 178 }, Code { op: 0, bits: 8, val: 9 }, Code { op: 0, bits: 8, val: 137 }, Code { op: 0, bits: 8, val: 73 }, Code { op: 0, bits: 9, val: 242 }, Code { op: 16, bits: 7, val: 4 },
    Code { op: 0, bits: 8, val: 85 }, Code { op: 0, bits: 8, val: 21 }, Code { op: 16, bits: 8, val: 258 }, Code { op: 19, bits: 7, val: 43 }, Code { op: 0, bits: 8, val: 117 }, Code { op: 0, bits: 8, val: 53 }, Code { op: 0, bits: 9, val: 202 },
    Code { op: 17, bits: 7, val: 13 }, Code { op: 0, bits: 8, val: 101 }, Code { op: 0, bits: 8, val: 37 }, Code { op: 0, bits: 9, val: 170 }, Code { op: 0, bits: 8, val: 5 }, Code { op: 0, bits: 8, val: 133 }, Code { op: 0, bits: 8, val: 69 },
    Code { op: 0, bits: 9, val: 234 }, Code { op: 16, bits: 7, val: 8 }, Code { op: 0, bits: 8, val: 93 }, Code { op: 0, bits: 8, val: 29 }, Code { op: 0, bits: 9, val: 154 }, Code { op: 20, bits: 7, val: 83 }, Code { op: 0, bits: 8, val: 125 },
    Code { op: 0, bits: 8, val: 61 }, Code { op: 0, bits: 9, val: 218 }, Code { op: 18, bits: 7, val: 23 }, Code { op: 0, bits: 8, val: 109 }, Code { op: 0, bits: 8, val: 45 }, Code { op: 0, bits: 9, val: 186 }, Code { op: 0, bits: 8, val: 13 },
    Code { op: 0, bits: 8, val: 141 }, Code { op: 0, bits: 8, val: 77 }, Code { op: 0, bits: 9, val: 250 }, Code { op: 16, bits: 7, val: 3 }, Code { op: 0, bits: 8, val: 83 }, Code { op: 0, bits: 8, val: 19 }, Code { op: 21, bits: 8, val: 195 },
    Code { op: 19, bits: 7, val: 35 }, Code { op: 0, bits: 8, val: 115 }, Code { op: 0, bits: 8, val: 51 }, Code { op: 0, bits: 9, val: 198 }, Code { op: 17, bits: 7, val: 11 }, Code { op: 0, bits: 8, val: 99 }, Code { op: 0, bits: 8, val: 35 },
    Code { op: 0, bits: 9, val: 166 }, Code { op: 0, bits: 8, val: 3 }, Code { op: 0, bits: 8, val: 131 }, Code { op: 0, bits: 8, val: 67 }, Code { op: 0, bits: 9, val: 230 }, Code { op: 16, bits: 7, val: 7 }, Code { op: 0, bits: 8, val: 91 },
    Code { op: 0, bits: 8, val: 27 }, Code { op: 0, bits: 9, val: 150 }, Code { op: 20, bits: 7, val: 67 }, Code { op: 0, bits: 8, val: 123 }, Code { op: 0, bits: 8, val: 59 }, Code { op: 0, bits: 9, val: 214 }, Code { op: 18, bits: 7, val: 19 },
    Code { op: 0, bits: 8, val: 107 }, Code { op: 0, bits: 8, val: 43 }, Code { op: 0, bits: 9, val: 182 }, Code { op: 0, bits: 8, val: 11 }, Code { op: 0, bits: 8, val: 139 }, Code { op: 0, bits: 8, val: 75 }, Code { op: 0, bits: 9, val: 246 },
    Code { op: 16, bits: 7, val: 5 }, Code { op: 0, bits: 8, val: 87 }, Code { op: 0, bits: 8, val: 23 }, Code { op: 64, bits: 8, val: 0 }, Code { op: 19, bits: 7, val: 51 }, Code { op: 0, bits: 8, val: 119 }, Code { op: 0, bits: 8, val: 55 },
    Code { op: 0, bits: 9, val: 206 }, Code { op: 17, bits: 7, val: 15 }, Code { op: 0, bits: 8, val: 103 }, Code { op: 0, bits: 8, val: 39 }, Code { op: 0, bits: 9, val: 174 }, Code { op: 0, bits: 8, val: 7 }, Code { op: 0, bits: 8, val: 135 },
    Code { op: 0, bits: 8, val: 71 }, Code { op: 0, bits: 9, val: 238 }, Code { op: 16, bits: 7, val: 9 }, Code { op: 0, bits: 8, val: 95 }, Code { op: 0, bits: 8, val: 31 }, Code { op: 0, bits: 9, val: 158 }, Code { op: 20, bits: 7, val: 99 },
    Code { op: 0, bits: 8, val: 127 }, Code { op: 0, bits: 8, val: 63 }, Code { op: 0, bits: 9, val: 222 }, Code { op: 18, bits: 7, val: 27 }, Code { op: 0, bits: 8, val: 111 }, Code { op: 0, bits: 8, val: 47 }, Code { op: 0, bits: 9, val: 190 },
    Code { op: 0, bits: 8, val: 15 }, Code { op: 0, bits: 8, val: 143 }, Code { op: 0, bits: 8, val: 79 }, Code { op: 0, bits: 9, val: 254 }, Code { op: 96, bits: 7, val: 0 }, Code { op: 0, bits: 8, val: 80 }, Code { op: 0, bits: 8, val: 16 },
    Code { op: 20, bits: 8, val: 115 }, Code { op: 18, bits: 7, val: 31 }, Code { op: 0, bits: 8, val: 112 }, Code { op: 0, bits: 8, val: 48 }, Code { op: 0, bits: 9, val: 193 }, Code { op: 16, bits: 7, val: 10 }, Code { op: 0, bits: 8, val: 96 },
    Code { op: 0, bits: 8, val: 32 }, Code { op: 0, bits: 9, val: 161 }, Code { op: 0, bits: 8, val: 0 }, Code { op: 0, bits: 8, val: 128 }, Code { op: 0, bits: 8, val: 64 }, Code { op: 0, bits: 9, val: 225 }, Code { op: 16, bits: 7, val: 6 },
    Code { op: 0, bits: 8, val: 88 }, Code { op: 0, bits: 8, val: 24 }, Code { op: 0, bits: 9, val: 145 }, Code { op: 19, bits: 7, val: 59 }, Code { op: 0, bits: 8, val: 120 }, Code { op: 0, bits: 8, val: 56 }, Code { op: 0, bits: 9, val: 209 },
    Code { op: 17, bits: 7, val: 17 }, Code { op: 0, bits: 8, val: 104 }, Code { op: 0, bits: 8, val: 40 }, Code { op: 0, bits: 9, val: 177 }, Code { op: 0, bits: 8, val: 8 }, Code { op: 0, bits: 8, val: 136 }, Code { op: 0, bits: 8, val: 72 },
    Code { op: 0, bits: 9, val: 241 }, Code { op: 16, bits: 7, val: 4 }, Code { op: 0, bits: 8, val: 84 }, Code { op: 0, bits: 8, val: 20 }, Code { op: 21, bits: 8, val: 227 }, Code { op: 19, bits: 7, val: 43 }, Code { op: 0, bits: 8, val: 116 },
    Code { op: 0, bits: 8, val: 52 }, Code { op: 0, bits: 9, val: 201 }, Code { op: 17, bits: 7, val: 13 }, Code { op: 0, bits: 8, val: 100 }, Code { op: 0, bits: 8, val: 36 }, Code { op: 0, bits: 9, val: 169 }, Code { op: 0, bits: 8, val: 4 },
    Code { op: 0, bits: 8, val: 132 }, Code { op: 0, bits: 8, val: 68 }, Code { op: 0, bits: 9, val: 233 }, Code { op: 16, bits: 7, val: 8 }, Code { op: 0, bits: 8, val: 92 }, Code { op: 0, bits: 8, val: 28 }, Code { op: 0, bits: 9, val: 153 },
    Code { op: 20, bits: 7, val: 83 }, Code { op: 0, bits: 8, val: 124 }, Code { op: 0, bits: 8, val: 60 }, Code { op: 0, bits: 9, val: 217 }, Code { op: 18, bits: 7, val: 23 }, Code { op: 0, bits: 8, val: 108 }, Code { op: 0, bits: 8, val: 44 },
    Code { op: 0, bits: 9, val: 185 }, Code { op: 0, bits: 8, val: 12 }, Code { op: 0, bits: 8, val: 140 }, Code { op: 0, bits: 8, val: 76 }, Code { op: 0, bits: 9, val: 249 }, Code { op: 16, bits: 7, val: 3 }, Code { op: 0, bits: 8, val: 82 },
    Code { op: 0, bits: 8, val: 18 }, Code { op: 21, bits: 8, val: 163 }, Code { op: 19, bits: 7, val: 35 }, Code { op: 0, bits: 8, val: 114 }, Code { op: 0, bits: 8, val: 50 }, Code { op: 0, bits: 9, val: 197 }, Code { op: 17, bits: 7, val: 11 },
    Code { op: 0, bits: 8, val: 98 }, Code { op: 0, bits: 8, val: 34 }, Code { op: 0, bits: 9, val: 165 }, Code { op: 0, bits: 8, val: 2 }, Code { op: 0, bits: 8, val: 130 }, Code { op: 0, bits: 8, val: 66 }, Code { op: 0, bits: 9, val: 229 },
    Code { op: 16, bits: 7, val: 7 }, Code { op: 0, bits: 8, val: 90 }, Code { op: 0, bits: 8, val: 26 }, Code { op: 0, bits: 9, val: 149 }, Code { op: 20, bits: 7, val: 67 }, Code { op: 0, bits: 8, val: 122 }, Code { op: 0, bits: 8, val: 58 },
    Code { op: 0, bits: 9, val: 213 }, Code { op: 18, bits: 7, val: 19 }, Code { op: 0, bits: 8, val: 106 }, Code { op: 0, bits: 8, val: 42 }, Code { op: 0, bits: 9, val: 181 }, Code { op: 0, bits: 8, val: 10 }, Code { op: 0, bits: 8, val: 138 },
    Code { op: 0, bits: 8, val: 74 }, Code { op: 0, bits: 9, val: 245 }, Code { op: 16, bits: 7, val: 5 }, Code { op: 0, bits: 8, val: 86 }, Code { op: 0, bits: 8, val: 22 }, Code { op: 64, bits: 8, val: 0 }, Code { op: 19, bits: 7, val: 51 },
    Code { op: 0, bits: 8, val: 118 }, Code { op: 0, bits: 8, val: 54 }, Code { op: 0, bits: 9, val: 205 }, Code { op: 17, bits: 7, val: 15 }, Code { op: 0, bits: 8, val: 102 }, Code { op: 0, bits: 8, val: 38 }, Code { op: 0, bits: 9, val: 173 },
    Code { op: 0, bits: 8, val: 6 }, Code { op: 0, bits: 8, val: 134 }, Code { op: 0, bits: 8, val: 70 }, Code { op: 0, bits: 9, val: 237 }, Code { op: 16, bits: 7, val: 9 }, Code { op: 0, bits: 8, val: 94 }, Code { op: 0, bits: 8, val: 30 },
    Code { op: 0, bits: 9, val: 157 }, Code { op: 20, bits: 7, val: 99 }, Code { op: 0, bits: 8, val: 126 }, Code { op: 0, bits: 8, val: 62 }, Code { op: 0, bits: 9, val: 221 }, Code { op: 18, bits: 7, val: 27 }, Code { op: 0, bits: 8, val: 110 },
    Code { op: 0, bits: 8, val: 46 }, Code { op: 0, bits: 9, val: 189 }, Code { op: 0, bits: 8, val: 14 }, Code { op: 0, bits: 8, val: 142 }, Code { op: 0, bits: 8, val: 78 }, Code { op: 0, bits: 9, val: 253 }, Code { op: 96, bits: 7, val: 0 },
    Code { op: 0, bits: 8, val: 81 }, Code { op: 0, bits: 8, val: 17 }, Code { op: 21, bits: 8, val: 131 }, Code { op: 18, bits: 7, val: 31 }, Code { op: 0, bits: 8, val: 113 }, Code { op: 0, bits: 8, val: 49 }, Code { op: 0, bits: 9, val: 195 },
    Code { op: 16, bits: 7, val: 10 }, Code { op: 0, bits: 8, val: 97 }, Code { op: 0, bits: 8, val: 33 }, Code { op: 0, bits: 9, val: 163 }, Code { op: 0, bits: 8, val: 1 }, Code { op: 0, bits: 8, val: 129 }, Code { op: 0, bits: 8, val: 65 },
    Code { op: 0, bits: 9, val: 227 }, Code { op: 16, bits: 7, val: 6 }, Code { op: 0, bits: 8, val: 89 }, Code { op: 0, bits: 8, val: 25 }, Code { op: 0, bits: 9, val: 147 }, Code { op: 19, bits: 7, val: 59 }, Code { op: 0, bits: 8, val: 121 },
    Code { op: 0, bits: 8, val: 57 }, Code { op: 0, bits: 9, val: 211 }, Code { op: 17, bits: 7, val: 17 }, Code { op: 0, bits: 8, val: 105 }, Code { op: 0, bits: 8, val: 41 }, Code { op: 0, bits: 9, val: 179 }, Code { op: 0, bits: 8, val: 9 },
    Code { op: 0, bits: 8, val: 137 }, Code { op: 0, bits: 8, val: 73 }, Code { op: 0, bits: 9, val: 243 }, Code { op: 16, bits: 7, val: 4 }, Code { op: 0, bits: 8, val: 85 }, Code { op: 0, bits: 8, val: 21 }, Code { op: 16, bits: 8, val: 258 },
    Code { op: 19, bits: 7, val: 43 }, Code { op: 0, bits: 8, val: 117 }, Code { op: 0, bits: 8, val: 53 }, Code { op: 0, bits: 9, val: 203 }, Code { op: 17, bits: 7, val: 13 }, Code { op: 0, bits: 8, val: 101 }, Code { op: 0, bits: 8, val: 37 },
    Code { op: 0, bits: 9, val: 171 }, Code { op: 0, bits: 8, val: 5 }, Code { op: 0, bits: 8, val: 133 }, Code { op: 0, bits: 8, val: 69 }, Code { op: 0, bits: 9, val: 235 }, Code { op: 16, bits: 7, val: 8 }, Code { op: 0, bits: 8, val: 93 },
    Code { op: 0, bits: 8, val: 29 }, Code { op: 0, bits: 9, val: 155 }, Code { op: 20, bits: 7, val: 83 }, Code { op: 0, bits: 8, val: 125 }, Code { op: 0, bits: 8, val: 61 }, Code { op: 0, bits: 9, val: 219 }, Code { op: 18, bits: 7, val: 23 },
    Code { op: 0, bits: 8, val: 109 }, Code { op: 0, bits: 8, val: 45 }, Code { op: 0, bits: 9, val: 187 }, Code { op: 0, bits: 8, val: 13 }, Code { op: 0, bits: 8, val: 141 }, Code { op: 0, bits: 8, val: 77 }, Code { op: 0, bits: 9, val: 251 },
    Code { op: 16, bits: 7, val: 3 }, Code { op: 0, bits: 8, val: 83 }, Code { op: 0, bits: 8, val: 19 }, Code { op: 21, bits: 8, val: 195 }, Code { op: 19, bits: 7, val: 35 }, Code { op: 0, bits: 8, val: 115 }, Code { op: 0, bits: 8, val: 51 },
    Code { op: 0, bits: 9, val: 199 }, Code { op: 17, bits: 7, val: 11 }, Code { op: 0, bits: 8, val: 99 }, Code { op: 0, bits: 8, val: 35 }, Code { op: 0, bits: 9, val: 167 }, Code { op: 0, bits: 8, val: 3 }, Code { op: 0, bits: 8, val: 131 },
    Code { op: 0, bits: 8, val: 67 }, Code { op: 0, bits: 9, val: 231 }, Code { op: 16, bits: 7, val: 7 }, Code { op: 0, bits: 8, val: 91 }, Code { op: 0, bits: 8, val: 27 }, Code { op: 0, bits: 9, val: 151 }, Code { op: 20, bits: 7, val: 67 },
    Code { op: 0, bits: 8, val: 123 }, Code { op: 0, bits: 8, val: 59 }, Code { op: 0, bits: 9, val: 215 }, Code { op: 18, bits: 7, val: 19 }, Code { op: 0, bits: 8, val: 107 }, Code { op: 0, bits: 8, val: 43 }, Code { op: 0, bits: 9, val: 183 },
    Code { op: 0, bits: 8, val: 11 }, Code { op: 0, bits: 8, val: 139 }, Code { op: 0, bits: 8, val: 75 }, Code { op: 0, bits: 9, val: 247 }, Code { op: 16, bits: 7, val: 5 }, Code { op: 0, bits: 8, val: 87 }, Code { op: 0, bits: 8, val: 23 },
    Code { op: 64, bits: 8, val: 0 }, Code { op: 19, bits: 7, val: 51 }, Code { op: 0, bits: 8, val: 119 }, Code { op: 0, bits: 8, val: 55 }, Code { op: 0, bits: 9, val: 207 }, Code { op: 17, bits: 7, val: 15 }, Code { op: 0, bits: 8, val: 103 },
    Code { op: 0, bits: 8, val: 39 }, Code { op: 0, bits: 9, val: 175 }, Code { op: 0, bits: 8, val: 7 }, Code { op: 0, bits: 8, val: 135 }, Code { op: 0, bits: 8, val: 71 }, Code { op: 0, bits: 9, val: 239 }, Code { op: 16, bits: 7, val: 9 },
    Code { op: 0, bits: 8, val: 95 }, Code { op: 0, bits: 8, val: 31 }, Code { op: 0, bits: 9, val: 159 }, Code { op: 20, bits: 7, val: 99 }, Code { op: 0, bits: 8, val: 127 }, Code { op: 0, bits: 8, val: 63 }, Code { op: 0, bits: 9, val: 223 },
    Code { op: 18, bits: 7, val: 27 }, Code { op: 0, bits: 8, val: 111 }, Code { op: 0, bits: 8, val: 47 }, Code { op: 0, bits: 9, val: 191 }, Code { op: 0, bits: 8, val: 15 }, Code { op: 0, bits: 8, val: 143 }, Code { op: 0, bits: 8, val: 79 },
    Code { op: 0, bits: 9, val: 255 },
];

/// `distfix` — the 32-entry fixed distance table (root 5).
pub(crate) const DISTFIX: [Code; 32] = [
    Code { op: 16, bits: 5, val: 1 }, Code { op: 23, bits: 5, val: 257 }, Code { op: 19, bits: 5, val: 17 }, Code { op: 27, bits: 5, val: 4097 }, Code { op: 17, bits: 5, val: 5 }, Code { op: 25, bits: 5, val: 1025 },
    Code { op: 21, bits: 5, val: 65 }, Code { op: 29, bits: 5, val: 16385 }, Code { op: 16, bits: 5, val: 3 }, Code { op: 24, bits: 5, val: 513 }, Code { op: 20, bits: 5, val: 33 }, Code { op: 28, bits: 5, val: 8193 },
    Code { op: 18, bits: 5, val: 9 }, Code { op: 26, bits: 5, val: 2049 }, Code { op: 22, bits: 5, val: 129 }, Code { op: 64, bits: 5, val: 0 }, Code { op: 16, bits: 5, val: 2 }, Code { op: 23, bits: 5, val: 385 },
    Code { op: 19, bits: 5, val: 25 }, Code { op: 27, bits: 5, val: 6145 }, Code { op: 17, bits: 5, val: 7 }, Code { op: 25, bits: 5, val: 1537 }, Code { op: 21, bits: 5, val: 97 }, Code { op: 29, bits: 5, val: 24577 },
    Code { op: 16, bits: 5, val: 4 }, Code { op: 24, bits: 5, val: 769 }, Code { op: 20, bits: 5, val: 49 }, Code { op: 28, bits: 5, val: 12289 }, Code { op: 18, bits: 5, val: 13 }, Code { op: 26, bits: 5, val: 3073 },
    Code { op: 22, bits: 5, val: 193 }, Code { op: 64, bits: 5, val: 0 },
];
