//! Minimal CS2 demo scanner.
//!
//! We deliberately do *not* parse entities, string tables, send tables, game events
//! or anything else that Valve changes regularly. We only walk the outer frame
//! container, look inside packet frames, and pick out `svc_ServerInfo` (for the tick
//! interval) and `svc_VoiceData`. Every other net message is skipped by its length
//! prefix without being looked at.

use std::io::{self, Read};

use anyhow::{Context, Result, bail};

use crate::bits::BitReader;
use crate::proto::{self, Value};

const MAGIC: &[u8; 8] = b"PBDEMS2\0";

// EDemoCommands
const DEM_STOP: u64 = 0;
const DEM_PACKET: u64 = 7;
const DEM_SIGNON_PACKET: u64 = 8;
const DEM_IS_COMPRESSED: u64 = 64;

// SVC_Messages
const SVC_SERVER_INFO: u32 = 40;
const SVC_VOICE_DATA: u32 = 47;

pub const FORMAT_STEAM: u32 = 0;
pub const FORMAT_ENGINE: u32 = 1;
pub const FORMAT_OPUS: u32 = 2;

#[derive(Debug, Clone)]
pub struct VoiceMessage {
    /// Demo frame tick the message was recorded at.
    pub tick: u32,
    pub xuid: u64,
    pub client: i32,
    pub format: u32,
    pub data: Vec<u8>,
    pub packet_offsets: Vec<u32>,
    pub num_packets: u32,
    pub sample_rate: u32,
    /// CMsgVoiceAudio.section_number: increments by one per voice message (Opus format).
    pub sequence: Option<u32>,
}

#[derive(Debug, Default)]
pub struct DemoScan {
    pub voice: Vec<VoiceMessage>,
    /// Seconds per tick, from svc_ServerInfo (falls back to 1/64).
    pub tick_interval: f32,
    pub map_name: Option<String>,
    pub first_tick: Option<u32>,
    pub last_tick: u32,
    pub truncated: bool,
}

/// Scans a demo held in memory. `want(xuid)` lets the caller drop voice of
/// players it doesn't care about early.
pub fn scan(data: &[u8], want: &dyn Fn(u64) -> bool) -> Result<DemoScan> {
    scan_reader(data, want)
}

/// Reads one varint from a stream. `Ok(None)` is a clean EOF before the first byte;
/// EOF in the middle of a varint is reported as `UnexpectedEof`.
fn read_varint_io<R: Read>(r: &mut R, consumed: &mut u64) -> io::Result<Option<u64>> {
    let mut result: u64 = 0;
    for i in 0..10 {
        let mut b = [0u8; 1];
        if r.read(&mut b)? == 0 {
            if i == 0 {
                return Ok(None);
            }
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        *consumed += 1;
        result |= ((b[0] & 0x7f) as u64) << (7 * i);
        if b[0] & 0x80 == 0 {
            return Ok(Some(result));
        }
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "varint too long"))
}

/// Scans a demo from a stream (a file, stdin, a socket, ...), one frame at a time,
/// so the demo never has to be held in memory. Pass a buffered reader.
pub fn scan_reader<R: Read>(mut r: R, want: &dyn Fn(u64) -> bool) -> Result<DemoScan> {
    let mut header = [0u8; 16];
    let mut got = 0;
    while got < header.len() {
        match r.read(&mut header[got..]).context("could not read demo")? {
            0 => break,
            n => got += n,
        }
    }
    if got < 16 || &header[..8] != MAGIC {
        if header[..got].starts_with(b"HL2DEMO\0") {
            bail!("this is a CS:GO demo (HL2DEMO), only CS2 demos (PBDEMS2) are supported");
        }
        bail!("not a CS2 demo (missing PBDEMS2 header)");
    }

    let mut out = DemoScan {
        tick_interval: 1.0 / 64.0,
        ..Default::default()
    };
    let mut decoder = snap::raw::Decoder::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut payload_buf: Vec<u8> = Vec::new();
    let mut pos = 16u64;

    loop {
        let frame_start = pos;
        let header = (|| -> io::Result<Option<(u64, u64, u64)>> {
            let Some(cmd) = read_varint_io(&mut r, &mut pos)? else { return Ok(None) };
            let tick = read_varint_io(&mut r, &mut pos)?.ok_or(io::ErrorKind::UnexpectedEof)?;
            let size = read_varint_io(&mut r, &mut pos)?.ok_or(io::ErrorKind::UnexpectedEof)?;
            Ok(Some((cmd, tick, size)))
        })();
        let (cmd, tick, size) = match header {
            Ok(Some(h)) => h,
            Ok(None) => break,
            Err(e) if matches!(e.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData) => {
                out.truncated = true;
                break;
            }
            Err(e) => return Err(e).context("could not read demo"),
        };
        let size = size as usize;
        payload_buf.clear();
        let n = (&mut r).take(size as u64).read_to_end(&mut payload_buf).context("could not read demo")?;
        if n < size {
            out.truncated = true;
            break;
        }
        pos += size as u64;
        let payload = &payload_buf[..];

        let compressed = cmd & DEM_IS_COMPRESSED != 0;
        let cmd = cmd & !DEM_IS_COMPRESSED;
        let tick = tick as u32;

        if tick != u32::MAX {
            out.first_tick.get_or_insert(tick);
            out.last_tick = out.last_tick.max(tick);
        }

        match cmd {
            DEM_STOP => break,
            DEM_PACKET | DEM_SIGNON_PACKET => {
                let body: &[u8] = if compressed {
                    let len = snap::raw::decompress_len(payload)
                        .with_context(|| format!("bad snappy header at offset {frame_start}"))?;
                    buf.resize(len, 0);
                    match decoder.decompress(payload, &mut buf) {
                        Ok(n) => &buf[..n],
                        Err(e) => {
                            eprintln!("warning: skipping corrupt frame at offset {frame_start}: {e}");
                            continue;
                        }
                    }
                } else {
                    payload
                };
                // CDemoPacket { bytes data = 3; }
                let Some(net) = proto::fields(body)
                    .find(|(f, _)| *f == 3)
                    .and_then(|(_, v)| v.as_bytes())
                else {
                    continue;
                };
                let frame_tick = if tick == u32::MAX { 0 } else { tick };
                parse_net_messages(net, frame_tick, &mut out, want);
            }
            _ => {}
        }
    }

    Ok(out)
}

