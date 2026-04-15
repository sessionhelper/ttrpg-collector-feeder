//! Startup-time check on the `AUDIO_FILE`.
//!
//! The feeder relies on songbird's OPUS passthrough — raw OGG Opus pages
//! from the file are forwarded to Discord unchanged. Passthrough requires
//! the source to be OGG-Opus, 48 kHz. If the file is anything else
//! (WAV, MP3, OGG Vorbis, Opus at 44.1 kHz, …), songbird will still play it,
//! but via the decode → resample → encode path, which mangles long-form
//! audio. We log a WARN in that case rather than failing startup: the user
//! might have intentionally supplied a WAV for a quick debugging run.
//!
//! The check itself reads only the first 64 bytes — enough for the OggS
//! capture pattern, the OpusHead magic, and the sample-rate field. It does
//! not try to validate the full stream; a corrupt but superficially
//! OGG-Opus file will still pass this check and fail later inside songbird.
use std::path::Path;

use tracing::warn;

/// Lightweight classification of the audio file's container/codec.
#[derive(Debug, PartialEq, Eq)]
pub enum AudioFormat {
    /// OGG container, Opus codec, sample rate in Hz.
    OggOpus { sample_rate: u32 },
    /// OGG container but not Opus (probably Vorbis).
    OggOther,
    /// Something else — WAV, MP3, random bytes, empty file, etc.
    Unknown,
}

impl AudioFormat {
    pub fn is_passthrough_ideal(&self) -> bool {
        matches!(
            self,
            Self::OggOpus {
                sample_rate: 48_000
            }
        )
    }
}

