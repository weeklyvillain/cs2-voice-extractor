//! Just enough Opus knowledge to remux packets without decoding them (RFC 6716 §3.1).

/// Number of 48 kHz samples in an Opus packet, from its TOC byte(s).
pub fn packet_samples(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][(config % 4) as usize], // SILK 10/20/40/60 ms
        12..=15 => [480, 960][(config % 2) as usize],            // Hybrid 10/20 ms
        _ => [120, 240, 480, 960][(config % 4) as usize],        // CELT 2.5/5/10/20 ms
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => (*packet.get(1)? & 0x3f) as u32,
    };
    let total = frame * frames;
    // Opus packets are at most 120 ms.
    (frames > 0 && total <= 5760).then_some(total)
}

/// A mono CELT fullband frame that decodes to digital silence
/// (`FF FE` sets the CELT silence flag), for 20/10/5/2.5 ms.
const SILENCE: [([u8; 3], u32); 4] = [
    ([0xF8, 0xFF, 0xFE], 960),
    ([0xF0, 0xFF, 0xFE], 480),
    ([0xE8, 0xFF, 0xFE], 240),
    ([0xE0, 0xFF, 0xFE], 120),
];

/// Calls `emit(packet, samples)` with silence packets covering as much of
/// `samples` as possible (2.5 ms granularity). Returns the samples covered.
pub fn silence(mut samples: u64, mut emit: impl FnMut(&[u8], u32)) -> u64 {
    let mut written = 0;
    for (pkt, len) in SILENCE {
        while samples >= len as u64 {
            emit(&pkt, len);
            samples -= len as u64;
            written += len as u64;
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toc_durations() {
        assert_eq!(packet_samples(&[0x70]), Some(480)); // hybrid FB 10ms (CS2 voice)
        assert_eq!(packet_samples(&[0xF8, 0xFF, 0xFE]), Some(960));
        assert_eq!(packet_samples(&[0x0B]), None); // code 3 without frame-count byte
        assert_eq!(packet_samples(&[0x19]), Some(2880 * 2)); // SILK 60ms, code 1
        assert_eq!(packet_samples(&[0x08 | 3, 3]), Some(960 * 3));
        assert_eq!(packet_samples(&[]), None);
    }

    #[test]
    fn silence_covers() {
        let mut n = 0;
        assert_eq!(silence(960 * 3 + 480 + 100, |_, s| n += s), 960 * 3 + 480);
        assert_eq!(n, 960 * 3 + 480);
    }
}
