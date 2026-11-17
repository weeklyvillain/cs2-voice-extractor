//! Turns raw `svc_VoiceData` messages into timed Opus packets per player and
//! lays them out on a timeline.

use std::collections::BTreeMap;
use std::io::{self, Write};

use crate::demo::{FORMAT_OPUS, FORMAT_STEAM, VoiceMessage};
use crate::ogg::OggOpusWriter;
use crate::opus;

/// Gaps in the per-player sequence numbers up to this many packets are treated
/// as packet loss and filled with silence; larger gaps are a new transmission.
const MAX_LOSS_FILL: u32 = 10;

#[derive(Debug, Clone)]
pub struct TimedPacket {
    pub tick: u32,
    pub data: Vec<u8>,
    /// Duration in 48 kHz samples.
    pub samples: u32,
    /// Silence (in 48 kHz samples) to insert before this packet for lost packets.
    pub loss_before: u32,
}

#[derive(Debug, Default)]
pub struct Track {
    pub packets: Vec<TimedPacket>,
    pub client: i32,
    seq: Option<u32>,
}

impl Track {
    pub fn voice_samples(&self) -> u64 {
        self.packets.iter().map(|p| p.samples as u64).sum()
    }
}

#[derive(Debug, Default)]
pub struct CollectStats {
    pub unsupported_format: usize,
    pub bad_packets: usize,
    pub duplicates: usize,
}

/// Player key: SteamID64, or the client slot if the demo didn't record one.
pub fn player_key(m: &VoiceMessage) -> u64 {
    if m.xuid != 0 { m.xuid } else { m.client.max(0) as u64 }
}

pub fn collect(msgs: &[VoiceMessage]) -> (BTreeMap<u64, Track>, CollectStats) {
    let mut tracks: BTreeMap<u64, Track> = BTreeMap::new();
    let mut stats = CollectStats::default();
    for m in msgs {
        let track = tracks.entry(player_key(m)).or_default();
        track.client = m.client;
        match m.format {
            FORMAT_OPUS => collect_opus(m, track, &mut stats),
            FORMAT_STEAM => collect_steam(m, track, &mut stats),
            _ => stats.unsupported_format += 1,
        }
    }
    tracks.retain(|_, t| !t.packets.is_empty());
    (tracks, stats)
}

/// A sequence number at most this far behind the last one is a duplicate or a
/// late retransmission and is dropped. Anything further back means the sender's
/// counter was reset (e.g. the player reconnected) and starts a new stream.
const STALE_WINDOW: u64 = 64;

/// Returns Some(lost packet count) for an in-order sequence number, None for a
/// duplicate/stale one. `wrap` is the counter's modulus (2^16 or 2^32).
fn advance_seq(prev: &mut Option<u32>, seq: u32, wrap: u64) -> Option<u32> {
    let Some(p) = *prev else {
        *prev = Some(seq);
        return Some(0);
    };
    let forward = (seq as u64).wrapping_sub(p as u64) & (wrap - 1);
    let backward = (p as u64).wrapping_sub(seq as u64) & (wrap - 1);
    if forward == 0 || backward <= STALE_WINDOW {
        return None;
    }
    *prev = Some(seq);
    // A counter reset (or a huge jump) is a new transmission, not packet loss.
    Some(if forward > wrap / 2 { u32::MAX } else { (forward - 1) as u32 })
}

fn push(track: &mut Track, tick: u32, data: &[u8], lost: u32, stats: &mut CollectStats) {
    let Some(samples) = opus::packet_samples(data) else {
        stats.bad_packets += 1;
        return;
    };
    let loss_before = if lost <= MAX_LOSS_FILL { lost * samples } else { 0 };
    track.packets.push(TimedPacket { tick, data: data.to_vec(), samples, loss_before });
}

/// Opus format (CS2 since 2024): `voice_data` holds `num_packets` raw Opus packets,
/// `packet_offsets` are their end offsets.
fn collect_opus(m: &VoiceMessage, track: &mut Track, stats: &mut CollectStats) {
    let mut lost = 0;
    if let Some(seq) = m.sequence {
        match advance_seq(&mut track.seq, seq, 1 << 32) {
            Some(l) => lost = l,
            None => {
                stats.duplicates += 1;
                return;
            }
        }
    }
    let n = (m.num_packets.max(1) as usize).min(m.packet_offsets.len().max(1));
    let ends = &m.packet_offsets[..m.packet_offsets.len().min(n)];
    let valid = !ends.is_empty()
        && ends.windows(2).all(|w| w[0] < w[1])
        && *ends.last().unwrap() as usize <= m.data.len();
    if !valid {
        push(track, m.tick, &m.data, lost, stats);
        return;
    }
    let mut start = 0usize;
    for (i, &end) in ends.iter().enumerate() {
        push(track, m.tick, &m.data[start..end as usize], if i == 0 { lost } else { 0 }, stats);
        start = end as usize;
    }
}

