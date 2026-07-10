//! NSF / NSF2 / NSFe container writers.
//!
//! Serializes an [`NsfHeader`] back into either on-disk shape:
//!
//! * [`NsfHeader::write_nsf`] — the classic fixed 128-byte `NESM\x1a`
//!   header (v1 or v2) followed by the program blob, with NSFe-style
//!   metadata appended after the program when the header carries any
//!   (per `docs/audio/nsf/nsf2-nesdev-wiki.html` §Metadata; legal even
//!   for version-1 files, whose players treat the tail as ROM).
//! * [`NsfHeader::write_nsfe`] — the chunk-based `NSFE` container per
//!   `docs/audio/nsf/nsf-container-layout.md` /
//!   `docs/audio/nsf/nsfe-nesdev-wiki.html`: `INFO` first, `DATA`
//!   after it, `NEND` last.
//!
//! The writers emit a canonical encoding (fixed chunk order, minimal
//! `RATE`/`regn` lengths, strings as UTF-8), so `write ∘ parse ∘
//! write` is byte-idempotent, and `parse ∘ write` preserves every
//! semantic field.

use core::fmt;

use crate::header::{NsfHeader, NsfRegion, NSFE_MAGIC, NSF_HEADER_LEN, NSF_MAGIC};
use crate::nsfe::{NsfeAuth, NsfeMetadata, NsfeRate, NsfeRegions};

/// Failures from the container writers.
#[derive(Debug, PartialEq, Eq)]
pub enum NsfWriteError {
    /// `total_songs` is zero — both container shapes require at least
    /// one song.
    NoSongs,
    /// The fixed-header shape only exists in versions 1 and 2.
    BadVersion(u8),
    /// NSF2 feature flags are set but the header declares version 1;
    /// per spec byte `$7C` MUST be ignored on v1 files, so writing it
    /// would produce a file that silently loses the features.
    FeaturesRequireV2,
    /// A v1 fixed-header string field exceeds its 31-byte limit
    /// ("length limited to 31 characters" + terminating NUL in the
    /// 32-byte field).
    StringTooLong { field: &'static str, len: usize },
    /// A string destined for a NUL-terminated field/chunk contains an
    /// interior NUL, which would silently truncate it on re-parse.
    StringContainsNul { field: &'static str },
    /// The program blob exceeds the NSF2 24-bit data-length field, so
    /// appended metadata cannot be delimited.
    ProgramTooLong { len: usize },
    /// A `VRC7` replacement patch set must be exactly 128 or 152
    /// bytes.
    BadVrc7PatchLen { len: usize },
    /// A chunk body exceeds the 32-bit chunk-length field.
    ChunkTooLong { tag: [u8; 4] },
}

impl fmt::Display for NsfWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NsfWriteError::NoSongs => f.write_str("NSF write: total_songs is zero"),
            NsfWriteError::BadVersion(v) => {
                write!(f, "NSF write: fixed-header version must be 1 or 2, got {v}")
            }
            NsfWriteError::FeaturesRequireV2 => {
                f.write_str("NSF write: NSF2 feature flags set on a version-1 header")
            }
            NsfWriteError::StringTooLong { field, len } => {
                write!(f, "NSF write: {field} is {len} bytes (31-byte limit)")
            }
            NsfWriteError::StringContainsNul { field } => {
                write!(f, "NSF write: {field} contains an interior NUL")
            }
            NsfWriteError::ProgramTooLong { len } => write!(
                f,
                "NSF write: program is {len} bytes, over the 24-bit data-length limit"
            ),
            NsfWriteError::BadVrc7PatchLen { len } => write!(
                f,
                "NSF write: VRC7 patch set must be 128 or 152 bytes, got {len}"
            ),
            NsfWriteError::ChunkTooLong { tag } => write!(
                f,
                "NSF write: chunk {:?} body exceeds the 32-bit length field",
                core::str::from_utf8(tag).unwrap_or("????")
            ),
        }
    }
}

impl std::error::Error for NsfWriteError {}

/// Emit one `[u32 length][fourcc][body]` chunk.
fn put_chunk(out: &mut Vec<u8>, tag: &[u8; 4], body: &[u8]) -> Result<(), NsfWriteError> {
    let len = u32::try_from(body.len()).map_err(|_| NsfWriteError::ChunkTooLong { tag: *tag })?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(body);
    Ok(())
}

