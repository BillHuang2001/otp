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

//! The `inet` drivers' statistics counters — the C's `inet_input_count`/`inet_output_count`.
//!
//! Both socket drivers keep one of these on their descriptor, exactly as the C's
//! `inet_descriptor` does (`inet_drv.c:11560`-`:11623`), and `INET_REQ_GETSTAT` renders it
//! (`inet_fill_stat`, `:10732`-`:10792`). The arithmetic is shared here rather than duplicated
//! per driver, because the two drivers differ only **where** they call it:
//!
//! * the C's `recv_avg`/`send_avg`/`recv_dvi` are **running doubles** (a Welford-style online
//!   update), while `recv_max`/`send_max` are integer peak sizes and `recv_oct`/`send_oct` are
//!   plain 64-bit octet counters on a 64-bit build (`ARCH_64`);
//! * the 32-bit rendering `inet_fill_stat` emits for every counter but the two octet counters
//!   truncates the double (`(unsigned long) desc->recv_avg`), so `recv_avg`/`send_avg`/`recv_dvi`
//!   are only ever observed through that truncation;
//! * the C's `send_avg` update uses the **old** average in the numerator (`desc->send_avg +=
//!   (len - avg) / n` with `avg` captured first) and the `recv_dvi` update uses the **new** one
//!   (`desc->recv_dvi = dvi + ((len - avg) - dvi) / n` with `avg` recomputed), which is why the
//!   two update functions below do not share a body;
//! * `n = cnt + 1` is an `u64` that can wrap to `0`, which the C handles by restarting the
//!   sequence at `n = 1` (the "WRAP, use old avg as input to a new sequence" comment at
//!   `:11576`/`:11603`) — the new value then becomes the next average outright.
//!
//! Legacy C ground truth (read-only): `erts/emulator/drivers/common/inet_drv.c`.

/// The C's `inet_descriptor` statistics block (`inet_drv.c:11560`-`:11623`).
///
/// [`Default`] is the state `inet_init_desc` builds (every counter zero), which is why a
/// socket-less port answers a fully zeroed `INET_REQ_GETSTAT` reply.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(super) struct InetStat {
    /// `desc->recv_cnt`: how many times input was counted.
    pub recv_cnt: u64,
    /// `desc->recv_max`: the largest counted input length.
    pub recv_max: usize,
    /// `desc->recv_avg`: the running mean input length.
    pub recv_avg: f64,
    /// `desc->recv_dvi`: the running mean deviation from [`Self::recv_avg`].
    pub recv_dvi: f64,
    /// `desc->send_cnt`: how many times output was counted.
    pub send_cnt: u64,
    /// `desc->send_max`: the largest counted output length.
    pub send_max: usize,
    /// `desc->send_avg`: the running mean output length.
    pub send_avg: f64,
    /// `desc->recv_oct`: total input octets.
    pub recv_oct: u64,
    /// `desc->send_oct`: total output octets.
    pub send_oct: u64,
}

impl InetStat {
    /// Count one delivered input payload of `len` bytes — the C's `inet_input_count`
    /// (`inet_drv.c:11585`-`:11617`), on the `ARCH_64` path where `recv_oct` is a plain `u64`.
    ///
    /// The deviation is taken from the **updated** average, exactly as the C computes it:
    /// `avg` is reassigned before `dvi` is updated (`:11607`/`:11615`).
    pub(super) fn input(&mut self, len: usize) {
        let mut n = self.recv_cnt.wrapping_add(1);
        let octets = len as u64;
        self.recv_oct = self.recv_oct.wrapping_add(octets);
        if n == 0 {
            // WRAP: restart the sequence at one, so `avg` becomes this length outright.
            n = 1;
        }
        // Average payload length.
        let avg = self.recv_avg + (len as f64 - self.recv_avg) / n as f64;
        self.recv_avg = avg;
        if len > self.recv_max {
            self.recv_max = len;
        }
        // Average deviation from the average payload length.
        let dvi = self.recv_dvi;
        self.recv_dvi = dvi + ((len as f64 - avg) - dvi) / n as f64;
        self.recv_cnt = n;
    }

    /// Count one issued output command of `len` bytes — the C's `inet_output_count`
    /// (`inet_drv.c:11560`-`:11582`), on the `ARCH_64` path.
    ///
    /// The C captures `avg` **before** updating it and uses that old value in the numerator
    /// (`:11567`/`:11578`), so the mean advances by `(len - old_avg) / n`.
    pub(super) fn output(&mut self, len: usize) {
        let mut n = self.send_cnt.wrapping_add(1);
        let octets = len as u64;
        self.send_oct = self.send_oct.wrapping_add(octets);
        if n == 0 {
            // WRAP, use old avg as input to a new sequence (`:11576`).
            n = 1;
        }
        let avg = self.send_avg;
        self.send_avg += (len as f64 - avg) / n as f64;
        if len > self.send_max {
            self.send_max = len;
        }
        self.send_cnt = n;
    }
}

#[cfg(test)]
mod tests {
    use super::InetStat;

