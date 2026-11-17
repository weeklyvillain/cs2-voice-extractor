//! Little-endian bit reader used for the net-message stream inside demo packets.

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // in bits
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn remaining_bits(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    /// Reads up to 32 bits.
    pub fn read_bits(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 32);
        if n == 0 {
            return Some(0);
        }
        if self.remaining_bits() < n as usize {
            return None;
        }
        let byte = self.pos / 8;
        let shift = self.pos % 8;
        let mut window: u64 = 0;
        for i in 0..8 {
            match self.data.get(byte + i) {
                Some(b) => window |= (*b as u64) << (8 * i),
                None => break,
            }
        }
        let v = (window >> shift) & ((1u64 << n) - 1);
        self.pos += n as usize;
        Some(v as u32)
    }

    pub fn read_byte(&mut self) -> Option<u8> {
        self.read_bits(8).map(|v| v as u8)
    }

    /// Valve's "ubitvar" (used for net message ids).
    pub fn read_ubitvar(&mut self) -> Option<u32> {
        let v = self.read_bits(6)?;
        Some(match v & 0x30 {
            0x10 => (v & 15) | (self.read_bits(4)? << 4),
            0x20 => (v & 15) | (self.read_bits(8)? << 4),
            0x30 => (v & 15) | (self.read_bits(28)? << 4),
            _ => v,
        })
    }

    pub fn read_varu32(&mut self) -> Option<u32> {
        let mut result: u32 = 0;
        for i in 0..5 {
            let b = self.read_byte()? as u32;
            result |= (b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Some(result);
            }
        }
        Some(result)
    }

    pub fn skip_bytes(&mut self, n: usize) -> Option<()> {
        if self.remaining_bits() < n * 8 {
            return None;
        }
        self.pos += n * 8;
        Some(())
    }

    /// Reads `n` bytes. Borrows when byte-aligned, copies otherwise.
    pub fn read_bytes(&mut self, n: usize) -> Option<std::borrow::Cow<'a, [u8]>> {
        if self.remaining_bits() < n * 8 {
            return None;
        }
        if self.pos.is_multiple_of(8) {
            let start = self.pos / 8;
            self.pos += n * 8;
            return Some(std::borrow::Cow::Borrowed(&self.data[start..start + n]));
        }
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.read_byte()?);
        }
        Some(std::borrow::Cow::Owned(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lsb_first() {
        let data = [0b1010_1100, 0xFF];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read_bits(2), Some(0b00));
        assert_eq!(r.read_bits(4), Some(0b1011));
        assert_eq!(r.read_bits(4), Some(0b1110));
        assert_eq!(r.remaining_bits(), 6);
    }
}