/// Append `s` as UTF-8 + terminating NUL, rejecting interior NULs.
fn put_cstr(out: &mut Vec<u8>, s: &str, field: &'static str) -> Result<(), NsfWriteError> {
    if s.as_bytes().contains(&0) {
        return Err(NsfWriteError::StringContainsNul { field });
    }
    out.extend_from_slice(s.as_bytes());
    out.push(0);
    Ok(())
}

/// Serialize the extended-metadata chunk set (everything except the
/// header-layer `INFO` / `DATA` / `BANK` / `NSF2` / `NEND` chunks) in
/// a fixed canonical order. `track_labels` supplies the `tlbl` chunk
/// (the parser hoists it out of [`NsfeMetadata`] into
/// [`NsfHeader::track_labels`]).
///
/// The output is a bare chunk run with no magic and no terminator —
/// exactly the NSF2 appended-metadata shape (both `RATE` and `regn`
/// are legal there per `docs/audio/nsf/nsf2-nesdev-wiki.html`
/// §Metadata); [`NsfHeader::write_nsfe`] embeds the same run in the
/// chunk stream.
pub fn write_metadata_chunks(
    meta: &NsfeMetadata,
    track_labels: &[String],
) -> Result<Vec<u8>, NsfWriteError> {
    let mut out = Vec::new();

    if let Some(rate) = &meta.rate {
        // 4-byte minimum (NTSC + PAL words), 6 bytes with the
        // optional Dendy word.
        let mut body = Vec::with_capacity(6);
        body.extend_from_slice(&rate.ntsc_us.unwrap_or(0).to_le_bytes());
        body.extend_from_slice(&rate.pal_us.unwrap_or(0).to_le_bytes());
        if let Some(dendy) = rate.dendy_us {
            body.extend_from_slice(&dendy.to_le_bytes());
        }
        put_chunk(&mut out, b"RATE", &body)?;
    }

    if let Some(vrc7) = &meta.vrc7 {
        let mut body = vec![vrc7.device];
        if let Some(patches) = &vrc7.patches {
            if patches.len() != 128 && patches.len() != 152 {
                return Err(NsfWriteError::BadVrc7PatchLen { len: patches.len() });
            }
            body.extend_from_slice(patches);
        }
        put_chunk(&mut out, b"VRC7", &body)?;
    }

    if let Some(regn) = &meta.regions {
        let mut body = vec![regn.mask];
        if let Some(pref) = regn.preferred {
            body.push(pref);
        }
        put_chunk(&mut out, b"regn", &body)?;
    }

    if !meta.playlist.is_empty() {
        put_chunk(&mut out, b"plst", &meta.playlist)?;
    }
    if !meta.sfx_playlist.is_empty() {
        put_chunk(&mut out, b"psfx", &meta.sfx_playlist)?;
    }

    if !meta.track_times_ms.is_empty() {
        let mut body = Vec::with_capacity(meta.track_times_ms.len() * 4);
        for v in &meta.track_times_ms {
            body.extend_from_slice(&v.to_le_bytes());
        }
        put_chunk(&mut out, b"time", &body)?;
    }
    if !meta.track_fades_ms.is_empty() {
        let mut body = Vec::with_capacity(meta.track_fades_ms.len() * 4);
        for v in &meta.track_fades_ms {
            body.extend_from_slice(&v.to_le_bytes());
        }
        put_chunk(&mut out, b"fade", &body)?;
    }

    if !track_labels.is_empty() {
        let mut body = Vec::new();
        for label in track_labels {
            put_cstr(&mut body, label, "tlbl label")?;
        }
        put_chunk(&mut out, b"tlbl", &body)?;
    }
    if !meta.track_authors.is_empty() {
        let mut body = Vec::new();
        for author in &meta.track_authors {
            put_cstr(&mut body, author, "taut author")?;
        }
        put_chunk(&mut out, b"taut", &body)?;
    }

    if let Some(auth) = &meta.auth {
        let mut body = Vec::new();
        put_cstr(&mut body, &auth.title, "auth title")?;
        put_cstr(&mut body, &auth.artist, "auth artist")?;
        put_cstr(&mut body, &auth.copyright, "auth copyright")?;
        put_cstr(&mut body, &auth.ripper, "auth ripper")?;
        put_chunk(&mut out, b"auth", &body)?;
    }

    if let Some(text) = &meta.text {
        let mut body = Vec::new();
        put_cstr(&mut body, text, "text")?;
        put_chunk(&mut out, b"text", &body)?;
    }

    if !meta.mixer.is_empty() {
        let mut body = Vec::with_capacity(meta.mixer.len() * 3);
        for entry in &meta.mixer {
            body.push(entry.device);
            body.extend_from_slice(&entry.millibel.to_le_bytes());
        }
        put_chunk(&mut out, b"mixe", &body)?;
    }

    Ok(out)
}

