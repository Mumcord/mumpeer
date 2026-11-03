//! The broadcast wire format: the 14-byte clear header in front of every unit and the control
//! units the relay reads or writes (spec 07 FR-7.43, §6.6). It mirrors `mumcord_p2p::broadcast`
//! byte for byte and asserts the same test vectors; media payloads stay opaque.

use bytes::{BufMut, Bytes, BytesMut};

pub const HEADER_VERSION: u8 = 1;
pub const HEADER_LEN: usize = 14;
pub const MAX_TEMPORAL_LAYER: u8 = 3;
/// AES-GCM tag and SFrame header in front of an encrypted media payload.
const ENCRYPTION_OVERHEAD: u32 = 64;
pub const MAX_VIDEO_PAYLOAD: u32 = 4 * 1024 * 1024 + ENCRYPTION_OVERHEAD;
pub const MAX_AUDIO_PAYLOAD: u32 = 4 * 1024 + ENCRYPTION_OVERHEAD;
/// The opaque body of a `keyreq` or `key`.
pub const MAX_KEY_BODY: u32 = 1024;
pub const SLOT_LEN: u32 = 2;

const KIND_VIDEO: u8 = 1;
const KIND_AUDIO: u8 = 2;
const KIND_CONTROL: u8 = 3;
const FLAG_KEYFRAME: u8 = 0b1;
const CODEC_VP9: u8 = 1;
const CODEC_AV1: u8 = 2;
const CODEC_OPUS: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    Vp9,
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKind {
    /// Subscriber → publisher: the start of the key exchange; the relay prepends the slot.
    KeyRequest = 1,
    /// Publisher → one subscriber: `slot ‖ body`; the relay strips the slot.
    Key = 2,
    /// Subscriber → publisher: please send a keyframe (rate-limited by the relay).
    KeyframeRequest = 3,
    /// Subscriber → relay: the highest temporal layer it wants.
    Layers = 4,
    /// Relay → publisher: the subscriber in this slot left.
    Gone = 5,
}