/// Steam voice format (CS2 before Feb 2024):
/// `u64 steamid | (u8 type, payload)* | u32 crc32`, where type 0x0B = u16 sample rate,
/// 0x06 = u16 len + [u16 frame_len, u16 seq, opus...]*, 0x00 = u16 silent samples.
fn collect_steam(m: &VoiceMessage, track: &mut Track, stats: &mut CollectStats) {
    let d = &m.data;
    if d.len() < 8 + 4 {
        stats.bad_packets += 1;
        return;
    }
    let body = &d[8..d.len() - 4];
    let u16_at = |b: &[u8], i: usize| b.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
    let mut i = 0;
    while i < body.len() {
        let kind = body[i];
        i += 1;
        match kind {
            0x0B | 0x00 => i += 2,
            0x04..=0x06 => {
                let Some(len) = u16_at(body, i) else { break };
                i += 2;
                let end = (i + len as usize).min(body.len());
                let frames = &body[i..end];
                i = end;
                let mut j = 0;
                while j + 2 <= frames.len() {
                    let flen = u16_at(frames, j).unwrap();
                    j += 2;
                    if flen == 0xFFFF {
                        track.seq = None; // decoder reset
                        break;
                    }
                    let Some(seq) = u16_at(frames, j) else { break };
                    j += 2;
                    let Some(pkt) = frames.get(j..j + flen as usize) else {
                        stats.bad_packets += 1;
                        break;
                    };
                    j += flen as usize;
                    match advance_seq(&mut track.seq, seq as u32, 1 << 16) {
                        Some(lost) => push(track, m.tick, pkt, lost, stats),
                        None => stats.duplicates += 1,
                    }
                }
            }
            _ => {
                stats.bad_packets += 1;
                break;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Voice placed at its real time; the file spans the whole demo.
    Full,
    /// Voice only, silence removed.
    Compact,
}

pub struct Timeline {
    pub origin_tick: u32,
    pub end_tick: u32,
    pub tick_interval: f32,
}

impl Timeline {
    fn sample_at(&self, tick: u32) -> u64 {
        let t = tick.saturating_sub(self.origin_tick) as f64 * self.tick_interval as f64;
        (t * 48000.0).round() as u64
    }
}

/// Packets more than this far apart in demo time (48 kHz samples) start a new segment.
const SEGMENT_GAP: u64 = 48000 / 4;

/// One continuous stretch of speech: where it happened in the demo and where it
/// sits in the rendered file. All values in 48 kHz samples / demo ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// Demo tick of the first packet.
    pub tick: u32,
    /// Position in demo time (relative to the timeline origin).
    pub demo_start: u64,
    /// Position in the rendered file.
    pub file_start: u64,
    /// Length in the rendered file, including filled packet loss.
    pub samples: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Rendered {
    /// Total duration of the file in samples.
    pub total_samples: u64,
    pub segments: Vec<Segment>,
}

/// Writes one player's track as an Ogg Opus stream, returning its duration and the
/// timing of each speech segment (to map file time back to demo time).
pub fn render<W: Write>(
    track: &Track,
    layout: Layout,
    timeline: &Timeline,
    w: W,
    serial: u32,
    comments: &[String],
) -> io::Result<Rendered> {
    let mut segments: Vec<Segment> = Vec::new();
    // End of the previous packet in demo time, to detect where a new segment starts.
    let mut demo_end: Option<u64> = None;
    let mut ogg = OggOpusWriter::new(w, serial, comments)?;
    let mut err = None;
    let mut fill = |ogg: &mut OggOpusWriter<W>, n: u64| {
        opus::silence(n, |p, s| {
            if err.is_none()
                && let Err(e) = ogg.write_packet(p, s)
            {
                err = Some(e);
            }
        });
    };

    for p in &track.packets {
        let target = timeline.sample_at(p.tick);
        let new_segment = demo_end.is_none_or(|end| target > end + SEGMENT_GAP);
        fill(&mut ogg, p.loss_before as u64);
        if layout == Layout::Full {
            let cursor = ogg.samples_written();
            if target > cursor {
                fill(&mut ogg, target - cursor);
            }
        }
        let file_pos = ogg.samples_written();
        if new_segment {
            segments.push(Segment { tick: p.tick, demo_start: target, file_start: file_pos, samples: 0 });
        }
        ogg.write_packet(&p.data, p.samples)?;
        let seg = segments.last_mut().unwrap();
        seg.samples = ogg.samples_written() - seg.file_start;
        let prev = demo_end.map_or(0, |e| e + p.loss_before as u64);
        demo_end = Some(prev.max(target) + p.samples as u64);
    }
    if layout == Layout::Full {
        let end = timeline.sample_at(timeline.end_tick);
        let cursor = ogg.samples_written();
        if end > cursor {
            fill(&mut ogg, end - cursor);
        }
    }
    if let Some(e) = err {
        return Err(e);
    }
    let total_samples = ogg.samples_written();
    ogg.finish()?;
    Ok(Rendered { total_samples, segments })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_in_order_and_loss() {
        let mut s = None;
        assert_eq!(advance_seq(&mut s, 100, 1 << 32), Some(0));
        assert_eq!(advance_seq(&mut s, 101, 1 << 32), Some(0));
        assert_eq!(advance_seq(&mut s, 104, 1 << 32), Some(2));
    }

    #[test]
    fn sequence_duplicates_dropped() {
        let mut s = None;
        advance_seq(&mut s, 500, 1 << 32);
        assert_eq!(advance_seq(&mut s, 500, 1 << 32), None);
        assert_eq!(advance_seq(&mut s, 490, 1 << 32), None);
        assert_eq!(s, Some(500));
    }

    #[test]
    fn sequence_reset_starts_new_stream() {
        // Seen in a real demo: a player's counter went 33291 -> 2 after a reconnect.
        let mut s = None;
        advance_seq(&mut s, 33291, 1 << 32);
        assert_eq!(advance_seq(&mut s, 2, 1 << 32).map(|l| l > MAX_LOSS_FILL), Some(true));
        assert_eq!(advance_seq(&mut s, 3, 1 << 32), Some(0));
    }

    #[test]
    fn steam_sequence_wraps() {
        let mut s = None;
        advance_seq(&mut s, 0xFFFF, 1 << 16);
        assert_eq!(advance_seq(&mut s, 0, 1 << 16), Some(0));
        assert_eq!(advance_seq(&mut s, 0xFFFF, 1 << 16), None);
    }
}
