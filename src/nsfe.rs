//! NSFe extended-chunk metadata.
//!
//! The NSFe container is a series of `[u32 size][FOURCC tag][size bytes
//! payload]` chunks. The chunk catalogue is documented in
//! `docs/audio/nsf/nsfe-nesdev-wiki.html`. NSF2 reuses the same chunk
//! catalogue for the appended-metadata blob (a chunk run with no `NSFE`
//! magic, no `INFO`/`DATA`/`BANK`/`NSF2` chunks, and otherwise identical
//! semantics).
//!
//! Header-style chunks (`INFO`, `DATA`, `BANK`, `NSF2`) are consumed by
//! the `header` module; the *extended* metadata chunks (`auth`, `tlbl`,
//! `taut`, `text`, `time`, `fade`, `plst`, `psfx`, `mixe`, `regn`,
//! `VRC7`, `RATE`) are parsed here into [`NsfeMetadata`].
//!
//! Mandatory-vs-optional rule: a chunk whose first FOURCC byte is an
//! ASCII uppercase letter is mandatory — a parser that doesn't
//! recognise it must reject the file. Lowercase-initial chunks are
//! optional. The four uppercase-initial chunks reserved to the NSFe
//! header (`INFO`, `DATA`, `BANK`, `NSF2`, `RATE`, `VRC7`) are handled
//! separately; any other uppercase-initial chunk is unknown and is
//! rejected here.

use core::fmt;

/// Authorship strings carried by the `auth` chunk. Each field is a
/// null-terminated string. Missing trailing strings are surfaced as
/// empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NsfeAuth {
    pub title: String,
    pub artist: String,
    pub copyright: String,
    pub ripper: String,
}

/// One mixer override entry from the `mixe` chunk: a device id paired
/// with a signed millibel (1/100 dB) offset relative to the APU square
/// reference level. Device-id meanings are documented on the nesdev
/// wiki under §mixe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NsfeMixerEntry {
    pub device: u8,
    pub millibel: i16,
}

/// Decoded `regn` chunk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NsfeRegions {
    /// Bitfield from byte 0: bit0=NTSC, bit1=PAL, bit2=Dendy.
    pub mask: u8,
    /// Optional preferred region from byte 1: 0=NTSC, 1=PAL, 2=Dendy.
    pub preferred: Option<u8>,
}

impl NsfeRegions {
    pub fn supports_ntsc(self) -> bool {
        self.mask & 0x01 != 0
    }
    pub fn supports_pal(self) -> bool {
        self.mask & 0x02 != 0
    }
    pub fn supports_dendy(self) -> bool {
        self.mask & 0x04 != 0
    }
}

/// Decoded `RATE` chunk — one to three little-endian `u16` play
/// periods in microseconds. Any sub-field unset by a too-short chunk
/// is surfaced as `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NsfeRate {
    pub ntsc_us: Option<u16>,
    pub pal_us: Option<u16>,
    pub dendy_us: Option<u16>,
}

/// Decoded `VRC7` chunk — device selector plus an optional replacement
/// patch table. The patch table is preserved verbatim (128 or 152
/// bytes per spec); decoding the patches into operator parameters is
/// blocked on the OPLL operator-internals docs gap (task #861).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NsfeVrc7 {
    /// 0 = VRC7 native, 1 = YM2413.
    pub device: u8,
    /// Optional 128- or 152-byte patch table.
    pub patches: Option<Vec<u8>>,
}

/// Aggregated extended-metadata chunks. Every field defaults to "not
/// present"; callers check `is_some` / `is_empty` / per-field defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NsfeMetadata {
    pub auth: Option<NsfeAuth>,
    pub track_labels: Vec<String>,
    pub track_authors: Vec<String>,
    pub text: Option<String>,
    pub track_times_ms: Vec<i32>,
    pub track_fades_ms: Vec<i32>,
    pub playlist: Vec<u8>,
    pub sfx_playlist: Vec<u8>,
    pub mixer: Vec<NsfeMixerEntry>,
    pub regions: Option<NsfeRegions>,
    pub rate: Option<NsfeRate>,
    pub vrc7: Option<NsfeVrc7>,
}