impl ControlKind {
    fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            1 => Self::KeyRequest,
            2 => Self::Key,
            3 => Self::KeyframeRequest,
            4 => Self::Layers,
            5 => Self::Gone,
            _ => return None,
        })
    }

    fn payload_range(self) -> (u32, u32) {
        match self {
            Self::KeyRequest | Self::Key => (0, SLOT_LEN + MAX_KEY_BODY),
            Self::KeyframeRequest => (0, 0),
            Self::Layers => (1, 1),
            Self::Gone => (SLOT_LEN, SLOT_LEN),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Content {
    Video {
        codec: VideoCodec,
        keyframe: bool,
        temporal_layer: u8,
    },
    Audio,
    Control(ControlKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub content: Content,
    pub key_id: u8,
    pub sequence: u32,
    pub payload_len: u32,
}

/// Any header the relay cannot accept; the link that sent it is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadHeader;

impl Header {
    fn control(kind: ControlKind, payload_len: u32) -> Self {
        Self {
            content: Content::Control(kind),
            key_id: 0,
            sequence: 0,
            payload_len,
        }
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let (kind, flags, layer, codec) = match self.content {
            Content::Video {
                codec,
                keyframe,
                temporal_layer,
            } => (
                KIND_VIDEO,
                if keyframe { FLAG_KEYFRAME } else { 0 },
                temporal_layer,
                match codec {
                    VideoCodec::Vp9 => CODEC_VP9,
                    VideoCodec::Av1 => CODEC_AV1,
                },
            ),
            Content::Audio => (KIND_AUDIO, 0, 0, CODEC_OPUS),
            Content::Control(control) => (KIND_CONTROL, 0, 0, control as u8),
        };
        let mut out = [0u8; HEADER_LEN];
        out[0] = HEADER_VERSION;
        out[1] = kind;
        out[2] = flags;
        out[3] = layer;
        out[4] = codec;
        out[5] = self.key_id;
        out[6..10].copy_from_slice(&self.sequence.to_be_bytes());
        out[10..14].copy_from_slice(&self.payload_len.to_be_bytes());
        out
    }

    /// Checks the whole header, as every receiver must before reading the payload.
    pub fn decode(bytes: &[u8]) -> Result<Self, BadHeader> {
        let Some(bytes) = bytes.get(..HEADER_LEN) else {
            return Err(BadHeader);
        };
        if bytes[0] != HEADER_VERSION {
            return Err(BadHeader);
        }
        let (flags, layer, codec) = (bytes[2], bytes[3], bytes[4]);
        let content = match bytes[1] {
            KIND_VIDEO => {
                if flags & !FLAG_KEYFRAME != 0 {
                    return Err(BadHeader);
                }
                Content::Video {
                    codec: match codec {
                        CODEC_VP9 => VideoCodec::Vp9,
                        CODEC_AV1 => VideoCodec::Av1,
                        _ => return Err(BadHeader),
                    },
                    keyframe: flags & FLAG_KEYFRAME != 0,
                    temporal_layer: layer,
                }
            }
            KIND_AUDIO if codec == CODEC_OPUS => Content::Audio,
            KIND_CONTROL => Content::Control(ControlKind::from_byte(codec).ok_or(BadHeader)?),
            _ => return Err(BadHeader),
        };
        if !matches!(content, Content::Video { .. }) && (flags != 0 || layer != 0) {
            return Err(BadHeader);
        }
        let header = Self {
            content,
            key_id: bytes[5],
            sequence: u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]),
            payload_len: u32::from_be_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]),
        };
        let (min, max) = match header.content {
            Content::Video { temporal_layer, .. } => {
                if temporal_layer > MAX_TEMPORAL_LAYER {
                    return Err(BadHeader);
                }
                (1, MAX_VIDEO_PAYLOAD)
            }
            Content::Audio => (1, MAX_AUDIO_PAYLOAD),
            Content::Control(control) => {
                if header.key_id != 0 || header.sequence != 0 {
                    return Err(BadHeader);
                }
                control.payload_range()
            }
        };
        if (min..=max).contains(&header.payload_len) {
            Ok(header)
        } else {
            Err(BadHeader)
        }
    }

    /// Header plus payload.
    pub fn unit_len(&self) -> usize {
        HEADER_LEN + self.payload_len as usize
    }
}

/// One complete unit as it travels: the header (parsed) and the unchanged bytes, header
/// included. Cloning shares the bytes, so a frame is stored once for all subscribers.
#[derive(Debug, Clone)]
pub struct Unit {
    pub header: Header,
    pub bytes: Bytes,
}

impl Unit {
    /// A unit received as a whole (a QUIC datagram).
    pub fn parse(bytes: Bytes) -> Result<Self, BadHeader> {
        let header = Header::decode(&bytes)?;
        if bytes.len() != header.unit_len() {
            return Err(BadHeader);
        }
        Ok(Self { header, bytes })
    }

    pub fn payload(&self) -> &[u8] {
        &self.bytes[HEADER_LEN..]
    }

    fn control(kind: ControlKind, parts: &[&[u8]]) -> Self {
        let len: usize = parts.iter().map(|part| part.len()).sum();
        // Control payloads are at most a few KiB; the caller checked the sizes.
        let header = Header::control(kind, u32::try_from(len).unwrap_or(u32::MAX));
        let mut bytes = BytesMut::with_capacity(HEADER_LEN + len);
        bytes.put_slice(&header.encode());
        for part in parts {
            bytes.put_slice(part);
        }
        Self {
            header,
            bytes: bytes.freeze(),
        }
    }

    /// `keyreq` for the publisher: the subscriber's body with its slot in front.
    pub fn key_request(slot: u16, body: &[u8]) -> Self {
        Self::control(ControlKind::KeyRequest, &[&slot.to_be_bytes(), body])
    }

