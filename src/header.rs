//! NSF / NSFe / NSF2 header parser.
//!
//! Three on-disk shapes are accepted:
//!
//! * **NSF v1.x** — magic `b"NESM\x1a"`, fixed 128-byte header, then the
//!   raw 6502 code blob loaded at `load_addr`.
//! * **NSF v2** — same 128-byte header, version byte `0x02`. Repurposes
//!   the previously-reserved byte `$7C` as a feature-flag bitfield
//!   (IRQ support, non-returning INIT, suppressed PLAY, mandatory
//!   metadata) and bytes `$7D-$7F` as a 24-bit little-endian length
//!   of the program data — when non-zero, NSFe-style metadata chunks
//!   are appended after the program (without the outer `NSFE` fourCC
//!   and with no `INFO` / `DATA` / `BANK` / `NSF2` chunks).
//! * **NSFe** — magic `b"NSFE"`, then a chain of 4-byte-length /
//!   4-byte-fourCC / payload chunks. `INFO` and `DATA` are required;
//!   `auth` (author / title / copyright / ripper), `tlbl` (per-track
//!   labels) and many optional chunks may follow. Unknown chunks whose
//!   first letter is uppercase are treated as mandatory and rejected.
//!
//! References (read-only):
//!
//! * `docs/audio/nsf/nsf-nesdev-wiki.html` (v1 layout).
//! * `docs/audio/nsf/nsf-wiki-source.md` (wikitext reproduction).
//! * `docs/audio/nsf/nsf2-nesdev-wiki.html` (v2 + feature-byte $7C +
//!   24-bit data length + embedded metadata rules).
//! * `docs/audio/nsf/nsfe-nesdev-wiki.html` (chunk format reused by
//!   NSF2 metadata).
//! * `docs/audio/nsf/nsfdrv-nesdev-wiki.html` (NSFDRV sound-driver
//!   identification tag at the start of the program data).

use core::fmt;

use crate::nsfe::{self, NsfeMetaError, NsfeMetadata};

/// NSF v1 magic — `NESM\x1a`.
pub const NSF_MAGIC: [u8; 5] = *b"NESM\x1a";

/// NSFe extension magic — `NSFE`.
pub const NSFE_MAGIC: [u8; 4] = *b"NSFE";

/// Fixed header size for the v1.x format.
pub const NSF_HEADER_LEN: usize = 0x80;

/// Region indicator (the low two bits of the v1 region byte plus the
/// NSFe `regn` chunk's preferred-region byte).
///
/// The v1 NSF header byte at `$7A` only encodes NTSC vs PAL vs dual —
/// Dendy is exclusively surfaced through the `regn` extended chunk per
/// `docs/audio/nsf/nsfe-nesdev-wiki.html` §regn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NsfRegion {
    Ntsc,
    Pal,
    Dual,
    /// Dendy — Russian NES clone running at 1.773448 MHz CPU clock.
    /// `regn.preferred == 2` selects this; the INIT call receives
    /// `X=2` per spec. When the dedicated Dendy play-period is absent
    /// the player falls back to the PAL period (per the same spec).
    Dendy,
}

impl NsfRegion {
    fn from_byte(b: u8) -> Self {
        let pal = b & 0x01 != 0;
        let dual = b & 0x02 != 0;
        if dual {
            NsfRegion::Dual
        } else if pal {
            NsfRegion::Pal
        } else {
            NsfRegion::Ntsc
        }
    }
}

/// Expansion-chip flag byte at offset 0x7B.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExpansionChips(pub u8);

impl ExpansionChips {
    pub fn vrc6(self) -> bool {
        self.0 & 0x01 != 0
    }
    pub fn vrc7(self) -> bool {
        self.0 & 0x02 != 0
    }
    pub fn fds(self) -> bool {
        self.0 & 0x04 != 0
    }
    pub fn mmc5(self) -> bool {
        self.0 & 0x08 != 0
    }
    pub fn n163(self) -> bool {
        self.0 & 0x10 != 0
    }
    pub fn s5b(self) -> bool {
        self.0 & 0x20 != 0
    }
    /// NSF2 added a 7th expansion-chip flag for VT02+ audio (bit 6).
    /// Untested on real hardware; surfaced so callers can detect it.
    pub fn vt02(self) -> bool {
        self.0 & 0x40 != 0
    }
}

/// NSF2 feature-flag byte at offset `$7C`. Bits per
/// `docs/audio/nsf/nsf2-nesdev-wiki.html`:
///
/// * bits 0..=3: reserved, must be 0.
/// * bit 4: IRQ support (`$401B/C/D` timer + vector overlay).
/// * bit 5: non-returning INIT (two-phase INIT + NMI-driven PLAY).
/// * bit 6: suppressed PLAY (PLAY subroutine will never be called).
/// * bit 7: appended NSFe metadata is mandatory (chunk-name-uppercase
///   semantics — player must succeed at parsing).
///
/// On v1 files this byte MUST be ignored per the spec.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nsf2Features(pub u8);

impl Nsf2Features {
    pub fn irq_support(self) -> bool {
        self.0 & 0x10 != 0
    }
    pub fn non_returning_init(self) -> bool {
        self.0 & 0x20 != 0
    }
    pub fn suppressed_play(self) -> bool {
        self.0 & 0x40 != 0
    }
    pub fn mandatory_metadata(self) -> bool {
        self.0 & 0x80 != 0
    }
    /// True iff a `$FFFA-$FFFF` vector overlay is required by the
    /// player (IRQ or non-returning INIT feature is set).
    pub fn needs_vector_overlay(self) -> bool {
        self.irq_support() || self.non_returning_init()
    }
}