fn parse_net_messages(net: &[u8], tick: u32, out: &mut DemoScan, want: &dyn Fn(u64) -> bool) {
    let mut r = BitReader::new(net);
    while r.remaining_bits() > 8 {
        let (Some(msg_type), Some(size)) = (r.read_ubitvar(), r.read_varu32()) else {
            return;
        };
        let size = size as usize;
        match msg_type {
            SVC_VOICE_DATA => {
                let Some(body) = r.read_bytes(size) else { return };
                if let Some(msg) = parse_voice_data(&body, tick)
                    && want(msg.xuid)
                {
                    out.voice.push(msg);
                }
            }
            SVC_SERVER_INFO => {
                let Some(body) = r.read_bytes(size) else { return };
                for (field, v) in proto::fields(&body) {
                    match field {
                        // tick_interval
                        13 => {
                            if let Some(t) = v.as_f32()
                                && (1.0 / 1024.0..=1.0).contains(&t)
                            {
                                out.tick_interval = t;
                            }
                        }
                        // map_name
                        15 => {
                            if let Some(b) = v.as_bytes() {
                                out.map_name = Some(String::from_utf8_lossy(b).into_owned());
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                if r.skip_bytes(size).is_none() {
                    return;
                }
            }
        }
    }
}

/// CSVCMsg_VoiceData { CMsgVoiceAudio audio = 1; int32 client = 2; bool proximity = 3;
///                     fixed64 xuid = 4; int32 audible_mask = 5; uint32 tick = 6; }
/// CMsgVoiceAudio    { VoiceDataFormat_t format = 1; bytes voice_data = 2; int32 sequence_bytes = 3;
///                     uint32 section_number = 4; uint32 sample_rate = 5;
///                     uint32 uncompressed_sample_offset = 6; uint32 num_packets = 7;
///                     repeated uint32 packet_offsets = 8; float voice_level = 9; }
fn parse_voice_data(body: &[u8], tick: u32) -> Option<VoiceMessage> {
    let mut msg = VoiceMessage {
        tick,
        xuid: 0,
        client: -1,
        format: FORMAT_STEAM,
        data: Vec::new(),
        packet_offsets: Vec::new(),
        num_packets: 0,
        sample_rate: 0,
        sequence: None,
    };
    let mut has_audio = false;
    for (field, v) in proto::fields(body) {
        match (field, v) {
            (1, Value::Bytes(audio)) => {
                has_audio = true;
                for (af, av) in proto::fields(audio) {
                    match (af, av) {
                        (1, v) => msg.format = v.as_u64()? as u32,
                        (2, Value::Bytes(b)) => msg.data = b.to_vec(),
                        (4, v) => msg.sequence = Some(v.as_u64()? as u32),
                        (5, v) => msg.sample_rate = v.as_u64()? as u32,
                        (7, v) => msg.num_packets = v.as_u64()? as u32,
                        (8, Value::Bytes(packed)) => {
                            let mut p = 0;
                            while p < packed.len() {
                                msg.packet_offsets.push(proto::read_varint(packed, &mut p)? as u32);
                            }
                        }
                        (8, Value::Varint(v)) => msg.packet_offsets.push(v as u32),
                        _ => {}
                    }
                }
            }
            (2, Value::Varint(v)) => msg.client = v as i64 as i32,
            (4, v) => msg.xuid = v.as_u64()?,
            _ => {}
        }
    }
    (has_audio && !msg.data.is_empty()).then_some(msg)
}
