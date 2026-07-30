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

    let duration_ms = scheduled_duration_ms(&header);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64),
        duration: duration_ms.map(|ms| (ms as i64) * (OUTPUT_SAMPLE_RATE as i64) / 1000),
        start_time: Some(0),
        params,
    };

    let metadata = build_metadata(&header);

    Ok(Box::new(NsfDemuxer {
        streams: vec![stream],
        blob,
        consumed: false,
        metadata,
        duration_micros: duration_ms.map(|ms| (ms as i64) * 1000),
    }))
}

/// Scheduled duration of the starting track in milliseconds, when the
/// NSFe `time` chunk declares one for it: play length + fadeout, per
/// §time/§fade of `docs/audio/nsf/nsfe-nesdev-wiki.html`. A negative
/// `time` entry is the chunk's own "player default" marker and an
/// absent/short chunk declares nothing — both yield `None` (the
/// common case: NSF rips loop forever). A negative/absent `fade`
/// entry contributes 0, matching the player's shipped default.
fn scheduled_duration_ms(h: &NsfHeader) -> Option<u64> {
    let idx = h.starting_song_index() as usize;
    let time = match h.metadata.track_times_ms.get(idx).copied() {
        Some(t) if t >= 0 => t as u64,
        _ => return None,
    };
    let fade = match h.metadata.track_fades_ms.get(idx).copied() {
        Some(f) if f >= 0 => f as u64,
        _ => 0,
    };
    Some(time + fade)
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
    /// Scheduled starting-track duration (`time` + `fade`) in
    /// microseconds, when the metadata declares one.
    duration_micros: Option<i64>,
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
        self.duration_micros
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// INIT enables pulse 1 (constant volume, length halted, timer
    /// $0FD) so the render is audibly non-silent; PLAY is the trailing
    /// RTS.
    const PROGRAM: [u8; 21] = [
        0xA9, 0x01, 0x8D, 0x15, 0x40, // LDA #$01 / STA $4015
        0xA9, 0xBF, 0x8D, 0x00, 0x40, // LDA #$BF / STA $4000
        0xA9, 0xFD, 0x8D, 0x02, 0x40, // LDA #$FD / STA $4002
        0xA9, 0x00, 0x8D, 0x03, 0x40, // LDA #$00 / STA $4003
        0x60, // RTS (doubles as PLAY at $8014)
    ];

    /// Minimal NSF v1: two songs, NTSC, titled, tone-generating INIT.
    fn synth_nsf() -> Vec<u8> {
        let mut buf = vec![0u8; 0x80];
        buf[..5].copy_from_slice(b"NESM\x1a");
        buf[0x05] = 1;
        buf[0x06] = 2;
        buf[0x07] = 1;
        buf[0x08..0x0a].copy_from_slice(&0x8000u16.to_le_bytes());
        buf[0x0a..0x0c].copy_from_slice(&0x8000u16.to_le_bytes());
        buf[0x0c..0x0e].copy_from_slice(&0x8014u16.to_le_bytes()); // PLAY = final RTS
        buf[0x0e..0x12].copy_from_slice(b"Glue");
        buf[0x2e..0x34].copy_from_slice(b"Tester");
        buf[0x6e..0x70].copy_from_slice(&16666u16.to_le_bytes());
        buf.extend_from_slice(&PROGRAM);
        buf
    }

    /// The synth file as NSF2 with the given appended-metadata chunks
    /// (an `NEND` terminator is added).
    fn synth_nsf2_with_meta(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut buf = synth_nsf();
        buf[0x05] = 2;
        buf[0x7d] = PROGRAM.len() as u8;
        for (tag, body) in chunks {
            buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
            buf.extend_from_slice(*tag);
            buf.extend_from_slice(body);
        }
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(b"NEND");
        buf
    }

    /// The same file as NSF2 with appended `tlbl` metadata, so the
    /// demuxer's track_N metadata rows are exercised.
    fn synth_nsf2_with_labels() -> Vec<u8> {
        synth_nsf2_with_meta(&[(b"tlbl", b"Overworld\0Boss\0")])
    }

    fn registries() -> (CodecRegistry, ContainerRegistry) {
        let mut codecs = CodecRegistry::new();
        register_codecs(&mut codecs);
        let mut containers = ContainerRegistry::default();
        register_containers(&mut containers);
        (codecs, containers)
    }

    #[test]
    fn probe_scores_both_magics_and_rejects_everything_else() {
        for (buf, score) in [
            (&b"NESM\x1aXX"[..], 100u8),
            (&b"NSFE"[..], 100),
            (&b"NESM"[..], 0), // v1 magic truncated before $1A
            (&b"NSF"[..], 0),  // NSFe magic truncated
            (&b"nesm\x1a"[..], 0),
            (&[][..], 0),
            (&[0xFF; 16][..], 0),
        ] {
            let p = ProbeData { buf, ext: None };
            assert_eq!(probe(&p), score, "probe({buf:02X?})");
        }
    }

    #[test]
    fn container_registry_probes_and_maps_extensions() {
        let (_, containers) = registries();
        assert_eq!(containers.container_for_extension("nsf"), Some("nsf"));
        assert_eq!(containers.container_for_extension("nsfe"), Some("nsf"));

        let mut input = Cursor::new(synth_nsf());
        assert_eq!(containers.probe_input(&mut input, None).unwrap(), "nsf");
        // The probe must restore the cursor for the demuxer open.
        assert_eq!(input.position(), 0);
    }

    #[test]
    fn demuxer_contract_stream_metadata_and_single_packet() {
        let (codecs, containers) = registries();
        let blob = synth_nsf2_with_labels();
        let mut demux = containers
            .open_demuxer("nsf", Box::new(Cursor::new(blob.clone())), &codecs)
            .unwrap();

        assert_eq!(demux.format_name(), "nsf");
        let streams = demux.streams();
        assert_eq!(streams.len(), 1);
        let params = &streams[0].params;
        assert_eq!(params.codec_id, CodecId::new(CODEC_ID_STR));
        assert_eq!(params.channels, Some(1));
        assert_eq!(params.sample_rate, Some(OUTPUT_SAMPLE_RATE));
        assert_eq!(params.sample_format, Some(SampleFormat::S16));
        assert_eq!(params.extradata, blob);

        let meta = demux.metadata().to_vec();
        let get = |k: &str| {
            meta.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("title"), Some("Glue"));
        assert_eq!(get("artist"), Some("Tester"));
        assert_eq!(get("track_0"), Some("Overworld"));
        assert_eq!(get("track_1"), Some("Boss"));
        assert!(get("extra_info").unwrap().contains("2 song(s)"));

        // Exactly one whole-file packet, then EOF.
        let pkt = demux.next_packet().unwrap();
        assert_eq!(pkt.data, blob);
        assert_eq!(pkt.pts, Some(0));
        assert!(pkt.flags.keyframe);
        assert!(matches!(demux.next_packet(), Err(Error::Eof)));
    }

    #[test]
    fn demuxer_open_rejects_hostile_input() {
        let (codecs, containers) = registries();
        for bad in [&[][..], &[0x00; 64][..], &b"NESM\x1a"[..]] {
            assert!(
                containers
                    .open_demuxer("nsf", Box::new(Cursor::new(bad.to_vec())), &codecs)
                    .is_err(),
                "hostile input {bad:02X?} must not open"
            );
        }
    }

    #[test]
    fn decoder_contract_frames_pts_and_state_machine() {
        let (codecs, _) = registries();
        let params = CodecParameters::audio(CodecId::new(CODEC_ID_STR));
        let mut dec = codecs.first_decoder(&params).unwrap();
        assert_eq!(dec.codec_id().as_str(), CODEC_ID_STR);

        // Before any packet: NeedMore, not a panic or EOF.
        assert!(matches!(dec.receive_frame(), Err(Error::NeedMore)));

        let blob = synth_nsf();
        let tb = TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64);
        let pkt = Packet::new(0, tb, blob.clone());
        dec.send_packet(&pkt).unwrap();

        // One packet per song: a second send must be refused.
        assert!(dec.send_packet(&pkt).is_err());

        // Frames flow with a monotonically advancing sample-count pts.
        let mut expected_pts = 0i64;
        for _ in 0..4 {
            let Frame::Audio(frame) = dec.receive_frame().unwrap() else {
                panic!("NSF decoder must produce audio frames");
            };
            assert!(frame.samples > 0);
            assert_eq!(frame.pts, Some(expected_pts));
            assert_eq!(frame.data[0].len(), frame.samples as usize * 2);
            expected_pts += frame.samples as i64;
        }

        // reset() returns to the awaiting state and accepts a new packet.
        dec.reset().unwrap();
        assert!(matches!(dec.receive_frame(), Err(Error::NeedMore)));
        dec.send_packet(&pkt).unwrap();
        let Frame::Audio(frame) = dec.receive_frame().unwrap() else {
            panic!("post-reset decode must produce audio frames");
        };
        assert_eq!(frame.pts, Some(0));
    }

    #[test]
    fn decoder_rejects_hostile_packet_payloads() {
        let (codecs, _) = registries();
        let params = CodecParameters::audio(CodecId::new(CODEC_ID_STR));
        let tb = TimeBase::new(1, OUTPUT_SAMPLE_RATE as i64);
        for bad in [&[][..], &[0xAA; 32][..], &b"NSFE"[..]] {
            let mut dec = codecs.first_decoder(&params).unwrap();
            assert!(
                dec.send_packet(&Packet::new(0, tb, bad.to_vec())).is_err(),
                "hostile packet {bad:02X?} must be refused"
            );
        }
    }

    #[test]
    fn demuxer_surfaces_scheduled_starting_track_duration() {
        let (codecs, containers) = registries();

        // No time/fade metadata: the common looping rip declares no
        // end.
        let demux = containers
            .open_demuxer("nsf", Box::new(Cursor::new(synth_nsf())), &codecs)
            .unwrap();
        assert_eq!(demux.duration_micros(), None);
        assert_eq!(demux.streams()[0].duration, None);

        // time = 120 s + fade = 3 s for the starting track (song 1 of
        // 2): 123 s total, in µs and in 1/44100 stream ticks.
        let time: Vec<u8> = [120_000i32, 45_000]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let fade: Vec<u8> = [3_000i32, -1]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let blob = synth_nsf2_with_meta(&[(b"time", &time), (b"fade", &fade)]);
        let demux = containers
            .open_demuxer("nsf", Box::new(Cursor::new(blob.clone())), &codecs)
            .unwrap();
        assert_eq!(demux.duration_micros(), Some(123_000_000));
        assert_eq!(
            demux.streams()[0].duration,
            Some(123 * OUTPUT_SAMPLE_RATE as i64)
        );

        // The starting-song byte selects which entry counts: song 2
        // has a negative fade entry (player default = 0), so its
        // duration is the bare 45 s time.
        let mut blob2 = blob;
        blob2[0x07] = 2;
        let demux = containers
            .open_demuxer("nsf", Box::new(Cursor::new(blob2)), &codecs)
            .unwrap();
        assert_eq!(demux.duration_micros(), Some(45_000_000));

        // A negative time entry is the chunk's own "player default"
        // marker — the container declares nothing.
        let neg: Vec<u8> = (-1i32).to_le_bytes().to_vec();
        let blob = synth_nsf2_with_meta(&[(b"time", &neg)]);
        let demux = containers
            .open_demuxer("nsf", Box::new(Cursor::new(blob)), &codecs)
            .unwrap();
        assert_eq!(demux.duration_micros(), None);
    }

    #[test]
    fn end_to_end_probe_demux_decode_produces_pcm() {
        let (codecs, containers) = registries();
        let blob = synth_nsf();
        let mut input = Cursor::new(blob);
        let name = containers.probe_input(&mut input, Some("nsf")).unwrap();
        let mut demux = containers
            .open_demuxer(&name, Box::new(input), &codecs)
            .unwrap();
        let pkt = demux.next_packet().unwrap();

        let mut dec = codecs.first_decoder(&demux.streams()[0].params).unwrap();
        dec.send_packet(&pkt).unwrap();
        let mut nonzero = false;
        for _ in 0..8 {
            let Frame::Audio(frame) = dec.receive_frame().unwrap() else {
                panic!("expected audio frames");
            };
            nonzero |= frame.data[0].iter().any(|&b| b != 0);
        }
        assert!(nonzero, "the synth NSF must render non-silent PCM");
    }
}
