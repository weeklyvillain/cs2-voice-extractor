//! Tiny protobuf wire-format reader. We only read the handful of fields we need,
//! so unknown/new fields Valve adds are skipped transparently.

pub fn read_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result: u64 = 0;
    for i in 0..10 {
        let b = *data.get(*pos)?;
        *pos += 1;
        result |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Some(result);
        }
    }
    None
}

#[derive(Debug, Clone, Copy)]
pub enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

impl<'a> Value<'a> {
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Value::Varint(v) | Value::Fixed64(v) => Some(v),
            Value::Fixed32(v) => Some(v as u64),
            Value::Bytes(_) => None,
        }
    }
    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        match *self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }
    pub fn as_f32(&self) -> Option<f32> {
        match *self {
            Value::Fixed32(v) => Some(f32::from_bits(v)),
            _ => None,
        }
    }
}

/// Iterates `(field_number, value)` pairs. Stops silently on malformed input.
pub struct Fields<'a> {
    data: &'a [u8],
    pos: usize,
}

pub fn fields(data: &[u8]) -> Fields<'_> {
    Fields { data, pos: 0 }
}

impl<'a> Iterator for Fields<'a> {
    type Item = (u32, Value<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.data.len() {
            return None;
        }
        let key = read_varint(self.data, &mut self.pos)?;
        let field = (key >> 3) as u32;
        let value = match key & 7 {
            0 => Value::Varint(read_varint(self.data, &mut self.pos)?),
            1 => {
                let b = self.data.get(self.pos..self.pos + 8)?;
                self.pos += 8;
                Value::Fixed64(u64::from_le_bytes(b.try_into().ok()?))
            }
            2 => {
                let len = read_varint(self.data, &mut self.pos)? as usize;
                let b = self.data.get(self.pos..self.pos.checked_add(len)?)?;
                self.pos += len;
                Value::Bytes(b)
            }
            5 => {
                let b = self.data.get(self.pos..self.pos + 4)?;
                self.pos += 4;
                Value::Fixed32(u32::from_le_bytes(b.try_into().ok()?))
            }
            _ => {
                self.pos = self.data.len();
                return None;
            }
        };
        Some((field, value))
    }
}