/// NSFDRV sound-driver identification tag, per
/// `docs/audio/nsf/nsfdrv-nesdev-wiki.html`.
///
/// The tag occupies the first 8 bytes of the program data — in a plain
/// NSF file that is offsets `$0080..=$0087`, immediately after the
/// 128-byte header per the wiki's §"File Format" layout table:
///
/// ```text
/// $0080 - $0085   6   Sound driver ID
/// $0086           1   Major version number
/// $0087           1   Minor version number
/// ```
///
/// Per §"Usage and applications", players can use the ID to recognise
/// sound drivers developed against inaccurate playback environments
/// (and patch accordingly), authors can assert an NSF is original
/// rather than ripped from a ROM, and bug reports can pin the driver
/// version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NsfDrvTag {
    /// 6-byte ASCII sound-driver ID (program bytes 0..=5, file
    /// `$0080-$0085`).
    pub id: [u8; 6],
    /// Major version number (program byte 6, file `$0086`).
    pub major: u8,
    /// Minor version number (program byte 7, file `$0087`).
    pub minor: u8,
}

/// Driver IDs registered in `docs/audio/nsf/nsfdrv-nesdev-wiki.html`
/// §"List of NSFDRV sound driver IDs". Variants are named after the
/// 6-byte ASCII tag itself, not after any product.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NsfDrvId {
    /// `"OFGS  "` — binary `$4F $46 $47 $53 $20 $20`.
    Ofgs,
    /// `"FTDRV "` — binary `$46 $54 $44 $52 $56 $20`.
    Ftdrv,
    /// `"NSDL  "` — binary `$4E $53 $44 $4C $20 $20`.
    Nsdl,
    /// `"      "` (six spaces) — binary `$20` × 6. Per the wiki, "a
    /// blank NSFDRV ID may be used for sound drivers under
    /// development".
    Blank,
}

impl NsfDrvTag {
    /// Reads the raw 8-byte tag from the start of a program blob.
    /// Returns `None` when the program is shorter than 8 bytes. No
    /// validation is performed — every 8-byte-plus program yields a
    /// tag; use [`NsfDrvTag::known_id`] (or [`NsfHeader::nsfdrv`]) to
    /// test the ID against the wiki's registered list.
    pub fn read(program: &[u8]) -> Option<NsfDrvTag> {
        let bytes = program.get(..8)?;
        let mut id = [0u8; 6];
        id.copy_from_slice(&bytes[..6]);
        Some(NsfDrvTag {
            id,
            major: bytes[6],
            minor: bytes[7],
        })
    }

    /// Matches the 6-byte ID against the registered IDs from the
    /// wiki's §"List of NSFDRV sound driver IDs" (including the
    /// documented blank in-development ID). `None` for anything else.
    pub fn known_id(&self) -> Option<NsfDrvId> {
        match &self.id {
            b"OFGS  " => Some(NsfDrvId::Ofgs),
            b"FTDRV " => Some(NsfDrvId::Ftdrv),
            b"NSDL  " => Some(NsfDrvId::Nsdl),
            b"      " => Some(NsfDrvId::Blank),
            _ => None,
        }
    }

    /// The ID bytes as ASCII, if they are printable ASCII (space
    /// through tilde). Registered IDs always are.
    pub fn id_ascii(&self) -> Option<&str> {
        if self.id.iter().all(|&b| (0x20..=0x7e).contains(&b)) {
            core::str::from_utf8(&self.id).ok()
        } else {
            None
        }
    }
}

/// Parsed NSF header + the raw program data tail.
#[derive(Clone, Debug)]
pub struct NsfHeader {
    pub version: u8,
    pub total_songs: u8,
    pub starting_song: u8,
    pub load_addr: u16,
    pub init_addr: u16,
    pub play_addr: u16,
    pub song_name: String,
    pub artist: String,
    pub copyright: String,
    pub ntsc_speed_us: u16,
    pub pal_speed_us: u16,
    pub bankswitch_init: [u8; 8],
    pub region: NsfRegion,
    pub expansion: ExpansionChips,
    pub program: Vec<u8>,
    pub track_labels: Vec<String>,
    pub is_nsfe: bool,
    /// NSF2 feature byte. Always `Nsf2Features(0)` on v1 files (the
    /// spec says callers MUST ignore byte `$7C` when `version == 1`).
    pub nsf2: Nsf2Features,
    /// Raw NSFe-format metadata appended after the program (NSF2 only,
    /// when bytes `$7D-$7F` are non-zero). Empty on v1 / NSFe inputs.
    /// Stored verbatim; per spec it never starts with an `NSFE` fourCC
    /// and never contains `INFO` / `DATA` / `BANK` / `NSF2` chunks.
    pub nsf2_metadata: Vec<u8>,
    /// Parsed extended NSFe chunks (`auth` / `tlbl` / `taut` / `text`
    /// / `time` / `fade` / `plst` / `psfx` / `mixe` / `regn` / `RATE`
    /// / `VRC7`). Populated for NSFe files and for NSF2 files whose
    /// appended-metadata blob is non-empty; defaulted on v1 NSF.
    pub metadata: NsfeMetadata,
}

impl NsfHeader {
    /// Returns the playback rate in Hz for the chosen region.
    ///
    /// Per `docs/audio/nsf/nsfe-nesdev-wiki.html` §RATE: if the
    /// region-specific period is zero the player uses the region's
    /// default (NTSC 60.0024 Hz / PAL 50.006 Hz). Dendy falls back to
    /// the dedicated Dendy speed (`NsfeRate::dendy_us`) first, then
    /// to the PAL speed, then to the PAL default — exactly as the
    /// spec dictates.
    pub fn play_rate_hz(&self) -> f64 {
        let us = self.play_period_us();
        if us == 0 {
            match self.region {
                NsfRegion::Pal | NsfRegion::Dendy => 50.006,
                NsfRegion::Ntsc | NsfRegion::Dual => 60.0024,
            }
        } else {
            1_000_000.0 / us as f64
        }
    }

    /// Effective per-region play period in microseconds. Looks at the
    /// dedicated Dendy speed (NSFe `RATE` chunk byte $0004) before
    /// falling back to the PAL field, matching the spec text.
    pub fn play_period_us(&self) -> u16 {
        match self.region {
            NsfRegion::Pal => self.pal_speed_us,
            NsfRegion::Dendy => {
                let dendy = self
                    .metadata
                    .rate
                    .as_ref()
                    .and_then(|r| r.dendy_us)
                    .unwrap_or(0);
                if dendy != 0 {
                    dendy
                } else {
                    self.pal_speed_us
                }
            }
            NsfRegion::Ntsc | NsfRegion::Dual => self.ntsc_speed_us,
        }
    }

