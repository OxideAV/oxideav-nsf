//! NSF / NSFe header parser.
//!
//! Two on-disk shapes are accepted:
//!
//! * **NSF v1.x** — magic `b"NESM\x1a"`, fixed 128-byte header, then the
//!   raw 6502 code blob loaded at `load_addr`.
//! * **NSFe** — magic `b"NSFE"`, then a chain of 4-byte-length /
//!   4-byte-fourCC / payload chunks. `INFO` and `DATA` are required;
//!   `auth` (author / title / copyright / ripper), `tlbl` (per-track
//!   labels) and many optional chunks may follow. Unknown chunks whose
//!   first letter is uppercase are treated as mandatory and rejected.

use core::fmt;

/// NSF v1 magic — `NESM\x1a`.
pub const NSF_MAGIC: [u8; 5] = *b"NESM\x1a";

/// NSFe extension magic — `NSFE`.
pub const NSFE_MAGIC: [u8; 4] = *b"NSFE";

/// Fixed header size for the v1.x format.
pub const NSF_HEADER_LEN: usize = 0x80;

/// Region indicator (the low two bits of the v1 region byte).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NsfRegion {
    Ntsc,
    Pal,
    Dual,
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
}

impl NsfHeader {
    /// Returns the playback rate in Hz for the chosen region.
    pub fn play_rate_hz(&self) -> f64 {
        let us = match self.region {
            NsfRegion::Pal => self.pal_speed_us,
            NsfRegion::Ntsc | NsfRegion::Dual => self.ntsc_speed_us,
        };
        if us == 0 {
            match self.region {
                NsfRegion::Pal => 50.006,
                NsfRegion::Ntsc | NsfRegion::Dual => 60.0024,
            }
        } else {
            1_000_000.0 / us as f64
        }
    }

    pub fn has_expansion(&self) -> bool {
        self.expansion.0 != 0
    }
}

/// Failures from the on-disk header parser.
#[derive(Debug, PartialEq, Eq)]
pub enum NsfError {
    TooShort { needed: usize, got: usize },
    BadMagic,
    BadVersion(u8),
    NoSongs,
    NsfeTruncatedChunk,
    NsfeChunkOverflow,
    NsfeMissingRequired(&'static str),
    NsfeUnknownMandatory([u8; 4]),
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
            NsfError::NsfeUnknownMandatory(fcc) => write!(
                f,
                "NSFe: unknown mandatory chunk {:?}",
                core::str::from_utf8(fcc).unwrap_or("????")
            ),
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
    let program = bytes[NSF_HEADER_LEN..].to_vec();

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
        track_labels: Vec::new(),
        is_nsfe: false,
    })
}

fn parse_nsfe(bytes: &[u8]) -> Result<NsfHeader, NsfError> {
    let mut info: Option<NsfeInfo> = None;
    let mut data: Option<Vec<u8>> = None;
    let mut song_name = String::new();
    let mut artist = String::new();
    let mut copyright = String::new();
    let mut track_labels: Vec<String> = Vec::new();

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
            b"DATA" => data = Some(body.to_vec()),
            b"auth" => {
                let mut it = body.split(|&b| b == 0);
                song_name = it.next().map(read_nsf_string).unwrap_or_default();
                artist = it.next().map(read_nsf_string).unwrap_or_default();
                copyright = it.next().map(read_nsf_string).unwrap_or_default();
            }
            b"tlbl" => {
                track_labels = body
                    .split(|&b| b == 0)
                    .filter(|s| !s.is_empty())
                    .map(read_nsf_string)
                    .collect();
            }
            b"NEND" => break,
            _ => {
                let first = fcc[0];
                if first.is_ascii_uppercase() {
                    return Err(NsfError::NsfeUnknownMandatory(fcc));
                }
            }
        }
        cursor = body_end;
    }

    let info = info.ok_or(NsfError::NsfeMissingRequired("INFO"))?;
    let program = data.ok_or(NsfError::NsfeMissingRequired("DATA"))?;

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
        ntsc_speed_us: 0,
        pal_speed_us: 0,
        bankswitch_init: [0u8; 8],
        region: NsfRegion::from_byte(info.region),
        expansion: ExpansionChips(info.expansion),
        program,
        track_labels,
        is_nsfe: true,
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
    if body.len() < 8 {
        return Err(NsfError::NsfeTruncatedChunk);
    }
    Ok(NsfeInfo {
        load_addr: u16::from_le_bytes([body[0], body[1]]),
        init_addr: u16::from_le_bytes([body[2], body[3]]),
        play_addr: u16::from_le_bytes([body[4], body[5]]),
        region: body[6],
        expansion: body[7],
        total_songs: body.get(8).copied().unwrap_or(1),
        starting_song: body.get(9).copied().unwrap_or(0),
    })
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

    #[test]
    fn nsfe_rejects_unknown_mandatory_chunk() {
        let mut out = Vec::new();
        out.extend_from_slice(&NSFE_MAGIC);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"ZZZZ");
        let err = parse_nsf(&out).unwrap_err();
        assert!(matches!(err, NsfError::NsfeUnknownMandatory(_)));
    }
}