impl NsfeMetadata {
    /// `true` iff every field is at its default. Useful for callers
    /// that want to short-circuit construction.
    pub fn is_empty(&self) -> bool {
        self.auth.is_none()
            && self.track_labels.is_empty()
            && self.track_authors.is_empty()
            && self.text.is_none()
            && self.track_times_ms.is_empty()
            && self.track_fades_ms.is_empty()
            && self.playlist.is_empty()
            && self.sfx_playlist.is_empty()
            && self.mixer.is_empty()
            && self.regions.is_none()
            && self.rate.is_none()
            && self.vrc7.is_none()
    }
}

/// Errors specific to the extended-chunk parser.
#[derive(Debug, PartialEq, Eq)]
pub enum NsfeMetaError {
    TruncatedChunk,
    ChunkOverflow,
    UnknownMandatory([u8; 4]),
    /// A chunk's payload was a length the spec rejects (e.g. a `mixe`
    /// chunk whose length isn't a multiple of 3, or a `VRC7` chunk
    /// with a patch-table length other than 128 or 152).
    BadChunkPayload {
        tag: [u8; 4],
        len: usize,
    },
}

impl fmt::Display for NsfeMetaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NsfeMetaError::TruncatedChunk => f.write_str("NSFe metadata: chunk header truncated"),
            NsfeMetaError::ChunkOverflow => f.write_str("NSFe metadata: chunk size overflows blob"),
            NsfeMetaError::UnknownMandatory(fcc) => write!(
                f,
                "NSFe metadata: unknown mandatory chunk {:?}",
                core::str::from_utf8(fcc).unwrap_or("????")
            ),
            NsfeMetaError::BadChunkPayload { tag, len } => write!(
                f,
                "NSFe metadata: chunk {:?} has invalid payload length {len}",
                core::str::from_utf8(tag).unwrap_or("????")
            ),
        }
    }
}

impl std::error::Error for NsfeMetaError {}

/// Drives the chunk walker over a metadata blob. The blob is a bare
/// chunk run with NO `NSFE` magic — the NSFe file parser strips the
/// magic before calling in here, and the NSF2 file parser hands the
/// appended bytes straight through.
///
/// `acceptable` controls which header-style chunks the caller wants
/// surfaced as unknowns vs silently swallowed. NSF2 metadata blobs
/// per spec **forbid** `INFO`/`DATA`/`BANK`/`NSF2`; the NSFe header
/// parser pre-consumes those four chunks before calling here so they
/// shouldn't appear either way.
pub fn parse_metadata_chunks(blob: &[u8]) -> Result<NsfeMetadata, NsfeMetaError> {
    let mut out = NsfeMetadata::default();
    let mut cursor = 0usize;
    while cursor < blob.len() {
        if blob.len() - cursor < 8 {
            return Err(NsfeMetaError::TruncatedChunk);
        }
        let size = u32::from_le_bytes([
            blob[cursor],
            blob[cursor + 1],
            blob[cursor + 2],
            blob[cursor + 3],
        ]) as usize;
        let mut tag = [0u8; 4];
        tag.copy_from_slice(&blob[cursor + 4..cursor + 8]);
        let body_start = cursor + 8;
        let body_end = body_start
            .checked_add(size)
            .ok_or(NsfeMetaError::ChunkOverflow)?;
        if body_end > blob.len() {
            return Err(NsfeMetaError::ChunkOverflow);
        }
        let body = &blob[body_start..body_end];

        match &tag {
            b"NEND" => return Ok(out),
            b"auth" => out.auth = Some(parse_auth(body)),
            b"tlbl" => out.track_labels = split_null_strings(body),
            b"taut" => out.track_authors = split_null_strings(body),
            b"text" => out.text = Some(parse_text(body)),
            b"time" => out.track_times_ms = parse_i32_array(body),
            b"fade" => out.track_fades_ms = parse_i32_array(body),
            b"plst" => out.playlist = body.to_vec(),
            b"psfx" => out.sfx_playlist = body.to_vec(),
            b"mixe" => out.mixer = parse_mixer(body, tag)?,
            b"regn" => out.regions = Some(parse_regions(body)),
            b"RATE" => out.rate = Some(parse_rate(body)),
            b"VRC7" => out.vrc7 = Some(parse_vrc7(body, tag)?),
            _ => {
                if tag[0].is_ascii_uppercase() {
                    return Err(NsfeMetaError::UnknownMandatory(tag));
                }
                // Unknown lowercase-initial chunk: per spec, skip.
            }
        }

        cursor = body_end;
    }
    Ok(out)
}