    pub fn has_expansion(&self) -> bool {
        self.expansion.0 != 0
    }

    /// Best-effort NSFDRV sound-driver identification, per
    /// `docs/audio/nsf/nsfdrv-nesdev-wiki.html`. Returns the 8-byte
    /// tag at the start of the program data only when its 6-byte ID
    /// matches one of the wiki's registered IDs (the wiki defines no
    /// stronger presence predicate than the ID list itself, so an
    /// unregistered first-6-bytes pattern is treated as plain program
    /// data). Note the documented blank in-development ID (six
    /// spaces) is also matched — callers wanting to treat it as
    /// absent can filter on [`NsfDrvTag::known_id`] `!=`
    /// [`NsfDrvId::Blank`].
    ///
    /// For plain NSF the program data starts at file offset `$0080`,
    /// matching the wiki's layout table exactly; for NSFe/NSF2 inputs
    /// the tag is read from the same program blob (the `DATA` chunk /
    /// pre-metadata program block).
    pub fn nsfdrv(&self) -> Option<NsfDrvTag> {
        let tag = NsfDrvTag::read(&self.program)?;
        tag.known_id().map(|_| tag)
    }
}

/// Failures from the on-disk header parser.
#[derive(Debug, PartialEq, Eq)]
pub enum NsfError {
    TooShort {
        needed: usize,
        got: usize,
    },
    BadMagic,
    BadVersion(u8),
    NoSongs,
    NsfeTruncatedChunk,
    NsfeChunkOverflow,
    NsfeMissingRequired(&'static str),
    NsfeUnknownMandatory([u8; 4]),
    /// A `DATA` chunk appeared before the `INFO` chunk. The spec
    /// (`docs/audio/nsf/nsfe-nesdev-wiki.html` §Chunks) requires the
    /// `INFO` chunk to precede `DATA`.
    NsfeDataBeforeInfo,
    /// NSF2 declared a 24-bit data length at `$7D-$7F` that runs past
    /// the end of the file.
    Nsf2DataLengthOverflow {
        declared: usize,
        available: usize,
    },
    /// The NSFe extended-chunk parser rejected the appended metadata
    /// (truncated chunk, overflowing size, illegal payload length, or
    /// an unknown mandatory chunk).
    Metadata(NsfeMetaError),
}

impl From<NsfeMetaError> for NsfError {
    fn from(err: NsfeMetaError) -> Self {
        NsfError::Metadata(err)
    }
}

impl fmt::Display for NsfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NsfError::TooShort { needed, got } => {
                write!(f, "NSF: buffer shorter than required ({got} < {needed})")
            }
            NsfError::BadMagic => f.write_str("NSF: bad magic (expected NESM\\x1a or NSFE)"),
            NsfError::BadVersion(v) => write!(f, "NSF: invalid version {v} (must be >= 1)"),
            NsfError::NoSongs => f.write_str("NSF: total_songs is zero"),
            NsfError::NsfeTruncatedChunk => f.write_str("NSFe: chunk header truncated"),
            NsfError::NsfeChunkOverflow => f.write_str("NSFe: chunk size overflows buffer"),
            NsfError::NsfeMissingRequired(name) => write!(f, "NSFe: missing required chunk {name}"),
            NsfError::NsfeDataBeforeInfo => {
                f.write_str("NSFe: DATA chunk appeared before the INFO chunk")
            }
            NsfError::NsfeUnknownMandatory(fcc) => write!(
                f,
                "NSFe: unknown mandatory chunk {:?}",
                core::str::from_utf8(fcc).unwrap_or("????")
            ),
            NsfError::Nsf2DataLengthOverflow {
                declared,
                available,
            } => write!(
                f,
                "NSF2: declared data length {declared} overflows available program bytes {available}"
            ),
            NsfError::Metadata(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for NsfError {}

/// Parse an NSF v1.x or NSFe blob into an [`NsfHeader`].
pub fn parse_nsf(bytes: &[u8]) -> Result<NsfHeader, NsfError> {
    if bytes.len() >= 4 && bytes[..4] == NSFE_MAGIC {
        return parse_nsfe(bytes);
    }
    parse_nsf_v1(bytes)
}

fn parse_nsf_v1(bytes: &[u8]) -> Result<NsfHeader, NsfError> {
    if bytes.len() < NSF_HEADER_LEN {
        return Err(NsfError::TooShort {
            needed: NSF_HEADER_LEN,
            got: bytes.len(),
        });
    }
    if bytes[..5] != NSF_MAGIC {
        return Err(NsfError::BadMagic);
    }
    let version = bytes[0x05];
    if version == 0 {
        return Err(NsfError::BadVersion(version));
    }
    let total_songs = bytes[0x06];
    if total_songs == 0 {
        return Err(NsfError::NoSongs);
    }
    let starting_song = bytes[0x07];
    let load_addr = u16::from_le_bytes([bytes[0x08], bytes[0x09]]);
    let init_addr = u16::from_le_bytes([bytes[0x0a], bytes[0x0b]]);
    let play_addr = u16::from_le_bytes([bytes[0x0c], bytes[0x0d]]);
    let song_name = read_nsf_string(&bytes[0x0e..0x2e]);
    let artist = read_nsf_string(&bytes[0x2e..0x4e]);
    let copyright = read_nsf_string(&bytes[0x4e..0x6e]);
    let ntsc_speed_us = u16::from_le_bytes([bytes[0x6e], bytes[0x6f]]);
    let mut bankswitch_init = [0u8; 8];
    bankswitch_init.copy_from_slice(&bytes[0x70..0x78]);
    let pal_speed_us = u16::from_le_bytes([bytes[0x78], bytes[0x79]]);
    let region = NsfRegion::from_byte(bytes[0x7a]);
    let expansion = ExpansionChips(bytes[0x7b]);

    // Byte $7C: NSF2 feature flags. Per spec the byte MUST be ignored
    // when version < 2 — this hands back `Nsf2Features(0)` so callers
    // never accidentally honour stale data in a v1 file.
    let nsf2 = if version >= 2 {
        Nsf2Features(bytes[0x7c])
    } else {
        Nsf2Features(0)
    };

    // Bytes $7D-$7F: 24-bit little-endian length of program data.
    // Defined for both v1 and v2; on v1 it merely allows the file to
    // be padded with metadata that older players treat as ROM. On v2
    // a non-zero length means appended NSFe-style metadata follows the
    // program block.
    let raw_tail = &bytes[NSF_HEADER_LEN..];
    let declared_len =
        (bytes[0x7d] as usize) | ((bytes[0x7e] as usize) << 8) | ((bytes[0x7f] as usize) << 16);

    let (program, nsf2_metadata) = if declared_len == 0 {
        (raw_tail.to_vec(), Vec::new())
    } else {
        if declared_len > raw_tail.len() {
            return Err(NsfError::Nsf2DataLengthOverflow {
                declared: declared_len,
                available: raw_tail.len(),
            });
        }
        (
            raw_tail[..declared_len].to_vec(),
            raw_tail[declared_len..].to_vec(),
        )
    };

    // Parse the appended NSF2 metadata blob (a bare chunk run with no
    // `NSFE` magic and no INFO/DATA/BANK/NSF2 chunks per spec). Empty
    // blobs decode to a defaulted `NsfeMetadata`. Lift extended
    // strings into the legacy v1 string fields when the v1 fields are
    // unset and the metadata supplied them.
    let mut metadata = if nsf2_metadata.is_empty() {
        NsfeMetadata::default()
    } else {
        nsfe::parse_metadata_chunks(&nsf2_metadata)?
    };
    let (mut song_name, mut artist, mut copyright) = (song_name, artist, copyright);
    if let Some(auth) = metadata.auth.as_ref() {
        if song_name.is_empty() {
            song_name = auth.title.clone();
        }
        if artist.is_empty() {
            artist = auth.artist.clone();
        }
        if copyright.is_empty() {
            copyright = auth.copyright.clone();
        }
    }
    let track_labels = std::mem::take(&mut metadata.track_labels);

    // NSF2 appended metadata may carry a `regn` chunk whose preferred
    // byte overrides the v1 region byte ($7A). Specifically, Dendy is
    // exclusively surfaced through `regn` per spec.
    let region = match metadata.regions.as_ref().and_then(|r| r.preferred) {
        Some(0) => NsfRegion::Ntsc,
        Some(1) => NsfRegion::Pal,
        Some(2) => NsfRegion::Dendy,
        _ => region,
    };

    // `RATE` chunk (when present) overrides the v1 header period fields.
    let (ntsc_speed_us, pal_speed_us) = match &metadata.rate {
        Some(rate) => (
            rate.ntsc_us.unwrap_or(ntsc_speed_us),
            rate.pal_us.unwrap_or(pal_speed_us),
        ),
        None => (ntsc_speed_us, pal_speed_us),
    };

    Ok(NsfHeader {
        version,
        total_songs,
        starting_song,
        load_addr,
        init_addr,
        play_addr,
        song_name,
        artist,
        copyright,
        ntsc_speed_us,
        pal_speed_us,
        bankswitch_init,
        region,
        expansion,
        program,
        track_labels,
        is_nsfe: false,
        nsf2,
        nsf2_metadata,
        metadata,
    })
}

fn parse_nsfe(bytes: &[u8]) -> Result<NsfHeader, NsfError> {
    // Two-pass walk: first peel off the chunks that belong to the v1
    // shadow header (INFO / DATA / BANK / NSF2) plus the special
    // `NEND` terminator, then re-feed every other chunk into the
    // extended-metadata parser so the heavy decoding for `auth /
    // tlbl / taut / text / time / fade / plst / psfx / mixe / regn
    // / RATE / VRC7` lives in one place.
    let mut info: Option<NsfeInfo> = None;
    let mut data: Option<Vec<u8>> = None;
    let mut bank_init: Option<[u8; 8]> = None;
    let mut nsf2_features: Option<u8> = None;
    let mut meta_blob = Vec::<u8>::new();
    let mut saw_nend = false;

    let mut cursor = 4usize;
    while cursor < bytes.len() {
        if bytes.len() - cursor < 8 {
            return Err(NsfError::NsfeTruncatedChunk);
        }
        let size = u32::from_le_bytes([
            bytes[cursor],
            bytes[cursor + 1],
            bytes[cursor + 2],
            bytes[cursor + 3],
        ]) as usize;
        let mut fcc = [0u8; 4];
        fcc.copy_from_slice(&bytes[cursor + 4..cursor + 8]);
        let body_start = cursor + 8;
        let body_end = body_start
            .checked_add(size)
            .ok_or(NsfError::NsfeChunkOverflow)?;
        if body_end > bytes.len() {
            return Err(NsfError::NsfeChunkOverflow);
        }
        let body = &bytes[body_start..body_end];

        match &fcc {
            b"INFO" => info = Some(parse_nsfe_info(body)?),
            b"DATA" => {
                // "the 'INFO' chunk must precede the 'DATA' chunk"
                // (docs/audio/nsf/nsfe-nesdev-wiki.html §Chunks).
                if info.is_none() {
                    return Err(NsfError::NsfeDataBeforeInfo);
                }
                data = Some(body.to_vec());
            }
            b"BANK" => {
                let mut b = [0u8; 8];
                let n = body.len().min(8);
                b[..n].copy_from_slice(&body[..n]);
                bank_init = Some(b);
            }
            b"NSF2" => {
                // Single-byte mirror of the NSF2 header feature
                // bits — surface so callers can act on the IRQ /
                // non-returning-INIT / suppressed-PLAY flags.
                nsf2_features = Some(body.first().copied().unwrap_or(0));
            }
            b"NEND" => {
                // Terminator: "the 'NEND' chunk marks the end of the
                // file; no further chunks should be read past 'NEND'".
                saw_nend = true;
                break;
            }
            // Everything else gets re-emitted into the metadata
            // buffer for the extended parser, which knows the full
            // catalogue of optional chunks (`auth / tlbl / taut /
            // text / time / fade / plst / psfx / mixe / regn /
            // RATE / VRC7`). Eagerly enforce the mandatory rule
            // here so a malformed file is rejected before we
            // bother walking the rest.
            _ => {
                if !is_known_extended(&fcc) && fcc[0].is_ascii_uppercase() {
                    return Err(NsfError::NsfeUnknownMandatory(fcc));
                }
                meta_blob.extend_from_slice(&(size as u32).to_le_bytes());
                meta_blob.extend_from_slice(&fcc);
                meta_blob.extend_from_slice(body);
            }
        }
        cursor = body_end;
    }

    let info = info.ok_or(NsfError::NsfeMissingRequired("INFO"))?;
    let program = data.ok_or(NsfError::NsfeMissingRequired("DATA"))?;
    // "There are three chunks that are required for a well formed
    // NSFe: 'INFO' ... 'DATA' ... 'NEND'" — a chunk run that simply
    // hits end-of-buffer without the terminator is malformed.
    if !saw_nend {
        return Err(NsfError::NsfeMissingRequired("NEND"));
    }

    let mut metadata = if meta_blob.is_empty() {
        NsfeMetadata::default()
    } else {
        nsfe::parse_metadata_chunks(&meta_blob)?
    };

    let (mut song_name, mut artist, mut copyright) = (String::new(), String::new(), String::new());
    if let Some(auth) = metadata.auth.as_ref() {
        song_name = auth.title.clone();
        artist = auth.artist.clone();
        copyright = auth.copyright.clone();
    }
    let track_labels = std::mem::take(&mut metadata.track_labels);

    // RATE chunk takes precedence over the v1 header defaults when
    // present (and when it provides the matching region's period).
    let (ntsc_speed_us, pal_speed_us) = match &metadata.rate {
        Some(rate) => (rate.ntsc_us.unwrap_or(0), rate.pal_us.unwrap_or(0)),
        None => (0, 0),
    };

    // regn overrides the INFO byte-6 region when present. Dendy is
    // exclusively surfaced through this chunk per spec — there is no
    // v1-byte $7A encoding for it.
    let region = match metadata.regions.as_ref().and_then(|r| r.preferred) {
        Some(0) => NsfRegion::Ntsc,
        Some(1) => NsfRegion::Pal,
        Some(2) => NsfRegion::Dendy,
        _ => NsfRegion::from_byte(info.region),
    };

    let bankswitch_init = bank_init.unwrap_or([0u8; 8]);

    Ok(NsfHeader {
        version: 1,
        total_songs: info.total_songs,
        starting_song: info.starting_song,
        load_addr: info.load_addr,
        init_addr: info.init_addr,
        play_addr: info.play_addr,
        song_name,
        artist,
        copyright,
        ntsc_speed_us,
        pal_speed_us,
        bankswitch_init,
        region,
        expansion: ExpansionChips(info.expansion),
        program,
        track_labels,
        is_nsfe: true,
        nsf2: Nsf2Features(nsf2_features.unwrap_or(0)),
        nsf2_metadata: Vec::new(),
        metadata,
    })
}

struct NsfeInfo {
    load_addr: u16,
    init_addr: u16,
    play_addr: u16,
    region: u8,
    expansion: u8,
    total_songs: u8,
    starting_song: u8,
}

fn parse_nsfe_info(body: &[u8]) -> Result<NsfeInfo, NsfError> {
    // "This chunk must be at least 9 bytes long. If starting song is
    // missing, 0 may be assumed. Any extra data in this chunk may be
    // ignored." (docs/audio/nsf/nsfe-nesdev-wiki.html §INFO) — so the
    // total-songs byte at offset 8 is mandatory, the starting-song
    // byte at offset 9 is the only optional field.
    if body.len() < 9 {
        return Err(NsfError::NsfeTruncatedChunk);
    }
    let total_songs = body[8];
    if total_songs == 0 {
        return Err(NsfError::NoSongs);
    }
    Ok(NsfeInfo {
        load_addr: u16::from_le_bytes([body[0], body[1]]),
        init_addr: u16::from_le_bytes([body[2], body[3]]),
        play_addr: u16::from_le_bytes([body[4], body[5]]),
        region: body[6],
        expansion: body[7],
        total_songs,
        starting_song: body.get(9).copied().unwrap_or(0),
    })
}

/// True for any chunk FOURCC the extended-metadata parser knows how
/// to decode (so a header-layer pre-pass can let it through without
/// triggering the "unknown mandatory chunk" check on uppercase tags).
fn is_known_extended(fcc: &[u8; 4]) -> bool {
    matches!(
        fcc,
        b"auth"
            | b"tlbl"
            | b"taut"
            | b"text"
            | b"time"
            | b"fade"
            | b"plst"
            | b"psfx"
            | b"mixe"
            | b"regn"
            | b"RATE"
            | b"VRC7"
    )
}

fn read_nsf_string(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    let trimmed = &field[..end];
    let mut last = trimmed.len();
    while last > 0 && (trimmed[last - 1] == b' ' || trimmed[last - 1] == b'\t') {
        last -= 1;
    }
    trimmed[..last].iter().map(|&b| b as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_v1() -> Vec<u8> {
        let mut buf = vec![0u8; NSF_HEADER_LEN + 4];
        buf[..5].copy_from_slice(&NSF_MAGIC);
        buf[0x05] = 1;
        buf[0x06] = 3;
        buf[0x07] = 1;
        buf[0x08..0x0a].copy_from_slice(&0x8000u16.to_le_bytes());
        buf[0x0a..0x0c].copy_from_slice(&0x8003u16.to_le_bytes());
        buf[0x0c..0x0e].copy_from_slice(&0x8006u16.to_le_bytes());
        buf[0x0e..0x13].copy_from_slice(b"Hello");
        buf[0x2e..0x36].copy_from_slice(b"Karpeles");
        buf[0x4e..0x53].copy_from_slice(b"2026 ");
        buf[0x6e..0x70].copy_from_slice(&16666u16.to_le_bytes());
        buf[0x78..0x7a].copy_from_slice(&19997u16.to_le_bytes());
        buf[0x7a] = 0x02;
        buf[0x7b] = 0x01;
        buf[NSF_HEADER_LEN..NSF_HEADER_LEN + 4].copy_from_slice(&[0xea, 0xea, 0x60, 0x00]);
        buf
    }

    #[test]
    fn parses_minimal_v1() {
        let h = parse_nsf(&fake_v1()).unwrap();
        assert_eq!(h.version, 1);
        assert_eq!(h.total_songs, 3);
        assert_eq!(h.starting_song, 1);
        assert_eq!(h.load_addr, 0x8000);
        assert_eq!(h.init_addr, 0x8003);
        assert_eq!(h.play_addr, 0x8006);
        assert_eq!(h.song_name, "Hello");
        assert_eq!(h.artist, "Karpeles");
        assert_eq!(h.copyright, "2026");
        assert_eq!(h.ntsc_speed_us, 16666);
        assert_eq!(h.pal_speed_us, 19997);
        assert_eq!(h.region, NsfRegion::Dual);
        assert!(h.expansion.vrc6());
        assert!(!h.expansion.vrc7());
        assert_eq!(h.program, vec![0xea, 0xea, 0x60, 0x00]);
        assert!(!h.is_nsfe);
        assert!((h.play_rate_hz() - 60.0024).abs() < 0.01);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = fake_v1();
        buf[0] = b'X';
        assert_eq!(parse_nsf(&buf).unwrap_err(), NsfError::BadMagic);
    }

    #[test]
    fn rejects_zero_version() {
        let mut buf = fake_v1();
        buf[0x05] = 0;
        assert_eq!(parse_nsf(&buf).unwrap_err(), NsfError::BadVersion(0));
    }

    #[test]
    fn rejects_zero_total_songs() {
        let mut buf = fake_v1();
        buf[0x06] = 0;
        assert_eq!(parse_nsf(&buf).unwrap_err(), NsfError::NoSongs);
    }

    #[test]
    fn parses_nsfe() {
        let mut out = Vec::new();
        out.extend_from_slice(&NSFE_MAGIC);
        let info_payload: [u8; 10] = [0x00, 0x80, 0x03, 0x80, 0x06, 0x80, 0x00, 0x00, 2, 0];
        out.extend_from_slice(&(info_payload.len() as u32).to_le_bytes());
        out.extend_from_slice(b"INFO");
        out.extend_from_slice(&info_payload);

        let auth: Vec<u8> = b"Title\0Author\0Copy\0Ripper\0".to_vec();
        out.extend_from_slice(&(auth.len() as u32).to_le_bytes());
        out.extend_from_slice(b"auth");
        out.extend_from_slice(&auth);

        let tlbl: Vec<u8> = b"Track 1\0Track 2\0".to_vec();
        out.extend_from_slice(&(tlbl.len() as u32).to_le_bytes());
        out.extend_from_slice(b"tlbl");
        out.extend_from_slice(&tlbl);

        let data = vec![0xea, 0x60];
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"DATA");
        out.extend_from_slice(&data);

        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"NEND");

        let h = parse_nsf(&out).unwrap();
        assert!(h.is_nsfe);
        assert_eq!(h.song_name, "Title");
        assert_eq!(h.artist, "Author");
        assert_eq!(h.copyright, "Copy");
        assert_eq!(h.total_songs, 2);
        assert_eq!(h.load_addr, 0x8000);
        assert_eq!(h.program, vec![0xea, 0x60]);
        assert_eq!(
            h.track_labels,
            vec!["Track 1".to_string(), "Track 2".into()]
        );
    }

    /// Helper: emit one `[u32 len][fourcc][body]` chunk.
    fn push_chunk(out: &mut Vec<u8>, tag: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(tag);
        out.extend_from_slice(body);
    }

    #[test]
    fn nsfe_rejects_data_before_info() {
        // "the 'INFO' chunk must precede the 'DATA' chunk".
        let mut out = Vec::new();
        out.extend_from_slice(&NSFE_MAGIC);
        push_chunk(&mut out, b"DATA", &[0xea, 0x60]);
        let info: [u8; 10] = [0x00, 0x80, 0x03, 0x80, 0x06, 0x80, 0x00, 0x00, 1, 0];
        push_chunk(&mut out, b"INFO", &info);
        push_chunk(&mut out, b"NEND", &[]);
        assert_eq!(parse_nsf(&out).unwrap_err(), NsfError::NsfeDataBeforeInfo);
    }

    #[test]
    fn nsfe_requires_nend_terminator() {
        // "There are three chunks that are required for a well formed
        // NSFe" — INFO, DATA and NEND. A stream that just runs off the
        // end of the buffer is malformed.
        let mut out = Vec::new();
        out.extend_from_slice(&NSFE_MAGIC);
        let info: [u8; 10] = [0x00, 0x80, 0x03, 0x80, 0x06, 0x80, 0x00, 0x00, 1, 0];
        push_chunk(&mut out, b"INFO", &info);
        push_chunk(&mut out, b"DATA", &[0xea, 0x60]);
        assert_eq!(
            parse_nsf(&out).unwrap_err(),
            NsfError::NsfeMissingRequired("NEND")
        );

        // Appending the terminator fixes the same stream.
        push_chunk(&mut out, b"NEND", &[]);
        assert!(parse_nsf(&out).is_ok());
    }

    #[test]
    fn nsfe_trailing_bytes_after_nend_are_ignored() {
        let mut out = Vec::new();
        out.extend_from_slice(&NSFE_MAGIC);
        let info: [u8; 10] = [0x00, 0x80, 0x03, 0x80, 0x06, 0x80, 0x00, 0x00, 1, 0];
        push_chunk(&mut out, b"INFO", &info);
        push_chunk(&mut out, b"DATA", &[0xea, 0x60]);
        push_chunk(&mut out, b"NEND", &[]);
        // Arbitrary garbage — including a truncated chunk header —
        // must be ignored past NEND.
        out.extend_from_slice(&[0xff, 0x00, 0xde]);
        assert!(parse_nsf(&out).is_ok());
    }

    #[test]
    fn nsfe_info_requires_nine_bytes() {
        // "This chunk must be at least 9 bytes long" — the total-songs
        // byte is mandatory; only starting-song (offset 9) may be
        // omitted (defaulting to 0).
        for len in 0..9usize {
            let mut out = Vec::new();
            out.extend_from_slice(&NSFE_MAGIC);
            let full: [u8; 10] = [0x00, 0x80, 0x03, 0x80, 0x06, 0x80, 0x00, 0x00, 1, 0];
            push_chunk(&mut out, b"INFO", &full[..len]);
            push_chunk(&mut out, b"DATA", &[0x60]);
            push_chunk(&mut out, b"NEND", &[]);
            assert_eq!(
                parse_nsf(&out).unwrap_err(),
                NsfError::NsfeTruncatedChunk,
                "INFO length {len} must be rejected"
            );
        }
    }

    #[test]
    fn nsfe_info_nine_bytes_defaults_starting_song_to_zero() {
        let mut out = Vec::new();
        out.extend_from_slice(&NSFE_MAGIC);
        // 9-byte INFO: no starting-song byte -> 0 assumed. Also
        // exercise the "extra data may be ignored" rule with a
        // 12-byte variant below.
        let info: [u8; 9] = [0x00, 0x80, 0x03, 0x80, 0x06, 0x80, 0x00, 0x00, 4];
        push_chunk(&mut out, b"INFO", &info);
        push_chunk(&mut out, b"DATA", &[0x60]);
        push_chunk(&mut out, b"NEND", &[]);
        let h = parse_nsf(&out).unwrap();
        assert_eq!(h.total_songs, 4);
        assert_eq!(h.starting_song, 0);

        let mut out = Vec::new();
        out.extend_from_slice(&NSFE_MAGIC);
        let info: [u8; 12] = [
            0x00, 0x80, 0x03, 0x80, 0x06, 0x80, 0x00, 0x00, 4, 2, 0xAA, 0xBB,
        ];
        push_chunk(&mut out, b"INFO", &info);
        push_chunk(&mut out, b"DATA", &[0x60]);
        push_chunk(&mut out, b"NEND", &[]);
        let h = parse_nsf(&out).unwrap();
        assert_eq!(h.total_songs, 4);
        assert_eq!(h.starting_song, 2);
    }

    #[test]
    fn nsfe_info_rejects_zero_total_songs() {
        let mut out = Vec::new();
        out.extend_from_slice(&NSFE_MAGIC);
        let info: [u8; 10] = [0x00, 0x80, 0x03, 0x80, 0x06, 0x80, 0x00, 0x00, 0, 0];
        push_chunk(&mut out, b"INFO", &info);
        push_chunk(&mut out, b"DATA", &[0x60]);
        push_chunk(&mut out, b"NEND", &[]);
        assert_eq!(parse_nsf(&out).unwrap_err(), NsfError::NoSongs);
    }

    #[test]
    fn nsfe_rejects_unknown_mandatory_chunk() {
        let mut out = Vec::new();
        out.extend_from_slice(&NSFE_MAGIC);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"ZZZZ");
        let err = parse_nsf(&out).unwrap_err();
        assert!(matches!(err, NsfError::NsfeUnknownMandatory(_)));
    }

