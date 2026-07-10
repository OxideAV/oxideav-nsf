//! Glue against `oxideav-core` so the crate plugs into the framework
//! registry. Compiled only when the default-on `registry` feature is on.

use std::io::Read;

use oxideav_core::{
    AudioFrame, CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry,
    CodecResolver, ContainerRegistry, Decoder, Demuxer, Error, Frame, MediaType, Packet, ProbeData,
    ReadSeek, Result, SampleFormat, StreamInfo, TimeBase,
};

use crate::header::{parse_nsf, NsfHeader};
use crate::player::NsfPlayer;
use crate::OUTPUT_SAMPLE_RATE;

/// Codec id string used for both the container codec parameters and
/// the registered decoder. Mirrors the `s3m` / `mod` convention.
pub const CODEC_ID_STR: &str = "nsf";

pub fn register_codecs(reg: &mut CodecRegistry) {
    let caps = CodecCapabilities::audio("nsf_sw")
        .with_lossy(false)
        .with_lossless(true)
        .with_intra_only(false)
        .with_max_channels(1)
        .with_max_sample_rate(OUTPUT_SAMPLE_RATE);
    reg.register(
        CodecInfo::new(CodecId::new(CODEC_ID_STR))
            .capabilities(caps)
            .decoder(make_decoder),
    );
}

pub fn register_containers(reg: &mut ContainerRegistry) {
    reg.register_demuxer("nsf", open);
    reg.register_extension("nsf", "nsf");
    reg.register_extension("nsfe", "nsf");
    reg.register_probe("nsf", probe);
}

/// `NESM\x1a` at offset 0 (NSF v1) or `NSFE` (NSFe).
fn probe(p: &ProbeData) -> u8 {
    let v1 = p.buf.len() >= 5 && &p.buf[..5] == b"NESM\x1a";
    let nsfe = p.buf.len() >= 4 && &p.buf[..4] == b"NSFE";
    if v1 || nsfe {
        100
    } else {
        0
    }
}

fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut blob = Vec::new();
    input.read_to_end(&mut blob)?;
    let header = parse_nsf(&blob).map_err(|e| Error::invalid(format!("NSF: {e}")))?;

    let mut params = CodecParameters::audio(CodecId::new(CODEC_ID_STR));
    params.media_type = MediaType::Audio;
    params.channels = Some(1);
    params.sample_rate = Some(OUTPUT_SAMPLE_RATE);
    params.sample_format = Some(SampleFormat::S16);
    params.extradata = blob.clone();

    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64),
        duration: None,
        start_time: Some(0),
        params,
    };

    let metadata = build_metadata(&header);

    Ok(Box::new(NsfDemuxer {
        streams: vec![stream],
        blob,
        consumed: false,
        metadata,
    }))
}

fn build_metadata(h: &NsfHeader) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if !h.song_name.is_empty() {
        out.push(("title".into(), h.song_name.clone()));
    }
    if !h.artist.is_empty() {
        out.push(("artist".into(), h.artist.clone()));
    }
    if !h.copyright.is_empty() {
        out.push(("copyright".into(), h.copyright.clone()));
    }
    out.push((
        "extra_info".into(),
        format!(
            "{} song(s), region {:?}, expansion 0x{:02X}, play rate {:.2} Hz",
            h.total_songs,
            h.region,
            h.expansion.0,
            h.play_rate_hz()
        ),
    ));
    for (i, name) in h.track_labels.iter().enumerate() {
        out.push((format!("track_{i}"), name.clone()));
    }
    out
}

struct NsfDemuxer {
    streams: Vec<StreamInfo>,
    blob: Vec<u8>,
    consumed: bool,
    metadata: Vec<(String, String)>,
}

impl Demuxer for NsfDemuxer {
    fn format_name(&self) -> &str {
        "nsf"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if self.consumed {
            return Err(Error::Eof);
        }
        self.consumed = true;
        let data = std::mem::take(&mut self.blob);
        let stream = &self.streams[0];
        let mut pkt = Packet::new(0, stream.time_base, data);
        pkt.pts = Some(0);
        pkt.dts = Some(0);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        None
    }
}

fn make_decoder(_params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(NsfDecoder {
        codec_id: CodecId::new(CODEC_ID_STR),
        state: DecoderState::AwaitingPacket,
    }))
}

enum DecoderState {
    AwaitingPacket,
    Playing { player: Box<NsfPlayer>, pts: i64 },
    Done,
}

struct NsfDecoder {
    codec_id: CodecId,
    state: DecoderState,
}

const CHUNK_FRAMES: u32 = 1024;
/// Render at most this many seconds before declaring EOF, so a song
/// without a natural end (the common case) still terminates the
/// pipeline. 5 minutes is a comfortable upper bound for chiptune.
const MAX_RENDER_FRAMES: i64 = (OUTPUT_SAMPLE_RATE as i64) * 300;

impl Decoder for NsfDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if !matches!(self.state, DecoderState::AwaitingPacket) {
            return Err(Error::other(
                "NSF decoder received a second packet; only one is expected per song",
            ));
        }
        let header = parse_nsf(&packet.data).map_err(|e| Error::invalid(format!("NSF: {e}")))?;
        let mut player = NsfPlayer::new(header.clone(), OUTPUT_SAMPLE_RATE);
        // `starting_song_number` resolves the container-dependent base
        // (v1 header byte $07 is 1-based, NSFe INFO offset 9 is
        // 0-based) into the 1-based convention `start_song` expects.
        // The old `== 0` special-case treated an NSFe starting song of
        // e.g. 1 (meaning the SECOND track) as track 1.
        player.start_song(header.starting_song_number());
        self.state = DecoderState::Playing {
            player: Box::new(player),
            pts: 0,
        };
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match &mut self.state {
            DecoderState::AwaitingPacket => Err(Error::NeedMore),
            DecoderState::Done => Err(Error::Eof),
            DecoderState::Playing { player, pts } => {
                if *pts >= MAX_RENDER_FRAMES {
                    self.state = DecoderState::Done;
                    return Err(Error::Eof);
                }
                let mut pcm = vec![0i16; CHUNK_FRAMES as usize];
                let produced = player.render(&mut pcm);
                if produced == 0 {
                    self.state = DecoderState::Done;
                    return Err(Error::Eof);
                }
                pcm.truncate(produced);
                let mut bytes = Vec::with_capacity(pcm.len() * 2);
                for s in &pcm {
                    bytes.extend_from_slice(&s.to_le_bytes());
                }
                let frame_pts = *pts;
                *pts += produced as i64;
                Ok(Frame::Audio(AudioFrame {
                    samples: produced as u32,
                    pts: Some(frame_pts),
                    data: vec![bytes],
                }))
            }
        }
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.state = DecoderState::AwaitingPacket;
        Ok(())
    }
}