fn parse_auth(body: &[u8]) -> NsfeAuth {
    let mut it = body.split(|&b| b == 0);
    NsfeAuth {
        title: read_string(it.next().unwrap_or(&[])),
        artist: read_string(it.next().unwrap_or(&[])),
        copyright: read_string(it.next().unwrap_or(&[])),
        ripper: read_string(it.next().unwrap_or(&[])),
    }
}

fn parse_text(body: &[u8]) -> String {
    let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
    read_string(&body[..end])
}

fn split_null_strings(body: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut iter = body.split(|&b| b == 0).peekable();
    // The spec describes the chunk as a series of null-terminated
    // strings — a trailing NUL produces an empty tail entry that we
    // discard (`["Alpha", "Beta", ""]` → 2 tracks named Alpha/Beta).
    while let Some(s) = iter.next() {
        if iter.peek().is_none() && s.is_empty() {
            break;
        }
        out.push(read_string(s));
    }
    out
}

fn parse_i32_array(body: &[u8]) -> Vec<i32> {
    body.chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn parse_mixer(body: &[u8], tag: [u8; 4]) -> Result<Vec<NsfeMixerEntry>, NsfeMetaError> {
    if body.len() % 3 != 0 {
        return Err(NsfeMetaError::BadChunkPayload {
            tag,
            len: body.len(),
        });
    }
    Ok(body
        .chunks_exact(3)
        .map(|c| NsfeMixerEntry {
            device: c[0],
            millibel: i16::from_le_bytes([c[1], c[2]]),
        })
        .collect())
}

fn parse_regions(body: &[u8]) -> NsfeRegions {
    NsfeRegions {
        mask: body.first().copied().unwrap_or(0),
        preferred: body.get(1).copied(),
    }
}

fn parse_rate(body: &[u8]) -> NsfeRate {
    let read_u16 = |i: usize| -> Option<u16> {
        if body.len() >= i + 2 {
            Some(u16::from_le_bytes([body[i], body[i + 1]]))
        } else {
            None
        }
    };
    NsfeRate {
        ntsc_us: read_u16(0),
        pal_us: read_u16(2),
        dendy_us: read_u16(4),
    }
}

fn parse_vrc7(body: &[u8], tag: [u8; 4]) -> Result<NsfeVrc7, NsfeMetaError> {
    if body.is_empty() {
        return Err(NsfeMetaError::BadChunkPayload {
            tag,
            len: body.len(),
        });
    }
    let device = body[0];
    let patches = if body.len() == 1 {
        None
    } else {
        // The spec allows 128-byte (15 melodic patches) or 152-byte
        // (15 melodic + 3 rhythm) patch payloads after the device
        // byte. Anything else is malformed.
        let patch_len = body.len() - 1;
        if patch_len != 128 && patch_len != 152 {
            return Err(NsfeMetaError::BadChunkPayload {
                tag,
                len: body.len(),
            });
        }
        Some(body[1..].to_vec())
    };
    Ok(NsfeVrc7 { device, patches })
}

fn read_string(field: &[u8]) -> String {
    let mut last = field.len();
    while last > 0 && (field[last - 1] == b' ' || field[last - 1] == b'\t') {
        last -= 1;
    }
    field[..last].iter().map(|&b| b as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (tag, body) in chunks {
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(*tag);
            out.extend_from_slice(body);
        }
        out
    }

    #[test]
    fn parses_auth_with_all_four_strings() {
        let blob = pack(&[(b"auth", b"Title\0Artist\0Copy\0Ripper\0")]);
        let m = parse_metadata_chunks(&blob).unwrap();
        let auth = m.auth.unwrap();
        assert_eq!(auth.title, "Title");
        assert_eq!(auth.artist, "Artist");
        assert_eq!(auth.copyright, "Copy");
        assert_eq!(auth.ripper, "Ripper");
    }

    #[test]
    fn auth_tolerates_missing_trailing_strings() {
        let blob = pack(&[(b"auth", b"OnlyTitle\0")]);
        let auth = parse_metadata_chunks(&blob).unwrap().auth.unwrap();
        assert_eq!(auth.title, "OnlyTitle");
        assert_eq!(auth.artist, "");
        assert_eq!(auth.copyright, "");
        assert_eq!(auth.ripper, "");
    }

    #[test]
    fn parses_tlbl_and_taut_arrays() {
        let blob = pack(&[
            (b"tlbl", b"Alpha\0Beta\0Gamma\0"),
            (b"taut", b"Composer A\0Composer B\0"),
        ]);
        let m = parse_metadata_chunks(&blob).unwrap();
        assert_eq!(m.track_labels, vec!["Alpha", "Beta", "Gamma"]);
        assert_eq!(m.track_authors, vec!["Composer A", "Composer B"]);
    }

    #[test]
    fn parses_text_chunk() {
        let blob = pack(&[(b"text", b"Hello\r\nFrom\nthe ripper\0")]);
        let m = parse_metadata_chunks(&blob).unwrap();
        assert_eq!(m.text.as_deref(), Some("Hello\r\nFrom\nthe ripper"));
    }

    #[test]
    fn parses_time_and_fade_as_signed_ints() {
        // Three tracks: 120 000 ms, 0 (immediate fade), default (-1).
        let times = [120_000i32, 0, -1];
        let fades = [3_000i32, -1, -1];
        let mut tb = Vec::new();
        let mut fb = Vec::new();
        for v in times {
            tb.extend_from_slice(&v.to_le_bytes());
        }
        for v in fades {
            fb.extend_from_slice(&v.to_le_bytes());
        }
        let blob = pack(&[(b"time", &tb), (b"fade", &fb)]);
        let m = parse_metadata_chunks(&blob).unwrap();
        assert_eq!(m.track_times_ms, times);
        assert_eq!(m.track_fades_ms, fades);
    }

    #[test]
    fn parses_plst_and_psfx() {
        let blob = pack(&[(b"plst", &[0, 2, 1, 3]), (b"psfx", &[5, 6])]);
        let m = parse_metadata_chunks(&blob).unwrap();
        assert_eq!(m.playlist, vec![0, 2, 1, 3]);
        assert_eq!(m.sfx_playlist, vec![5, 6]);
    }

    #[test]
    fn parses_mixe_entries_and_rejects_misaligned_payload() {
        // Two entries: APU squares (id 0, 0 mB), VRC7 (id 3, +1100 mB).
        let mut body = Vec::new();
        body.push(0u8);
        body.extend_from_slice(&0i16.to_le_bytes());
        body.push(3u8);
        body.extend_from_slice(&1100i16.to_le_bytes());
        let blob = pack(&[(b"mixe", &body)]);
        let m = parse_metadata_chunks(&blob).unwrap();
        assert_eq!(
            m.mixer,
            vec![
                NsfeMixerEntry {
                    device: 0,
                    millibel: 0
                },
                NsfeMixerEntry {
                    device: 3,
                    millibel: 1100
                },
            ]
        );

        let bad = pack(&[(b"mixe", b"\x00\x00")]); // length 2, not multiple of 3
        assert!(matches!(
            parse_metadata_chunks(&bad).unwrap_err(),
            NsfeMetaError::BadChunkPayload { .. }
        ));
    }

    #[test]
    fn parses_regn_with_and_without_preferred() {
        // Mask = NTSC|PAL|Dendy, preferred = Dendy.
        let blob = pack(&[(b"regn", &[0x07, 0x02])]);
        let r = parse_metadata_chunks(&blob).unwrap().regions.unwrap();
        assert!(r.supports_ntsc() && r.supports_pal() && r.supports_dendy());
        assert_eq!(r.preferred, Some(2));

        // Mask only, no preferred.
        let blob = pack(&[(b"regn", &[0x01])]);
        let r = parse_metadata_chunks(&blob).unwrap().regions.unwrap();
        assert!(r.supports_ntsc() && !r.supports_pal() && !r.supports_dendy());
        assert_eq!(r.preferred, None);
    }

    #[test]
    fn parses_rate_with_optional_pal_and_dendy() {
        // All three populated.
        let mut body = Vec::new();
        body.extend_from_slice(&16639u16.to_le_bytes()); // NTSC
        body.extend_from_slice(&19997u16.to_le_bytes()); // PAL
        body.extend_from_slice(&19120u16.to_le_bytes()); // Dendy
        let blob = pack(&[(b"RATE", &body)]);
        let r = parse_metadata_chunks(&blob).unwrap().rate.unwrap();
        assert_eq!(r.ntsc_us, Some(16639));
        assert_eq!(r.pal_us, Some(19997));
        assert_eq!(r.dendy_us, Some(19120));

        // NTSC only.
        let blob = pack(&[(b"RATE", &16639u16.to_le_bytes())]);
        let r = parse_metadata_chunks(&blob).unwrap().rate.unwrap();
        assert_eq!(r.ntsc_us, Some(16639));
        assert_eq!(r.pal_us, None);
        assert_eq!(r.dendy_us, None);
    }

    #[test]
    fn parses_vrc7_device_only_and_with_patch_table() {
        // Device byte only.
        let blob = pack(&[(b"VRC7", &[0u8])]);
        let v = parse_metadata_chunks(&blob).unwrap().vrc7.unwrap();
        assert_eq!(v.device, 0);
        assert!(v.patches.is_none());

        // YM2413 + 128-byte custom patch table.
        let mut body = vec![1u8];
        body.extend(std::iter::repeat_n(0xABu8, 128));
        let blob = pack(&[(b"VRC7", &body)]);
        let v = parse_metadata_chunks(&blob).unwrap().vrc7.unwrap();
        assert_eq!(v.device, 1);
        assert_eq!(v.patches.as_ref().map(|p| p.len()), Some(128));

        // 152-byte custom patch table (with rhythm).
        let mut body = vec![0u8];
        body.extend(std::iter::repeat_n(0x55u8, 152));
        let blob = pack(&[(b"VRC7", &body)]);
        let v = parse_metadata_chunks(&blob).unwrap().vrc7.unwrap();
        assert_eq!(v.patches.as_ref().map(|p| p.len()), Some(152));

        // Patch table of an illegal length (between 128 and 152, here 130).
        let mut body = vec![0u8];
        body.extend(std::iter::repeat_n(0u8, 130));
        let blob = pack(&[(b"VRC7", &body)]);
        assert!(matches!(
            parse_metadata_chunks(&blob).unwrap_err(),
            NsfeMetaError::BadChunkPayload { .. }
        ));
    }

    #[test]
    fn rejects_unknown_uppercase_chunk() {
        let blob = pack(&[(b"ZZZZ", b"")]);
        assert!(matches!(
            parse_metadata_chunks(&blob).unwrap_err(),
            NsfeMetaError::UnknownMandatory(_)
        ));
    }

    #[test]
    fn skips_unknown_lowercase_chunk() {
        let blob = pack(&[(b"xxxx", b"junk bytes"), (b"auth", b"Hi\0There\0\0\0")]);
        let m = parse_metadata_chunks(&blob).unwrap();
        assert_eq!(m.auth.unwrap().title, "Hi");
    }

    #[test]
    fn nend_stops_parsing() {
        let blob = pack(&[
            (b"auth", b"Title\0\0\0\0"),
            (b"NEND", b""),
            (b"text", b"This should be ignored\0"),
        ]);
        let m = parse_metadata_chunks(&blob).unwrap();
        assert!(m.auth.is_some());
        assert!(m.text.is_none());
    }

    #[test]
    fn truncated_chunk_header_is_an_error() {
        let blob = vec![0x00u8, 0x00, 0x00]; // < 8 bytes
        assert_eq!(
            parse_metadata_chunks(&blob),
            Err(NsfeMetaError::TruncatedChunk)
        );
    }

    #[test]
    fn declared_chunk_size_overrun_is_an_error() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&100u32.to_le_bytes());
        blob.extend_from_slice(b"text");
        blob.extend_from_slice(b"short"); // only 5 bytes follow
        assert_eq!(
            parse_metadata_chunks(&blob),
            Err(NsfeMetaError::ChunkOverflow)
        );
    }
}