    fn fake_v2(feature_byte: u8, program: &[u8], appended_metadata: &[u8]) -> Vec<u8> {
        let mut buf = vec![0u8; NSF_HEADER_LEN];
        buf[..5].copy_from_slice(&NSF_MAGIC);
        buf[0x05] = 2;
        buf[0x06] = 1;
        buf[0x07] = 1;
        buf[0x08..0x0a].copy_from_slice(&0x8000u16.to_le_bytes());
        buf[0x0a..0x0c].copy_from_slice(&0x8000u16.to_le_bytes());
        buf[0x0c..0x0e].copy_from_slice(&0x8003u16.to_le_bytes());
        buf[0x6e..0x70].copy_from_slice(&16666u16.to_le_bytes());
        buf[0x78..0x7a].copy_from_slice(&19997u16.to_le_bytes());
        buf[0x7b] = 0;
        buf[0x7c] = feature_byte;
        let len = if appended_metadata.is_empty() {
            0
        } else {
            program.len()
        };
        buf[0x7d] = (len & 0xFF) as u8;
        buf[0x7e] = ((len >> 8) & 0xFF) as u8;
        buf[0x7f] = ((len >> 16) & 0xFF) as u8;
        buf.extend_from_slice(program);
        buf.extend_from_slice(appended_metadata);
        buf
    }