/// Encode a region into the v1 header byte `$7A` / NSFe `INFO` byte 6
/// (bit 0 = PAL, bit 1 = dual). Dendy has no encoding in this byte —
/// it is exclusively surfaced through the `regn` chunk — so a Dendy
/// header writes the PAL bit (the documented fallback rate for Dendy
/// playback) and relies on the synthesized `regn` chunk (see
/// [`effective_metadata`]) to restore the region on re-parse.
fn region_byte(region: NsfRegion) -> u8 {
    match region {
        NsfRegion::Ntsc => 0x00,
        NsfRegion::Pal | NsfRegion::Dendy => 0x01,
        NsfRegion::Dual => 0x02,
    }
}

/// Clone the header's metadata, synthesizing whatever chunks are
/// needed so that re-parsing reproduces the header's semantic state:
///
/// * a `regn` chunk with `preferred = 2` when the region is Dendy
///   (the fixed header/`INFO` region byte cannot express it);
/// * for the NSFe shape only (`synthesize_rate_auth`), a `RATE` chunk
///   when the header carries explicit play periods but no `RATE`
///   (NSFe has no other place for them, unlike the fixed header's
///   `$6E`/`$78` words), and an `auth` chunk when the header carries
///   title/artist/copyright strings but no `auth`.
fn effective_metadata(header: &NsfHeader, synthesize_rate_auth: bool) -> NsfeMetadata {
    let mut meta = header.metadata.clone();

    if header.region == NsfRegion::Dendy {
        let mut regions = meta.regions.unwrap_or(NsfeRegions {
            mask: 0x04,
            preferred: None,
        });
        regions.mask |= 0x04;
        regions.preferred = Some(2);
        meta.regions = Some(regions);
    }

    if synthesize_rate_auth {
        if meta.rate.is_none() && (header.ntsc_speed_us != 0 || header.pal_speed_us != 0) {
            meta.rate = Some(NsfeRate {
                ntsc_us: Some(header.ntsc_speed_us),
                pal_us: Some(header.pal_speed_us),
                dendy_us: None,
            });
        }
        if meta.auth.is_none()
            && !(header.song_name.is_empty()
                && header.artist.is_empty()
                && header.copyright.is_empty())
        {
            meta.auth = Some(NsfeAuth {
                title: header.song_name.clone(),
                artist: header.artist.clone(),
                copyright: header.copyright.clone(),
                ripper: String::new(),
            });
        }
    }

    meta
}

/// Write a v1 fixed-header string field: UTF-8 bytes, terminating
/// NUL, zero padding to 32 bytes. "The size of the string must be 31
/// characters or less! There has to be at least a single NULL byte"
/// (`docs/audio/nsf/nsfspec-kevtris-v1.61.txt`).
fn put_fixed_string(slot: &mut [u8], s: &str, field: &'static str) -> Result<(), NsfWriteError> {
    debug_assert_eq!(slot.len(), 32);
    let bytes = s.as_bytes();
    if bytes.contains(&0) {
        return Err(NsfWriteError::StringContainsNul { field });
    }
    if bytes.len() > 31 {
        return Err(NsfWriteError::StringTooLong {
            field,
            len: bytes.len(),
        });
    }
    slot[..bytes.len()].copy_from_slice(bytes);
    // Remainder is already zero (NUL terminator + padding).
    Ok(())
}

