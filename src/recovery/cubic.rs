// Copyright (C) 2019, Cloudflare, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright notice,
//       this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

//! CUBIC Congestion Control
//!
//! This implementation is based on the following RFC:
//!
//! https://tools.ietf.org/html/rfc8312
//!
//! Note that Slow Start can use HyStart++ when enabled.

use std::cmp;
use std::time::Duration;
use std::time::Instant;

use crate::packet;
use crate::recovery;
use crate::recovery::reno;
use crate::recovery::CongestionControlOps;
use crate::recovery::Recovery;
use crate::recovery::Sent;

pub static CUBIC: CongestionControlOps = CongestionControlOps {
    on_packet_sent,
    on_packet_acked,
    congestion_event,
    collapse_cwnd,
};

/// CUBIC Constants.
///
/// These are recommended value in RFC8312.
const BETA_CUBIC: f64 = 0.7;

const C: f64 = 0.4;

/// CUBIC State Variables.
///
/// We need to keep those variables across the connection.
/// k, w_max, w_last_max is described in the RFC.
#[derive(Debug, Default)]
pub struct State {
    k: f64,

    w_max: f64,

    w_last_max: f64,

    // Used in CUBIC fix (see on_packet_sent())
    last_sent_time: Option<Instant>,
}

/// CUBIC Functions.
///
/// Note that these calculations are based on a count of cwnd as bytes,
/// not packets.
/// Unit of t (duration) and RTT are based on seconds (f64).
impl State {
    // K = cbrt(w_max * (1 - beta_cubic) / C) (Eq. 2)
    fn cubic_k(&self) -> f64 {
        let w_max = self.w_max / recovery::MAX_DATAGRAM_SIZE as f64;
        libm::cbrt(w_max * (1.0 - BETA_CUBIC) / C)
    }

    // W_cubic(t) = C * (t - K)^3 - w_max (Eq. 1)
    fn w_cubic(&self, t: Duration) -> f64 {
        let w_max = self.w_max / recovery::MAX_DATAGRAM_SIZE as f64;

        (C * (t.as_secs_f64() - self.k).powi(3) + w_max) *
            recovery::MAX_DATAGRAM_SIZE as f64
    }

    // W_est(t) = w_max * beta_cubic + 3 * (1 - beta_cubic) / (1 + beta_cubic) *
    // (t / RTT) (Eq. 4)
    fn w_est(&self, t: Duration, rtt: Duration) -> f64 {
        let w_max = self.w_max / recovery::MAX_DATAGRAM_SIZE as f64;
        (w_max * BETA_CUBIC +
            3.0 * (1.0 - BETA_CUBIC) / (1.0 + BETA_CUBIC) * t.as_secs_f64() /
                rtt.as_secs_f64()) *
            recovery::MAX_DATAGRAM_SIZE as f64
    }
}

fn collapse_cwnd(r: &mut Recovery) {
    let cubic = &mut r.cubic_state;

    r.congestion_recovery_start_time = None;

    cubic.w_last_max = r.congestion_window as f64;
    cubic.w_max = cubic.w_last_max;

    // 4.7 Timeout - reduce ssthresh based on BETA_CUBIC
    r.ssthresh = (r.congestion_window as f64 * BETA_CUBIC) as usize;
    r.ssthresh = cmp::max(r.ssthresh, recovery::MINIMUM_WINDOW);

    reno::collapse_cwnd(r);
}

fn on_packet_sent(r: &mut Recovery, sent_bytes: usize, now: Instant) {
    // See https://github.com/torvalds/linux/commit/30927520dbae297182990bb21d08762bcc35ce1d
    // First transmit when no packets in flight
    let cubic = &mut r.cubic_state;

    if let Some(last_sent_time) = cubic.last_sent_time {
        if r.bytes_in_flight == 0 {
            let delta = now - last_sent_time;

            // We were application limited (idle) for a while.
            // Shift epoch start to keep cwnd growth to cubic curve.
            if let Some(recovery_start_time) = r.congestion_recovery_start_time {
                if delta.as_nanos() > 0 {
                    r.congestion_recovery_start_time =
                        Some(recovery_start_time + delta);
                }
            }
        }
    }

    cubic.last_sent_time = Some(now);

    reno::on_packet_sent(r, sent_bytes, now);
}