    #[test]
    fn parses_nsf2_feature_byte() {
        // bits 4 (IRQ) + 5 (non-returning init) + 6 (suppressed play).
        let bytes = fake_v2(0x10 | 0x20 | 0x40, &[0xea, 0x60], &[]);
        let h = parse_nsf(&bytes).unwrap();
        assert_eq!(h.version, 2);
        assert!(h.nsf2.irq_support());
        assert!(h.nsf2.non_returning_init());
        assert!(h.nsf2.suppressed_play());
        assert!(!h.nsf2.mandatory_metadata());
        assert!(h.nsf2.needs_vector_overlay());
        assert!(h.nsf2_metadata.is_empty());
        assert_eq!(h.program, vec![0xea, 0x60]);
    }

    #[test]
    fn v1_ignores_byte_7c_per_spec() {
        let mut buf = fake_v1();
        // Even with the byte set, v1 must surface Nsf2Features(0).
        buf[0x7c] = 0xFF;
        let h = parse_nsf(&buf).unwrap();
        assert_eq!(h.nsf2, Nsf2Features(0));
    }

    #[test]
    fn nsf2_splits_program_from_appended_metadata() {
        let program: Vec<u8> = (0..16).collect();
        let metadata: Vec<u8> = b"\x00\x00\x00\x00NEND".to_vec();
        let bytes = fake_v2(0x00, &program, &metadata);
        let h = parse_nsf(&bytes).unwrap();
        assert_eq!(h.program, program);
        assert_eq!(h.nsf2_metadata, metadata);
    }