impl NsfHeader {
    /// Serialize into the classic `NESM\x1a` fixed-header shape
    /// (128-byte header + program blob). When the header carries any
    /// extended metadata (track labels, per-track times, `auth`
    /// overrides, …) it is appended after the program as an
    /// NSFe-style chunk run terminated by `NEND`, with the 24-bit
    /// data length at `$7D-$7F` delimiting the program — the NSF2
    /// appended-metadata mechanism, which version-1 players safely
    /// treat as trailing ROM.
    ///
    /// The starting-song byte is written 1-based via
    /// [`NsfHeader::starting_song_number`], so headers parsed from
    /// either container shape serialize correctly.
    pub fn write_nsf(&self) -> Result<Vec<u8>, NsfWriteError> {
        if self.total_songs == 0 {
            return Err(NsfWriteError::NoSongs);
        }
        if !(1..=2).contains(&self.version) {
            return Err(NsfWriteError::BadVersion(self.version));
        }
        if self.version < 2 && self.nsf2.0 != 0 {
            return Err(NsfWriteError::FeaturesRequireV2);
        }

        let mut buf = vec![0u8; NSF_HEADER_LEN];
        buf[..5].copy_from_slice(&NSF_MAGIC);
        buf[0x05] = self.version;
        buf[0x06] = self.total_songs;
        buf[0x07] = self.starting_song_number();
        buf[0x08..0x0a].copy_from_slice(&self.load_addr.to_le_bytes());
        buf[0x0a..0x0c].copy_from_slice(&self.init_addr.to_le_bytes());
        buf[0x0c..0x0e].copy_from_slice(&self.play_addr.to_le_bytes());
        put_fixed_string(&mut buf[0x0e..0x2e], &self.song_name, "song name")?;
        put_fixed_string(&mut buf[0x2e..0x4e], &self.artist, "artist")?;
        put_fixed_string(&mut buf[0x4e..0x6e], &self.copyright, "copyright")?;
        buf[0x6e..0x70].copy_from_slice(&self.ntsc_speed_us.to_le_bytes());
        buf[0x70..0x78].copy_from_slice(&self.bankswitch_init);
        buf[0x78..0x7a].copy_from_slice(&self.pal_speed_us.to_le_bytes());
        buf[0x7a] = region_byte(self.region);
        buf[0x7b] = self.expansion.0;
        if self.version >= 2 {
            buf[0x7c] = self.nsf2.0;
        }

        // The fixed header itself carries the strings, banks and play
        // periods, so no RATE/auth synthesis — only what the header
        // cannot express (Dendy regn + genuine extended chunks).
        let meta = effective_metadata(self, false);
        let mut meta_blob = write_metadata_chunks(&meta, &self.track_labels)?;
        if !meta_blob.is_empty() {
            // "Metadata should end with an NEND chunk"
            // (docs/audio/nsf/nsf2-nesdev-wiki.html §Metadata).
            let mut terminated = meta_blob;
            put_chunk(&mut terminated, b"NEND", &[])?;
            meta_blob = terminated;

            if self.program.len() > 0xFF_FFFF {
                return Err(NsfWriteError::ProgramTooLong {
                    len: self.program.len(),
                });
            }
            let len = self.program.len();
            buf[0x7d] = (len & 0xff) as u8;
            buf[0x7e] = ((len >> 8) & 0xff) as u8;
            buf[0x7f] = ((len >> 16) & 0xff) as u8;
        }

        buf.extend_from_slice(&self.program);
        buf.extend_from_slice(&meta_blob);
        Ok(buf)
    }

    /// Serialize into the chunk-based `NSFE` container shape: `INFO`
    /// first, then `BANK` / `NSF2` when non-default, the extended
    /// metadata chunk run, `DATA`, and the required `NEND`
    /// terminator.
    ///
    /// Because NSFe has no fixed header, play periods and the
    /// title/artist/copyright strings are re-homed into synthesized
    /// `RATE` / `auth` chunks when the header carries them without
    /// explicit chunk equivalents — so converting a parsed v1 file
    /// (`parse_nsf` → `write_nsfe`) loses nothing. The starting-song
    /// byte is written 0-based via
    /// [`NsfHeader::starting_song_index`] per the `INFO` layout.
    pub fn write_nsfe(&self) -> Result<Vec<u8>, NsfWriteError> {
        if self.total_songs == 0 {
            return Err(NsfWriteError::NoSongs);
        }

        let mut out = NSFE_MAGIC.to_vec();

        let mut info = [0u8; 10];
        info[0..2].copy_from_slice(&self.load_addr.to_le_bytes());
        info[2..4].copy_from_slice(&self.init_addr.to_le_bytes());
        info[4..6].copy_from_slice(&self.play_addr.to_le_bytes());
        info[6] = region_byte(self.region);
        info[7] = self.expansion.0;
        info[8] = self.total_songs;
        info[9] = self.starting_song_index();
        put_chunk(&mut out, b"INFO", &info)?;

        if self.bankswitch_init != [0u8; 8] {
            put_chunk(&mut out, b"BANK", &self.bankswitch_init)?;
        }
        if self.nsf2.0 != 0 {
            put_chunk(&mut out, b"NSF2", &[self.nsf2.0])?;
        }

        let meta = effective_metadata(self, true);
        let meta_blob = write_metadata_chunks(&meta, &self.track_labels)?;
        out.extend_from_slice(&meta_blob);

        put_chunk(&mut out, b"DATA", &self.program)?;
        put_chunk(&mut out, b"NEND", &[])?;
        Ok(out)
    }
}