/// Inspect the first bytes of `path`. Returns `Unknown` on any I/O error or
/// if the file is too short. Caller interprets.
pub fn detect_format(path: &Path) -> AudioFormat {
    let Ok(bytes) = std::fs::read(path) else {
        return AudioFormat::Unknown;
    };
    // OGG capture pattern: bytes[0..4] == "OggS".
    if bytes.len() < 64 || &bytes[..4] != b"OggS" {
        return AudioFormat::Unknown;
    }
    // The OpusHead magic lives in the first OGG page's payload. For a
    // well-formed Opus file that's at offset 28 (OGG page header is 27
    // bytes + 1 segment table byte for a single-segment first page).
    // Rather than parse the page header, scan the first 64 bytes for the
    // magic — cheap and tolerant of minor variations.
    let window = &bytes[..64.min(bytes.len())];
    let Some(head_off) = find_subslice(window, b"OpusHead") else {
        return AudioFormat::OggOther;
    };
    // OpusHead layout (RFC 7845 §5.1):
    //   0..8  : "OpusHead"
    //   8     : version (1)
    //   9     : channel count
    //  10..12 : pre-skip (u16 LE)
    //  12..16 : input sample rate (u32 LE)
    let sr_off = head_off + 12;
    if sr_off + 4 > bytes.len() {
        return AudioFormat::OggOther;
    }
    let sample_rate = u32::from_le_bytes([
        bytes[sr_off],
        bytes[sr_off + 1],
        bytes[sr_off + 2],
        bytes[sr_off + 3],
    ]);
    AudioFormat::OggOpus { sample_rate }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Verify the `AUDIO_FILE` exists. Panics with a clear message if it
/// doesn't. This is called from the binary's startup path; failure means
/// "this container is misconfigured" and the right response is to crash
/// loudly so the harness notices.
pub fn require_audio_file_exists(path: &Path) {
    if !path.exists() {
        panic!("AUDIO_FILE does not exist: {}", path.display());
    }
}

/// Run [`detect_format`] and log a WARN if the file is anything other than
/// ideal (OGG Opus @ 48 kHz). Does not fail. Called once at startup.
pub fn check_audio_file(path: &Path) {
    let fmt = detect_format(path);
    match fmt {
        AudioFormat::OggOpus {
            sample_rate: 48_000,
        } => {
            tracing::info!(
                file = %path.display(),
                "audio_file_ok: OGG Opus @ 48kHz — passthrough eligible"
            );
        }
        AudioFormat::OggOpus { sample_rate } => warn!(
            file = %path.display(),
            %sample_rate,
            "audio_file_non_ideal: OGG Opus but not 48kHz; songbird will resample"
        ),
        AudioFormat::OggOther => warn!(
            file = %path.display(),
            "audio_file_non_ideal: OGG container but not Opus; passthrough disabled"
        ),
        AudioFormat::Unknown => warn!(
            file = %path.display(),
            "audio_file_non_ideal: not OGG Opus; songbird will decode+reencode"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Minimal OGG+OpusHead blob. Not a playable stream — just enough to
    /// satisfy [`detect_format`].
    fn write_fake_ogg_opus(path: &Path, sample_rate: u32) {
        let mut f = std::fs::File::create(path).unwrap();
        // OGG page header (27 bytes), segment table (1 byte), then payload.
        let mut page = Vec::new();
        page.extend_from_slice(b"OggS");
        page.push(0); // version
        page.push(0x02); // header type (first page)
        page.extend_from_slice(&0u64.to_le_bytes()); // granule pos
        page.extend_from_slice(&0u32.to_le_bytes()); // serial
        page.extend_from_slice(&0u32.to_le_bytes()); // seq
        page.extend_from_slice(&0u32.to_le_bytes()); // checksum
        page.push(1); // page segments
        page.push(19); // segment length: OpusHead(8) + version(1) + ch(1) + preskip(2) + sr(4) + gain(2) + mapping(1) = 19
        // OpusHead payload
        page.extend_from_slice(b"OpusHead");
        page.push(1); // version
        page.push(2); // channels
        page.extend_from_slice(&0u16.to_le_bytes()); // preskip
        page.extend_from_slice(&sample_rate.to_le_bytes());
        page.extend_from_slice(&0u16.to_le_bytes()); // output gain
        page.push(0); // channel mapping family
        // Pad so file is at least 64 bytes.
        while page.len() < 64 {
            page.push(0);
        }
        f.write_all(&page).unwrap();
    }

    #[test]
    fn detects_ogg_opus_48khz() {
        let tmp = std::env::temp_dir().join("feeder_test_48k.ogg");
        write_fake_ogg_opus(&tmp, 48_000);
        assert_eq!(
            detect_format(&tmp),
            AudioFormat::OggOpus {
                sample_rate: 48_000
            }
        );
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn detects_ogg_opus_non_48khz() {
        let tmp = std::env::temp_dir().join("feeder_test_44k.ogg");
        write_fake_ogg_opus(&tmp, 44_100);
        assert!(matches!(
            detect_format(&tmp),
            AudioFormat::OggOpus {
                sample_rate: 44_100
            }
        ));
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn detects_unknown_for_non_ogg() {
        let tmp = std::env::temp_dir().join("feeder_test_wav.wav");
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(b"RIFF").unwrap();
        f.write_all(&[0u8; 60]).unwrap();
        assert_eq!(detect_format(&tmp), AudioFormat::Unknown);
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn detects_unknown_for_missing_file() {
        let path = std::env::temp_dir().join("feeder_test_does_not_exist_12345.ogg");
        assert_eq!(detect_format(&path), AudioFormat::Unknown);
    }

    #[test]
    #[should_panic(expected = "AUDIO_FILE does not exist")]
    fn require_audio_file_exists_panics_on_missing() {
        let path = std::env::temp_dir().join("feeder_test_definitely_missing_file_9f3a2b1c.ogg");
        // sanity: make sure it really is missing
        let _ = std::fs::remove_file(&path);
        require_audio_file_exists(&path);
    }

    #[test]
    fn require_audio_file_exists_accepts_existing() {
        let path = std::env::temp_dir().join("feeder_test_exists.bin");
        std::fs::write(&path, b"ok").unwrap();
        require_audio_file_exists(&path);
        let _ = std::fs::remove_file(path);
    }
}