    #[test]
    fn nsf2_rejects_overdeclared_data_length() {
        // Declare 0x123456 bytes but provide only 2.
        let mut buf = fake_v2(0x00, &[0xea, 0x60], &[]);
        buf[0x7d] = 0x56;
        buf[0x7e] = 0x34;
        buf[0x7f] = 0x12;
        let err = parse_nsf(&buf).unwrap_err();
        match err {
            NsfError::Nsf2DataLengthOverflow { declared, .. } => {
                assert_eq!(declared, 0x123456);
            }
            other => panic!("expected Nsf2DataLengthOverflow, got {other:?}"),
        }
    }

    #[test]
    fn nsf2_features_bit_helpers_cover_individual_bits() {
        assert!(Nsf2Features(0x10).irq_support());
        assert!(!Nsf2Features(0x10).non_returning_init());
        assert!(Nsf2Features(0x20).non_returning_init());
        assert!(Nsf2Features(0x40).suppressed_play());
        assert!(Nsf2Features(0x80).mandatory_metadata());
        // Reserved low nibble: should not influence any helper.
        assert!(!Nsf2Features(0x0F).irq_support());
        assert!(!Nsf2Features(0x0F).non_returning_init());
        assert!(!Nsf2Features(0x0F).suppressed_play());
        assert!(!Nsf2Features(0x0F).mandatory_metadata());
        assert!(!Nsf2Features(0x0F).needs_vector_overlay());
    }