    /// Consecutive inputs of 10, 20 and 30 bytes: the C's online average/deviation sequence.
    ///
    /// By hand, from `inet_input_count` (`inet_drv.c:11585`-`:11617`):
    /// * `len = 10`, `n = 1` ⇒ `avg = 0 + (10 - 0)/1 = 10`;
    ///   `dvi = 0 + ((10 - 10) - 0)/1 = 0`.
    /// * `len = 20`, `n = 2` ⇒ `avg = 10 + (20 - 10)/2 = 15`;
    ///   `dvi = 0 + ((20 - 15) - 0)/2 = 2.5`.
    /// * `len = 30`, `n = 3` ⇒ `avg = 15 + (30 - 15)/3 = 20`;
    ///   `dvi = 2.5 + ((30 - 20) - 2.5)/3 = 2.5 + 2.5 = 5`.
    ///
    /// `recv_oct` accumulates every octet and `recv_max` tracks the peak.
    #[test]
    fn consecutive_inputs_advance_the_running_average_and_deviation() {
        let mut stat = InetStat::default();
        stat.input(10);
        assert_eq!(stat.recv_cnt, 1);
        assert_eq!(stat.recv_max, 10);
        assert_eq!(stat.recv_oct, 10);
        assert_eq!(stat.recv_avg, 10.0);
        assert_eq!(stat.recv_dvi, 0.0);

        stat.input(20);
        assert_eq!(stat.recv_cnt, 2);
        assert_eq!(stat.recv_max, 20);
        assert_eq!(stat.recv_oct, 30);
        assert_eq!(stat.recv_avg, 15.0);
        assert_eq!(stat.recv_dvi, 2.5);

        stat.input(30);
        assert_eq!(stat.recv_cnt, 3);
        assert_eq!(stat.recv_max, 30);
        assert_eq!(stat.recv_oct, 60);
        assert_eq!(stat.recv_avg, 20.0);
        assert_eq!(stat.recv_dvi, 5.0);
    }

    /// `recv_max` only moves for a payload **larger** than the current peak — a smaller one
    /// still updates the counters, exactly as the C's `if (len > desc->recv_max)` guard does.
    #[test]
    fn recv_max_only_moves_upwards() {
        let mut stat = InetStat::default();
        stat.input(30);
        assert_eq!(stat.recv_max, 30);

        stat.input(10);
        assert_eq!(stat.recv_max, 30, "a smaller payload leaves the peak alone");
        assert_eq!(stat.recv_cnt, 2);
        assert_eq!(stat.recv_oct, 40);

        stat.input(31);
        assert_eq!(stat.recv_max, 31, "a larger payload replaces the peak");
    }

    /// The output path uses the **old** average in the numerator (`inet_drv.c:11567`/`:11578`).
    ///
    /// By hand: `len = 10`, `n = 1` ⇒ `avg = 0 + (10 - 0)/1 = 10`. Then `len = 20`, `n = 2`
    /// with the old `avg = 10` ⇒ `avg = 10 + (20 - 10)/2 = 15` — the same value the *input*
    /// path reaches, but here the deviation is maintained by computing it against the old
    /// average captured first. A third command of `30` then moves the mean to `15 + (30 -
    /// 15)/3 = 20`.
    #[test]
    fn output_uses_the_previous_average_in_the_numerator() {
        let mut stat = InetStat::default();
        stat.output(10);
        assert_eq!(stat.send_cnt, 1);
        assert_eq!(stat.send_max, 10);
        assert_eq!(stat.send_oct, 10);
        assert_eq!(stat.send_avg, 10.0);

        stat.output(20);
        assert_eq!(stat.send_cnt, 2);
        assert_eq!(stat.send_max, 20);
        assert_eq!(stat.send_oct, 30);
        assert_eq!(stat.send_avg, 15.0);

        stat.output(30);
        assert_eq!(stat.send_cnt, 3);
        assert_eq!(stat.send_oct, 60);
        assert_eq!(stat.send_avg, 20.0);
    }

    /// The `n = cnt + 1` counter is an `u64` that can wrap to `0`; the C restarts the sequence
    /// at `n = 1`, so the next average becomes the new length outright and the deviation returns
    /// to zero (`inet_drv.c:11603`-`:11615`).
    #[test]
    fn a_wrapping_counter_restarts_the_sequence() {
        let mut stat = InetStat {
            recv_cnt: u64::MAX,
            recv_avg: 4.0,
            recv_dvi: 7.0,
            recv_max: 100,
            ..InetStat::default()
        };
        stat.input(9);
        assert_eq!(stat.recv_cnt, 1, "the wrapped n is treated as 1");
        assert_eq!(stat.recv_avg, 9.0, "the new length becomes the average");
        assert_eq!(stat.recv_dvi, 0.0, "and the deviation restarts from zero");
        assert_eq!(stat.recv_max, 100, "the peak is unaffected by the wrap");
        assert_eq!(stat.recv_oct, 9);

        let mut stat = InetStat {
            send_cnt: u64::MAX,
            send_avg: 4.0,
            send_max: 100,
            ..InetStat::default()
        };
        stat.output(9);
        assert_eq!(stat.send_cnt, 1);
        assert_eq!(stat.send_avg, 9.0);
        assert_eq!(stat.send_max, 100);
        assert_eq!(stat.send_oct, 9);
    }

    /// A zero-length payload counts as one more event without moving any average.
    #[test]
    fn a_zero_length_payload_counts_without_moving_the_average() {
        let mut stat = InetStat::default();
        stat.input(0);
        stat.input(0);
        assert_eq!(stat.recv_cnt, 2);
        assert_eq!(stat.recv_oct, 0);
        assert_eq!(stat.recv_avg, 0.0);
        assert_eq!(stat.recv_dvi, 0.0);
        assert_eq!(stat.recv_max, 0);

        stat.output(0);
        assert_eq!(stat.send_cnt, 1);
        assert_eq!(stat.send_oct, 0);
        assert_eq!(stat.send_avg, 0.0);
        assert_eq!(stat.send_max, 0);
    }
}