    /// `key` for a subscriber: the publisher's body without the slot.
    pub fn key(body: &[u8]) -> Self {
        Self::control(ControlKind::Key, &[body])
    }

    pub fn keyframe_request() -> Self {
        Self::control(ControlKind::KeyframeRequest, &[])
    }

    pub fn gone(slot: u16) -> Self {
        Self::control(ControlKind::Gone, &[&slot.to_be_bytes()])
    }
}

/// The slot in front of a `key` or `keyreq` payload, and the body after it.
pub fn split_slot(payload: &[u8]) -> Option<(u16, &[u8])> {
    let (slot, rest) = payload.split_first_chunk::<2>()?;
    Some((u16::from_be_bytes(*slot), rest))
}

/// Puts units back together from the byte stream of TCP `0x01` frames, which may cut a unit
/// anywhere. It allocates once per unit, never per frame.
pub struct Assembler {
    header: [u8; HEADER_LEN],
    have: usize,
    unit: Option<(Header, BytesMut)>,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    pub fn new() -> Self {
        Self {
            header: [0; HEADER_LEN],
            have: 0,
            unit: None,
        }
    }

    /// Feeds the payload of one frame. `accept` sees each header before its payload is buffered;
    /// `false` there or a bad header stops with an error. `on_unit` gets every complete unit and
    /// stops with an error when it returns `false`.
    pub fn feed(
        &mut self,
        mut data: &[u8],
        accept: impl Fn(&Header) -> bool,
        mut on_unit: impl FnMut(Unit) -> bool,
    ) -> Result<(), BadHeader> {
        while !data.is_empty() {
            match &mut self.unit {
                None => {
                    let take = (HEADER_LEN - self.have).min(data.len());
                    self.header[self.have..self.have + take].copy_from_slice(&data[..take]);
                    self.have += take;
                    data = &data[take..];
                    if self.have < HEADER_LEN {
                        break;
                    }
                    let header = Header::decode(&self.header)?;
                    if !accept(&header) {
                        return Err(BadHeader);
                    }
                    let mut bytes = BytesMut::with_capacity(header.unit_len());
                    bytes.put_slice(&self.header);
                    self.unit = Some((header, bytes));
                }
                Some((header, bytes)) => {
                    let take = (header.unit_len() - bytes.len()).min(data.len());
                    bytes.put_slice(&data[..take]);
                    data = &data[take..];
                }
            }
            if let Some((header, bytes)) = &self.unit
                && bytes.len() == header.unit_len()
            {
                let header = *header;
                let bytes = self
                    .unit
                    .take()
                    .map(|(_, b)| b.freeze())
                    .unwrap_or_default();
                self.have = 0;
                if !on_unit(Unit { header, bytes }) {
                    return Err(BadHeader);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    fn video(codec: VideoCodec, keyframe: bool, layer: u8, key: u8, seq: u32, len: u32) -> Header {
        Header {
            content: Content::Video {
                codec,
                keyframe,
                temporal_layer: layer,
            },
            key_id: key,
            sequence: seq,
            payload_len: len,
        }
    }

    /// The test vectors of `crates/p2p/README.md` in Mumcord.
    fn vectors() -> Vec<(&'static str, Header)> {
        vec![
            (
                "0101010002020000000100000400",
                video(VideoCodec::Av1, true, 0, 2, 1, 1024),
            ),
            (
                "0101000201070000002a000000c8",
                video(VideoCodec::Vp9, false, 2, 7, 42, 200),
            ),
            (
                "010200000302000000020000005c",
                Header {
                    content: Content::Audio,
                    key_id: 2,
                    sequence: 2,
                    payload_len: 92,
                },
            ),
            (
                "0103000001000000000000000062",
                Header::control(ControlKind::KeyRequest, 98),
            ),
            (
                "0103000002000000000000000064",
                Header::control(ControlKind::Key, 100),
            ),
            (
                "0103000003000000000000000000",
                Header::control(ControlKind::KeyframeRequest, 0),
            ),
            (
                "0103000004000000000000000001",
                Header::control(ControlKind::Layers, 1),
            ),
            (
                "0103000005000000000000000002",
                Header::control(ControlKind::Gone, 2),
            ),
        ]
    }

    #[test]
    fn test_vectors_round_trip() {
        for (text, header) in vectors() {
            assert_eq!(hex(&header.encode()), text);
            assert_eq!(Header::decode(&unhex(text)), Ok(header), "{text}");
        }
    }

    #[test]
    fn invalid_headers_are_refused() {
        let bad = [
            "0201010002020000000100000400", // version
            "0104010002020000000100000400", // kind
            "0101030002020000000100000400", // flags
            "0101010402020000000100000400", // layer 4
            "0101010003020000000100000400", // video codec
            "0102000001020000000200000010", // audio codec
            "0102010003020000000200000010", // audio flags
            "0103000006000000000000000000", // control type
            "0103000003010000000000000000", // control with key id
            "0103000003000000000100000000", // control with sequence
            "0103000003000000000000000001", // kf with payload
            "0103000004000000000000000002", // layers of two bytes
            "0103000001000000000000000403", // keyreq over 1026 bytes
            "0101010002020000000100000000", // empty video
            "0101010002020000000100400041", // video over 4 MiB + 64
            "0102000003020000000200001041", // audio over 4 KiB + 64
            "01010100020200000001000004",   // truncated
        ];
        for text in bad {
            assert_eq!(Header::decode(&unhex(text)), Err(BadHeader), "{text}");
        }
        let largest = video(VideoCodec::Av1, true, 3, 0, 0, MAX_VIDEO_PAYLOAD);
        assert_eq!(Header::decode(&largest.encode()), Ok(largest));
    }

    #[test]
    fn control_units_carry_and_strip_slots() {
        let request = Unit::key_request(7, b"pub-key");
        assert_eq!(
            &request.bytes[..HEADER_LEN],
            &unhex("0103000001000000000000000009")[..]
        );
        assert_eq!(split_slot(request.payload()), Some((7, &b"pub-key"[..])));
        let gone = Unit::gone(2);
        assert_eq!(hex(&gone.bytes), "01030000050000000000000000020002");
        assert_eq!(Unit::key(b"x").header, Header::control(ControlKind::Key, 1));
        assert!(Unit::parse(Unit::keyframe_request().bytes).is_ok());
        assert!(Unit::parse(Bytes::from(unhex("0103000004000000000000000001"))).is_err());
    }

    #[test]
    fn assembler_rebuilds_units_cut_anywhere() {
        let mut stream = Vec::new();
        let first = video(VideoCodec::Vp9, true, 0, 1, 1, 3000);
        stream.extend_from_slice(&first.encode());
        stream.extend((0..3000u32).map(|i| i as u8));
        stream.extend_from_slice(&Unit::keyframe_request().bytes);
        stream.extend_from_slice(&Unit::gone(9).bytes);
        for cut in [1, 5, 14, 1500] {
            let mut assembler = Assembler::new();
            let mut units = Vec::new();
            for chunk in stream.chunks(cut) {
                assembler
                    .feed(
                        chunk,
                        |_| true,
                        |unit| {
                            units.push(unit);
                            true
                        },
                    )
                    .unwrap();
            }
            assert_eq!(units.len(), 3, "cut {cut}");
            assert_eq!(units[0].header, first);
            assert_eq!(units[0].payload()[2999], (2999u32 as u8));
            assert_eq!(units[2].payload(), &[0, 9]);
        }
        let mut assembler = Assembler::new();
        assert_eq!(
            assembler.feed(
                &stream,
                |h| matches!(h.content, Content::Control(_)),
                |_| true
            ),
            Err(BadHeader)
        );
        let mut assembler = Assembler::new();
        assert_eq!(assembler.feed(&[2; 20], |_| true, |_| true), Err(BadHeader));
    }
}