    /// Builds a v1 NSF whose program data starts with the given
    /// 8-byte NSFDRV tag (6-byte ID + major + minor), followed by a
    /// couple of code bytes.
    fn fake_v1_with_nsfdrv(id: &[u8; 6], major: u8, minor: u8) -> Vec<u8> {
        let mut buf = fake_v1();
        buf.truncate(NSF_HEADER_LEN);
        buf.extend_from_slice(id);
        buf.push(major);
        buf.push(minor);
        buf.extend_from_slice(&[0xea, 0x60]);
        buf
    }

    #[test]
    fn nsfdrv_registered_ids_match_documented_binary_forms() {
        // §"List of NSFDRV sound driver IDs": ASCII tags and their
        // documented binary byte sequences must agree.
        assert_eq!(b"OFGS  ", &[0x4f, 0x46, 0x47, 0x53, 0x20, 0x20]);
        assert_eq!(b"FTDRV ", &[0x46, 0x54, 0x44, 0x52, 0x56, 0x20]);
        assert_eq!(b"NSDL  ", &[0x4e, 0x53, 0x44, 0x4c, 0x20, 0x20]);
        assert_eq!(b"      ", &[0x20, 0x20, 0x20, 0x20, 0x20, 0x20]);
    }

    #[test]
    fn nsfdrv_detects_each_registered_id_through_parse_nsf() {
        for (id, expected) in [
            (b"OFGS  ", NsfDrvId::Ofgs),
            (b"FTDRV ", NsfDrvId::Ftdrv),
            (b"NSDL  ", NsfDrvId::Nsdl),
            (b"      ", NsfDrvId::Blank),
        ] {
            let h = parse_nsf(&fake_v1_with_nsfdrv(id, 2, 11)).unwrap();
            let tag = h.nsfdrv().expect("registered ID must be detected");
            assert_eq!(&tag.id, id);
            assert_eq!(tag.known_id(), Some(expected));
            // File layout: ID at $0080-$0085, major at $0086, minor
            // at $0087 — i.e. program bytes 6 + 7.
            assert_eq!(tag.major, 2);
            assert_eq!(tag.minor, 11);
        }
    }

