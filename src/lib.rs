//! NSF (Nintendo Sound Format) playback support.
//!
//! NSF is the de-facto container for music ripped out of NES games. A
//! file ships a 128-byte header (or, in the NSFe variant, a chunk-based
//! header with the `NSFE` magic) followed by a blob of raw 6502 machine
//! code that the player loads at the header's `load_addr`.
//!
//! Implementation is derived strictly from the public nesdev.org wiki
//! (NSF, APU, CPU, CPU_unofficial_opcodes pages) plus our own analysis.

#![forbid(unsafe_code)]

pub mod apu;
pub mod bus;
pub mod cpu;
pub mod expansion;
pub mod header;
pub mod nsfe;
pub mod opll;
pub mod player;

#[cfg(feature = "registry")]
mod registry;

pub use apu::Apu2A03;
pub use bus::NesBus;
pub use cpu::Cpu6502;
pub use header::{parse_nsf, ExpansionChips, Nsf2Features, NsfError, NsfHeader, NsfRegion};
pub use nsfe::{
    NsfeAuth, NsfeMetaError, NsfeMetadata, NsfeMixerEntry, NsfeRate, NsfeRegions, NsfeVrc7,
};
pub use player::NsfPlayer;

#[cfg(feature = "registry")]
pub use registry::{register_codecs, register_containers, CODEC_ID_STR};

/// Output sample rate that [`NsfPlayer::render`] resamples to.
pub const OUTPUT_SAMPLE_RATE: u32 = 44_100;
