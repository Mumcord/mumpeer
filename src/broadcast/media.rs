//! The Bovnar media profile on a broadcast link (Mumcord spec 07, FR-7.89 … FR-7.94).
//!
//! A new-format link carries one endless `.media` document. The relay never looks inside a
//! message: it only needs the channel (to drop whole temporal layers) and the keyframe flag
//! (to cache what a late joiner needs). This module reads exactly that from the mux framing
//! without copying a payload; the bytes themselves are forwarded unchanged elsewhere.

use bovnar::codec::{Item, MuxReader};

/// Channel of control messages and the stream description.
pub const CHANNEL_CONTROL: u16 = 0;
/// First video channel; temporal layer `n` travels on this plus `n`.
pub const CHANNEL_VIDEO_BASE: u16 = 1;
/// Last video channel.
pub const CHANNEL_VIDEO_LAST: u16 = 4;
/// Channel of the share sound.
pub const CHANNEL_AUDIO: u16 = 5;
/// Channel of plugin relay data.
pub const CHANNEL_PLUGIN_DATA: u16 = 6;
/// Highest channel the profile allows.
pub const MAX_CHANNEL: u16 = 4095;
/// The `media` value of a new-format bind and status (FR-7.96).
pub const MEDIA_VERSION: u8 = 2;

/// Largest video track message.
pub const MAX_VIDEO_MESSAGE: usize = 4 * 1024 * 1024 + 64;
/// Largest audio track message.
pub const MAX_AUDIO_MESSAGE: usize = 4 * 1024 + 64;
/// Largest plugin data message.
pub const MAX_PLUGIN_MESSAGE: usize = 64 * 1024 + 64;
/// Keyframe bit of the track flags byte.
pub const FLAG_KEYFRAME: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaError {
    Malformed,
    BadMessage,
    OutOfProfile,
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Malformed => "malformed media document",
            Self::BadMessage => "malformed media message",
            Self::OutOfProfile => "media message outside the profile",
        })
    }
}

impl std::error::Error for MediaError {}

/// What a channel carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelUse {
    Control,
    VideoLayer(u8),
    Audio,
    PluginData,
    Reserved,
}

/// Classifies a channel, `None` above [`MAX_CHANNEL`].
pub const fn channel_use(channel: u16) -> Option<ChannelUse> {
    match channel {
        CHANNEL_CONTROL => Some(ChannelUse::Control),
        CHANNEL_VIDEO_BASE..=CHANNEL_VIDEO_LAST => {
            Some(ChannelUse::VideoLayer((channel - CHANNEL_VIDEO_BASE) as u8))
        }
        CHANNEL_AUDIO => Some(ChannelUse::Audio),
        CHANNEL_PLUGIN_DATA => Some(ChannelUse::PluginData),
        7..=MAX_CHANNEL => Some(ChannelUse::Reserved),
        _ => None,
    }
}

/// The size cap of a whole track message on `channel`.
pub const fn message_limit(channel: u16) -> Option<usize> {
    match channel {
        CHANNEL_VIDEO_BASE..=CHANNEL_VIDEO_LAST => Some(MAX_VIDEO_MESSAGE),
        CHANNEL_AUDIO => Some(MAX_AUDIO_MESSAGE),
        CHANNEL_PLUGIN_DATA => Some(MAX_PLUGIN_MESSAGE),
        _ => None,
    }
}

/// The routing metadata of one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub channel: u16,
    /// Payload length from the chunk header.
    pub len: usize,
    /// The track flags byte; `None` on channel 0.
    pub flags: Option<u8>,
}

impl Meta {
    pub fn keyframe(self) -> bool {
        self.flags.is_some_and(|flags| flags & FLAG_KEYFRAME != 0)
    }

    pub fn video_layer(self) -> Option<u8> {
        match channel_use(self.channel) {
            Some(ChannelUse::VideoLayer(layer)) => Some(layer),
            _ => None,
        }
    }
}

/// Reads routing metadata from the chunks of one new-format link, without gathering payloads.
pub struct MetaReader {
    mux: MuxReader,
    buffer: Vec<u8>,
    fatal: bool,
}