    #[test]
    fn nsfdrv_id_ascii_roundtrips_registered_ids() {
        let tag = NsfDrvTag::read(b"FTDRV \x01\x02").unwrap();
        assert_eq!(tag.id_ascii(), Some("FTDRV "));
        assert_eq!(tag.major, 1);
        assert_eq!(tag.minor, 2);
        // Non-printable ID bytes -> no ASCII rendering.
        let raw = NsfDrvTag::read(&[0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07]).unwrap();
        assert_eq!(raw.id_ascii(), None);
    }

    #[test]
    fn nsfdrv_unregistered_program_start_is_plain_code() {
        // fake_v1's program is 4 bytes (shorter than the 8-byte tag).
        let h = parse_nsf(&fake_v1()).unwrap();
        assert_eq!(h.nsfdrv(), None);
        assert_eq!(NsfDrvTag::read(&h.program), None);

        // 8+ bytes that don't match any registered ID: the raw read
        // succeeds but classification + header-level detection fail.
        let h = parse_nsf(&fake_v1_with_nsfdrv(b"XYZZY ", 1, 0)).unwrap();
        assert_eq!(h.nsfdrv(), None);
        let raw = NsfDrvTag::read(&h.program).unwrap();
        assert_eq!(raw.known_id(), None);
        assert_eq!(raw.id_ascii(), Some("XYZZY "));
    }

    #[test]
    fn nsfdrv_read_from_nsfe_data_chunk_program() {
        let mut out = Vec::new();
        out.extend_from_slice(&NSFE_MAGIC);
        let info_payload: [u8; 10] = [0x00, 0x80, 0x03, 0x80, 0x06, 0x80, 0x00, 0x00, 1, 0];
        out.extend_from_slice(&(info_payload.len() as u32).to_le_bytes());
        out.extend_from_slice(b"INFO");
        out.extend_from_slice(&info_payload);

        let mut data = Vec::new();
        data.extend_from_slice(b"NSDL  ");
        data.extend_from_slice(&[3, 7, 0xea, 0x60]);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"DATA");
        out.extend_from_slice(&data);

        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"NEND");

        let h = parse_nsf(&out).unwrap();
        let tag = h.nsfdrv().unwrap();
        assert_eq!(tag.known_id(), Some(NsfDrvId::Nsdl));
        assert_eq!(tag.major, 3);
        assert_eq!(tag.minor, 7);
    }
}