fn on_packet_acked(
    r: &mut Recovery, epoch: packet::Epoch, packet: &Sent, now: Instant,
) {
    let in_congestion_recovery = r.in_congestion_recovery(packet.time_sent);
    let cubic = &mut r.cubic_state;

    r.bytes_in_flight = r.bytes_in_flight.saturating_sub(packet.size);

    if in_congestion_recovery {
        return;
    }

    if r.app_limited {
        return;
    }

    if r.congestion_window < r.ssthresh {
        // Slow start.
        if r.hystart.enabled() && epoch == packet::EPOCH_APPLICATION {
            let (cwnd, ssthresh) = r.hystart_on_packet_acked(packet);

            r.congestion_window = cwnd;
            r.ssthresh = ssthresh;
        } else {
            // Reno Slow Start.
            r.congestion_window += packet.size;
        }
    } else {
        // Congestion avoidance.

        // When we come here without congestion_event() triggered,
        // This value can be None. In this case we initialize
        // the value here.
        if r.congestion_recovery_start_time.is_none() {
            r.congestion_recovery_start_time = Some(now);

            // This is also when the first congestion avoidance after a
            // timeout. Following 4.7 of RFC, set k to 0 and reset
            // w_max to cwnd during this period.
            cubic.k = 0.0;

            cubic.w_max = r.congestion_window as f64;
        }

        let t = now - r.congestion_recovery_start_time.unwrap();

        // w_cubic(t + rtt)
        let w_cubic = cubic.w_cubic(t + r.min_rtt);

        // w_est(t)
        let w_est = cubic.w_est(t, r.min_rtt);

        let mut cubic_cwnd = r.congestion_window;

        if w_cubic < w_est {
            // TCP friendly region.
            cubic_cwnd = cmp::max(cubic_cwnd, w_est as usize);
        } else if cubic_cwnd < w_cubic as usize {
            // Concave region or convex region use same increment.
            let cwnd_inc = (w_cubic - cubic_cwnd as f64) / cubic_cwnd as f64 *
                recovery::MAX_DATAGRAM_SIZE as f64;

            cubic_cwnd += cwnd_inc as usize;
        }

        // When in Limited Slow Start, take the max of CA cwnd and
        // LSS cwnd.
        if r.hystart.enabled() &&
            epoch == packet::EPOCH_APPLICATION &&
            r.hystart.in_lss()
        {
            let (lss_cwnd, _) = r.hystart_on_packet_acked(packet);

            cubic_cwnd = cmp::max(cubic_cwnd, lss_cwnd);
        }

        r.congestion_window = cubic_cwnd;
    }
}

