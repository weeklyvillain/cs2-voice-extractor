//! Minimal Ogg Opus muxer (RFC 3533 + RFC 7845). Mono, 48 kHz, no pre-skip.

use std::io::{self, Write};

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut r = (i as u32) << 24;
        let mut j = 0;
        while j < 8 {
            r = if r & 0x8000_0000 != 0 { (r << 1) ^ 0x04c1_1db7 } else { r << 1 };
            j += 1;
        }
        table[i] = r;
        i += 1;
    }
    table
}
static CRC: [u32; 256] = crc_table();

fn crc32(data: &[u8]) -> u32 {
    data.iter()
        .fold(0u32, |crc, &b| (crc << 8) ^ CRC[((crc >> 24) as u8 ^ b) as usize])
}

const MAX_PAGE_PACKETS: usize = 50; // ~0.5-1 s of audio per page, keeps seeking cheap

pub struct OggOpusWriter<W: Write> {
    w: W,
    serial: u32,
    page_seq: u32,
    granule: u64,
    lacing: Vec<u8>,
    body: Vec<u8>,
    packets_in_page: usize,
}

impl<W: Write> OggOpusWriter<W> {
    pub fn new(w: W, serial: u32, comments: &[String]) -> io::Result<Self> {
        let mut s = Self {
            w,
            serial,
            page_seq: 0,
            granule: 0,
            lacing: Vec::new(),
            body: Vec::new(),
            packets_in_page: 0,
        };

        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1); // version
        head.push(1); // channels
        head.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
        head.extend_from_slice(&48000u32.to_le_bytes()); // input sample rate
        head.extend_from_slice(&0i16.to_le_bytes()); // output gain
        head.push(0); // channel mapping family
        s.add(&head);
        s.flush_page(0x02)?; // BOS

        let vendor = concat!("cs2-voice-extractor ", env!("CARGO_PKG_VERSION"));
        let mut tags = Vec::new();
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor.as_bytes());
        tags.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for c in comments {
            tags.extend_from_slice(&(c.len() as u32).to_le_bytes());
            tags.extend_from_slice(c.as_bytes());
        }
        s.add(&tags);
        s.flush_page(0)?;
        Ok(s)
    }

    fn add(&mut self, packet: &[u8]) {
        let mut n = packet.len();
        while n >= 255 {
            self.lacing.push(255);
            n -= 255;
        }
        self.lacing.push(n as u8);
        self.body.extend_from_slice(packet);
        self.packets_in_page += 1;
    }

    pub fn write_packet(&mut self, packet: &[u8], samples: u32) -> io::Result<()> {
        let segments = packet.len() / 255 + 1;
        if self.packets_in_page >= MAX_PAGE_PACKETS || self.lacing.len() + segments > 255 {
            self.flush_page(0)?;
        }
        self.add(packet);
        self.granule += samples as u64;
        Ok(())
    }

    pub fn samples_written(&self) -> u64 {
        self.granule
    }

    fn flush_page(&mut self, flags: u8) -> io::Result<()> {
        let mut page = Vec::with_capacity(27 + self.lacing.len() + self.body.len());
        page.extend_from_slice(b"OggS");
        page.push(0); // version
        page.push(flags);
        page.extend_from_slice(&self.granule.to_le_bytes());
        page.extend_from_slice(&self.serial.to_le_bytes());
        page.extend_from_slice(&self.page_seq.to_le_bytes());
        page.extend_from_slice(&0u32.to_le_bytes()); // crc placeholder
        page.push(self.lacing.len() as u8);
        page.extend_from_slice(&self.lacing);
        page.extend_from_slice(&self.body);
        let crc = crc32(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        self.w.write_all(&page)?;
        self.page_seq += 1;
        self.lacing.clear();
        self.body.clear();
        self.packets_in_page = 0;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.flush_page(0x04)?; // EOS
        self.w.flush()?;
        Ok(self.w)
    }
}