impl MetaReader {
    pub fn new() -> Self {
        Self {
            mux: MuxReader::with_options(MAX_VIDEO_MESSAGE as u64, Some("media"))
                .expect("the fixed media profile is valid"),
            buffer: Vec::new(),
            fatal: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Checks that the input ended cleanly at a message boundary.
    pub fn finish(&mut self) -> Result<(), MediaError> {
        match self.mux.finish() {
            Ok(()) => Ok(()),
            Err(_) => {
                self.fatal = true;
                Err(MediaError::Malformed)
            }
        }
    }

    /// The next started message, or `None` once the pushed bytes are used up. The payload is
    /// skipped, not buffered.
    pub fn next_meta(&mut self) -> Result<Option<Meta>, MediaError> {
        if self.fatal {
            return Err(MediaError::Malformed);
        }
        let (consumed, outcome) = {
            let mut input = &self.buffer[..];
            let item = self.mux.next_item(&mut input, 1);
            let consumed = self.buffer.len() - input.len();
            let outcome = match item {
                Ok(Some(Item::Peek(peek))) => {
                    let channel =
                        u16::try_from(peek.channel).map_err(|_| MediaError::OutOfProfile)?;
                    let len = usize::try_from(peek.len).map_err(|_| MediaError::OutOfProfile)?;
                    if channel > MAX_CHANNEL {
                        return Err(MediaError::OutOfProfile);
                    }
                    if matches!(channel_use(channel), Some(ChannelUse::Reserved)) {
                        return Err(MediaError::OutOfProfile);
                    }
                    if let Some(limit) = message_limit(channel)
                        && len > limit
                    {
                        return Err(MediaError::OutOfProfile);
                    }
                    let flags = match channel_use(channel) {
                        Some(ChannelUse::Control | ChannelUse::Reserved) => None,
                        Some(
                            ChannelUse::VideoLayer(_) | ChannelUse::Audio | ChannelUse::PluginData,
                        ) => Some(*peek.head.first().ok_or(MediaError::BadMessage)?),
                        None => return Err(MediaError::OutOfProfile),
                    };
                    peek.skip();
                    Ok(Some(Meta {
                        channel,
                        len,
                        flags,
                    }))
                }
                Ok(Some(Item::Message(_) | Item::Wire(_))) => Err(MediaError::BadMessage),
                Ok(None) => Ok(None),
                Err(_) => Err(MediaError::Malformed),
            };
            (consumed, outcome)
        };
        if consumed > 0 {
            self.buffer.drain(..consumed);
        }
        match outcome {
            Ok(Some(meta)) => Ok(Some(meta)),
            Ok(None) => Ok(None),
            Err(error) => {
                self.fatal = true;
                Err(error)
            }
        }
    }
}

impl Default for MetaReader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use bovnar::codec::MuxWriter;

    use super::*;

    fn document(messages: &[(u64, &[u8])]) -> Vec<u8> {
        let writer = MuxWriter::with_preamble(b"#!bovnar 1.2\n", "media").expect("opening");
        let mut out = Vec::new();
        writer.begin(&mut out);
        for (channel, payload) in messages {
            writer.write_message(&mut out, *channel, payload);
        }
        writer.finish(&mut out);
        out
    }

    fn metas(messages: &[(u64, &[u8])], piece: usize) -> Vec<Meta> {
        let doc = document(messages);
        let mut reader = MetaReader::new();
        let mut out = Vec::new();
        for part in doc.chunks(piece) {
            reader.push(part);
            while let Some(meta) = reader.next_meta().expect("valid") {
                out.push(meta);
            }
        }
        out
    }

    #[test]
    fn every_channel_and_flag_is_reported() {
        let video = [FLAG_KEYFRAME, 9, 9, 9];
        let delta = [0, 8, 8];
        let messages: &[(u64, &[u8])] = &[
            (0, b"description"),
            (1, &video),
            (3, &delta),
            (5, &delta),
            (6, &delta),
        ];
        for piece in [1, 2, 7, 64, 1024, usize::MAX] {
            let metas = metas(messages, piece);
            assert_eq!(metas.len(), messages.len());
            assert_eq!(metas[0].channel, 0);
            assert_eq!(metas[0].flags, None);
            assert_eq!(metas[1].channel, 1);
            assert!(metas[1].keyframe());
            assert_eq!(metas[1].video_layer(), Some(0));
            assert_eq!(metas[2].video_layer(), Some(2));
            assert!(!metas[2].keyframe());
            assert_eq!(metas[3].channel, 5);
            assert_eq!(metas[4].channel, 6);
        }
    }

    #[test]
    fn reserved_and_oversized_channels_are_refused() {
        let mut reader = MetaReader::new();
        reader.push(&document(&[(7, b"reserved")]));
        assert_eq!(reader.next_meta(), Err(MediaError::OutOfProfile));

        let mut reader = MetaReader::new();
        let big = vec![0u8; MAX_AUDIO_MESSAGE + 1];
        reader.push(&document(&[(5, &big)]));
        assert_eq!(reader.next_meta(), Err(MediaError::OutOfProfile));
    }

    #[test]
    fn a_malformed_document_stays_failed() {
        let mut reader = MetaReader::new();
        reader.push(b"not a media document\n");
        assert_eq!(reader.next_meta(), Ok(None));
        assert_eq!(reader.finish(), Err(MediaError::Malformed));
        assert_eq!(reader.next_meta(), Err(MediaError::Malformed));
    }
}