fn congestion_event(
    r: &mut Recovery, time_sent: Instant, epoch: packet::Epoch, now: Instant,
) {
    let in_congestion_recovery = r.in_congestion_recovery(time_sent);
    let cubic = &mut r.cubic_state;

    // Start a new congestion event if packet was sent after the
    // start of the previous congestion recovery period.
    if !in_congestion_recovery {
        r.congestion_recovery_start_time = Some(now);

        // Fast convergence
        if cubic.w_max < cubic.w_last_max {
            cubic.w_last_max = cubic.w_max;
            cubic.w_max = cubic.w_max as f64 * (1.0 + BETA_CUBIC) / 2.0;
        } else {
            cubic.w_last_max = cubic.w_max;
        }

        cubic.w_max = r.congestion_window as f64;
        r.ssthresh = (cubic.w_max * BETA_CUBIC) as usize;
        r.ssthresh = cmp::max(r.ssthresh, recovery::MINIMUM_WINDOW);
        r.congestion_window = r.ssthresh;
        cubic.k = cubic.cubic_k();

        if r.hystart.enabled() && epoch == packet::EPOCH_APPLICATION {
            r.hystart.congestion_event();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cubic_init() {
        let mut cfg = crate::Config::new(crate::PROTOCOL_VERSION).unwrap();
        cfg.set_cc_algorithm(recovery::CongestionControlAlgorithm::CUBIC);

        let r = Recovery::new(&cfg);

        assert!(r.cwnd() > 0);
        assert_eq!(r.bytes_in_flight, 0);
    }

    #[test]
    fn cubic_send() {
        let mut cfg = crate::Config::new(crate::PROTOCOL_VERSION).unwrap();
        cfg.set_cc_algorithm(recovery::CongestionControlAlgorithm::CUBIC);

        let mut r = Recovery::new(&cfg);

        r.on_packet_sent_cc(1000, Instant::now());

        assert_eq!(r.bytes_in_flight, 1000);
    }

    #[test]
    fn cubic_slow_start() {
        let mut cfg = crate::Config::new(crate::PROTOCOL_VERSION).unwrap();
        cfg.set_cc_algorithm(recovery::CongestionControlAlgorithm::CUBIC);

        let mut r = Recovery::new(&cfg);
        let now = Instant::now();

        let p = Sent {
            pkt_num: 0,
            frames: vec![],
            time_sent: now,
            size: 5000,
            ack_eliciting: true,
            in_flight: true,
            delivered: 0,
            delivered_time: now,
            recent_delivered_packet_sent_time: now,
            is_app_limited: false,
        };

        // Send 5k x 4 = 20k, higher than default cwnd(~15k)
        // to become no longer app limited
        r.on_packet_sent_cc(p.size, now);
        r.on_packet_sent_cc(p.size, now);
        r.on_packet_sent_cc(p.size, now);
        r.on_packet_sent_cc(p.size, now);

        let cwnd_prev = r.cwnd();

        r.on_packet_acked_cc(packet::EPOCH_APPLICATION, &p, now);

        // Check if cwnd increased by packet size (slow start)
        assert_eq!(r.cwnd(), cwnd_prev + p.size);
    }

    #[test]
    fn cubic_congestion_event() {
        let mut cfg = crate::Config::new(crate::PROTOCOL_VERSION).unwrap();
        cfg.set_cc_algorithm(recovery::CongestionControlAlgorithm::CUBIC);

        let mut r = Recovery::new(&cfg);
        let now = Instant::now();
        let prev_cwnd = r.cwnd();

        r.congestion_event(now, packet::EPOCH_APPLICATION, now);

        // In CUBIC, after congestion event, cwnd will be reduced by (1 -
        // CUBIC_BETA)
        assert_eq!(prev_cwnd as f64 * BETA_CUBIC, r.cwnd() as f64);
    }

    #[test]
    fn cubic_congestion_avoidance() {
        let mut cfg = crate::Config::new(crate::PROTOCOL_VERSION).unwrap();
        cfg.set_cc_algorithm(recovery::CongestionControlAlgorithm::CUBIC);

        let mut r = Recovery::new(&cfg);
        let now = Instant::now();
        let prev_cwnd = r.cwnd();

        // Fill up bytes_in_flight to avoid app_limited=true
        r.on_packet_sent_cc(20000, now);

        // Trigger congestion event to update ssthresh
        r.congestion_event(now, packet::EPOCH_APPLICATION, now);

        // After congestion event, cwnd will be reduced.
        assert_eq!(prev_cwnd as f64 * BETA_CUBIC, r.cwnd() as f64);

        let rtt = Duration::from_millis(100);

        let p = Sent {
            pkt_num: 0,
            frames: vec![],
            // To exit from recovery
            time_sent: now + rtt,
            size: 1000,
            ack_eliciting: true,
            in_flight: true,
            delivered: 0,
            delivered_time: now,
            recent_delivered_packet_sent_time: now,
            is_app_limited: false,
        };

        // Ack 1000 bytes with rtt=100ms
        r.update_rtt(rtt, Duration::from_millis(0), now);
        r.on_packet_acked_cc(packet::EPOCH_APPLICATION, &p, now + rtt * 2);

        // Expecting a small increase (congestion avoidance mode)
        assert_eq!(r.cwnd(), 10408);
    }

    #[test]
    fn cubic_collapse_cwnd_and_restart() {
        let mut cfg = crate::Config::new(crate::PROTOCOL_VERSION).unwrap();
        cfg.set_cc_algorithm(recovery::CongestionControlAlgorithm::CUBIC);

        let mut r = Recovery::new(&cfg);
        let now = Instant::now();

        // Fill up bytes_in_flight to avoid app_limited=true
        r.on_packet_sent_cc(30000, now);

        // Trigger congestion event to update ssthresh
        r.congestion_event(now, packet::EPOCH_APPLICATION, now);

        // After persistent congestion, cwnd should be MINIMUM_WINDOW
        r.collapse_cwnd();
        assert_eq!(r.cwnd(), recovery::MINIMUM_WINDOW);

        let p = Sent {
            pkt_num: 0,
            frames: vec![],
            // To exit from recovery
            time_sent: now + Duration::from_millis(1),
            size: 10000,
            ack_eliciting: true,
            in_flight: true,
            delivered: 0,
            delivered_time: now,
            recent_delivered_packet_sent_time: now,
            is_app_limited: false,
        };

        // rtt = 100ms
        let rtt = Duration::from_millis(100);
        std::thread::sleep(rtt);

        // Ack 10000 x 2 to exit from slow start
        r.on_packet_acked_cc(packet::EPOCH_APPLICATION, &p, now);
        std::thread::sleep(rtt);

        // This will make CC into congestion avoidance mode
        r.on_packet_acked_cc(packet::EPOCH_APPLICATION, &p, now);

        assert_eq!(r.cwnd(), recovery::MINIMUM_WINDOW + 10000);
    }
}
