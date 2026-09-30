//! Qobuz CMAF streams (`file/url`), as the web player plays them.
//!
//! - `session/start` (profile `qbz-1`) answers `infos` = `<salt>.<info>`
//!   (base64url). The session key is HKDF-SHA256 of the request-signing
//!   secret (its 16 bytes, from hex) with that salt and info: 16 bytes.
//! - `file/url` answers `key` = `qbz-1.<wrapped>.<iv>` (base64url): the
//!   content key is `wrapped` decrypted with the session key in AES-128-CBC
//!   (PKCS#7 padding).
//! - `url_template` with `$SEGMENT$` = 0 is the init segment, 1 to
//!   `n_segments` the audio. Every file is a list of MP4 boxes; Qobuz puts
//!   its own data in `uuid` boxes:
//!   - init (`c7c75df0…`): the FLAC header (`fLaC`, STREAMINFO…) and the
//!     table of segments (audio bytes and samples of each);
//!   - segment (`3b421292…`): the frames and, for each, whether it is
//!     encrypted and its IV. An encrypted frame is AES-128-CTR with the
//!     content key, counter = IV followed by zeros.
//!
//! The FLAC header followed by every segment's frames, decrypted, is the
//! original FLAC file.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit, StreamCipher};
use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::Engine;

const INIT_UUID: [u8; 16] = hex16("c7c75df0fdd951e98fc22971e4acf8d2");
const SEGMENT_UUID: [u8; 16] = hex16("3b42129256f35f75923663b69a1f52b2");

const fn hex16(s: &str) -> [u8; 16] {
    let b = s.as_bytes();
    let mut out = [0u8; 16];
    let mut i = 0;
    while i < 16 {
        out[i] = (nibble(b[2 * i]) << 4) | nibble(b[2 * i + 1]);
        i += 1;
    }
    out
}

const fn nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => panic!("not hex"),
    }
}

fn b64url(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .context("base64url")
}

/// Session key from `session/start`'s `infos` and the signing secret (hex).
pub fn session_key(secret_hex: &str, infos: &str) -> Result<[u8; 16]> {
    let (salt, info) = infos.split_once('.').ok_or_else(|| anyhow!("infos without a dot"))?;
    let (salt, info) = (b64url(salt)?, b64url(info)?);
    let ikm: Vec<u8> = (0..secret_hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(secret_hex.get(i..i + 2).unwrap_or("x"), 16))
        .collect::<Result<_, _>>()
        .context("secret is not hex")?;
    let mut key = [0u8; 16];
    hkdf::Hkdf::<sha2::Sha256>::new(Some(&salt), &ikm)
        .expand(&info, &mut key)
        .map_err(|_| anyhow!("hkdf"))?;
    Ok(key)
}

/// Content key from `file/url`'s `key` (`qbz-1.<wrapped>.<iv>`).
pub fn content_key(session_key: &[u8; 16], key: &str) -> Result<[u8; 16]> {
    let mut parts = key.split('.');
    let (Some(scheme), Some(wrapped), Some(iv), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        bail!("key is not <scheme>.<wrapped>.<iv>");
    };
    ensure!(scheme == "qbz-1", "unknown key scheme {scheme:?}");
    let mut wrapped = b64url(wrapped)?;
    let iv: [u8; 16] = b64url(iv)?.try_into().map_err(|_| anyhow!("iv is not 16 bytes"))?;
    let plain = cbc::Decryptor::<aes::Aes128>::new(session_key.into(), &iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut wrapped)
        .map_err(|_| anyhow!("key unwrap: bad padding"))?;
    plain.try_into().map_err(|_| anyhow!("content key is not 16 bytes"))
}

/// One audio segment, as the init segment describes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentInfo {
    /// Bytes of FLAC frames it holds.
    pub audio_bytes: u32,
    pub samples: u32,
}

/// What the init segment says.
#[derive(Debug, Clone, PartialEq)]
pub struct Init {
    pub sample_rate: u32,
    pub bits: u8,
    pub channels: u8,
    pub samples: u64,
    /// `fLaC` and the metadata blocks.
    pub flac_header: Vec<u8>,
    pub segments: Vec<SegmentInfo>,
}

impl Init {
    /// Size of the whole FLAC file.
    pub fn file_size(&self) -> u64 {
        self.flac_header.len() as u64 + self.segments.iter().map(|s| u64::from(s.audio_bytes)).sum::<u64>()
    }

    /// Byte offset of each segment's first frame in the FLAC file.
    pub fn offsets(&self) -> Vec<u64> {
        let mut at = self.flac_header.len() as u64;
        self.segments
            .iter()
            .map(|s| {
                let start = at;
                at += u64::from(s.audio_bytes);
                start
            })
            .collect()
    }
}

/// Bounds-checked big-endian reader.
struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.b.len()).ok_or_else(|| anyhow!("truncated box"))?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn uint(&mut self, n: usize) -> Result<u64> {
        Ok(self.take(n)?.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b)))
    }
}

/// `(offset, size)` of the `uuid` box with this uuid.
fn find_box(b: &[u8], uuid: &[u8; 16]) -> Result<(usize, usize)> {
    let mut at = 0usize;
    while at + 8 <= b.len() {
        let size = u32::from_be_bytes(b[at..at + 4].try_into().expect("4 bytes")) as usize;
        ensure!(size >= 8 && at + size <= b.len(), "bad box size at {at}");
        if &b[at + 4..at + 8] == b"uuid" && b.get(at + 8..at + 24) == Some(uuid.as_slice()) {
            return Ok((at, size));
        }
        at += size;
    }
    bail!("no Qobuz box")
}

pub fn parse_init(b: &[u8]) -> Result<Init> {
    let (at, size) = find_box(b, &INIT_UUID)?;
    // Box header, uuid, version and flags: 28 bytes.
    let mut r = Reader { b: &b[..at + size], at: at + 28 };
    let _track_id = r.uint(4)?;
    let _file_id = r.uint(4)?;
    let sample_rate = r.uint(4)? as u32;
    let bits = r.uint(1)? as u8;
    let channels = r.uint(1)? as u8;
    r.take(2)?;
    let samples = r.uint(6)?;
    let header_len = r.uint(2)? as usize;
    let flac_header = r.take(header_len)?.to_vec();
    ensure!(flac_header.starts_with(b"fLaC"), "init: no FLAC header");
    let key_id_len = r.uint(1)? as usize;
    r.take(key_id_len)?;
    let count = r.uint(2)? as usize;
    let segments = (0..count)
        .map(|_| Ok(SegmentInfo { audio_bytes: r.uint(4)? as u32, samples: r.uint(4)? as u32 }))
        .collect::<Result<Vec<_>>>()?;
    ensure!(!segments.is_empty(), "init: no segments");
    Ok(Init { sample_rate, bits, channels, samples, flac_header, segments })
}

/// The FLAC frames of an audio segment, decrypted.
pub fn decrypt_segment(b: &[u8], key: &[u8; 16]) -> Result<Vec<u8>> {
    let (at, size) = find_box(b, &SEGMENT_UUID)?;
    let mut r = Reader { b: &b[..at + size], at: at + 28 };
    let data_offset = r.uint(4)? as usize;
    let iv_len = r.uint(1)? as usize;
    ensure!(iv_len <= 16, "segment: iv of {iv_len} bytes");
    let frames = r.uint(3)? as usize;
    let mut pos = at.checked_add(data_offset).ok_or_else(|| anyhow!("segment: bad data offset"))?;
    let mut out = Vec::new();
    for _ in 0..frames {
        let len = r.uint(4)? as usize;
        r.take(2)?;
        let flags = r.uint(2)?;
        let iv = r.take(iv_len)?;
        let end = pos.checked_add(len).filter(|&e| e <= b.len()).ok_or_else(|| anyhow!("segment: frame past the end"))?;
        let start = out.len();
        out.extend_from_slice(&b[pos..end]);
        if flags != 0 {
            let mut counter = [0u8; 16];
            counter[..iv_len].copy_from_slice(iv);
            ctr::Ctr64BE::<aes::Aes128>::new(key.into(), &counter.into()).apply_keystream(&mut out[start..]);
        }
        pos = end;
    }
    Ok(out)
}

#[cfg(test)]
pub mod testing {
    //! Builds CMAF files the way Qobuz serves them, for tests.
    use super::*;
    use aes::cipher::BlockEncryptMut;

    fn boxed(kind: &[u8; 4], uuid: Option<&[u8; 16]>, body: &[u8]) -> Vec<u8> {
        let extra = if uuid.is_some() { 20 } else { 0 };
        let mut b = ((8 + extra + body.len()) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        if let Some(u) = uuid {
            b.extend_from_slice(u);
            b.extend_from_slice(&[0; 4]); // version and flags
        }
        b.extend_from_slice(body);
        b
    }

    pub fn init(header: &[u8], segments: &[(u32, u32)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&2u32.to_be_bytes());
        body.extend_from_slice(&44_100u32.to_be_bytes());
        body.extend_from_slice(&[16, 2, 0, 0]);
        body.extend_from_slice(&(segments.iter().map(|s| u64::from(s.1)).sum::<u64>().to_be_bytes()[2..]));
        body.extend_from_slice(&(header.len() as u16).to_be_bytes());
        body.extend_from_slice(header);
        body.extend_from_slice(&[2, 0xab, 0xcd]);
        body.extend_from_slice(&(segments.len() as u16).to_be_bytes());
        for (bytes, samples) in segments {
            body.extend_from_slice(&bytes.to_be_bytes());
            body.extend_from_slice(&samples.to_be_bytes());
        }
        let mut file = boxed(b"ftyp", None, b"iso6");
        file.extend(boxed(b"uuid", Some(&INIT_UUID), &body));
        file
    }

    /// A segment holding `frames`, every other one encrypted.
    pub fn segment(frames: &[&[u8]], key: &[u8; 16]) -> Vec<u8> {
        let iv_len = 8;
        let table_len = 28 + 4 + 1 + 3 + frames.len() * (8 + iv_len);
        let mut table = Vec::new();
        let mut data = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            let iv = [i as u8 + 1; 8];
            table.extend_from_slice(&(f.len() as u32).to_be_bytes());
            table.extend_from_slice(&[0, 0]);
            let encrypted = i % 2 == 0;
            table.extend_from_slice(&u16::from(encrypted).to_be_bytes());
            table.extend_from_slice(&iv);
            let mut f = f.to_vec();
            if encrypted {
                let mut counter = [0u8; 16];
                counter[..8].copy_from_slice(&iv);
                ctr::Ctr64BE::<aes::Aes128>::new(key.into(), &counter.into()).apply_keystream(&mut f);
            }
            data.extend(f);
        }
        let mut body = Vec::new();
        // Frames start right after this box and an 8-byte `mdat` header.
        body.extend_from_slice(&((table_len + 8) as u32).to_be_bytes());
        body.push(iv_len as u8);
        body.extend_from_slice(&(frames.len() as u32).to_be_bytes()[1..]);
        body.extend(table);
        let mut file = boxed(b"styp", None, b"msdh");
        let uuid_box = boxed(b"uuid", Some(&SEGMENT_UUID), &body);
        // `data_offset` counts from the start of the Qobuz box.
        file.extend(uuid_box);
        file.extend(boxed(b"mdat", None, &data));
        file
    }

    /// `key` as `file/url` hands it out, wrapped with `session_key`.
    pub fn wrapped_key(session_key: &[u8; 16], key: &[u8; 16]) -> String {
        let iv = [7u8; 16];
        let mut buf = [0u8; 32];
        buf[..16].copy_from_slice(key);
        let wrapped = cbc::Encryptor::<aes::Aes128>::new(session_key.into(), &iv.into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, 16)
            .expect("fits");
        let b = |x: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(x);
        format!("qbz-1.{}.{}", b(wrapped), b(&iv))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real files: `QCONNECT_CMAF_SAMPLE=<dir>` holding `init.mp4`,
    /// `segment1.mp4`, `key.hex` (content key), `infos.txt`, `key.txt` and
    /// `expected.flac` (header and the first segment's frames, decrypted by
    /// another implementation).
    #[test]
    #[ignore]
    fn real_sample_decrypts() {
        let dir = std::path::PathBuf::from(std::env::var("QCONNECT_CMAF_SAMPLE").expect("QCONNECT_CMAF_SAMPLE"));
        let read = |f: &str| std::fs::read(dir.join(f)).unwrap();
        let key_hex = String::from_utf8(read("key.hex")).unwrap();
        let key: Vec<u8> = (0..32).step_by(2).map(|i| u8::from_str_radix(&key_hex[i..i + 2], 16).unwrap()).collect();
        let key: [u8; 16] = key.try_into().unwrap();
        // `infos.txt` and `key.txt`: `session/start` and `file/url` answers,
        // with the built-in signing secret.
        let text = |f: &str| String::from_utf8(read(f)).unwrap();
        let session = session_key(crate::msgtype::api::APP_SECRET, text("infos.txt").trim()).unwrap();
        assert_eq!(content_key(&session, text("key.txt").trim()).unwrap(), key, "keys derived as the web player does");
        let init = parse_init(&read("init.mp4")).unwrap();
        let mut flac = init.flac_header.clone();
        flac.extend(decrypt_segment(&read("segment1.mp4"), &key).unwrap());
        assert_eq!(flac.len() as u64, init.offsets()[1], "first segment: as long as the table says");
        assert!(flac == read("expected.flac"));
    }

    #[test]
    fn keys_derive_and_unwrap() {
        let b = |x: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(x);
        let infos = format!("{}.{}", b(b"salt-salt"), b(b"info"));
        let sk = session_key("0123456789abcdef0123456789abcdef", &infos).unwrap();
        assert_eq!(sk, session_key("0123456789abcdef0123456789abcdef", &infos).unwrap());
        assert_ne!(sk, session_key("0123456789abcdef0123456789abcdee", &infos).unwrap());
        let content = [42u8; 16];
        assert_eq!(content_key(&sk, &testing::wrapped_key(&sk, &content)).unwrap(), content);
        assert!(content_key(&sk, "qbz-2.a.b").is_err());
        assert!(session_key("zz", &infos).is_err());
    }

    #[test]
    fn init_and_segments_rebuild_the_flac_file() {
        let key = [9u8; 16];
        let header = b"fLaC\x80\0\0\x22 streaminfo".to_vec();
        let seg1: [&[u8]; 3] = [b"\xff\xf8frame-one", b"\xff\xf8frame-two", b"\xff\xf8three"];
        let seg2: [&[u8]; 1] = [b"\xff\xf8last"];
        let len = |fs: &[&[u8]]| fs.iter().map(|f| f.len() as u32).sum::<u32>();
        let init = parse_init(&testing::init(&header, &[(len(&seg1), 4096), (len(&seg2), 1024)])).unwrap();
        assert_eq!((init.sample_rate, init.bits, init.channels, init.samples), (44_100, 16, 2, 5120));
        assert_eq!(init.flac_header, header);
        assert_eq!(init.offsets(), [header.len() as u64, header.len() as u64 + u64::from(len(&seg1))]);
        assert_eq!(init.file_size(), header.len() as u64 + u64::from(len(&seg1) + len(&seg2)));

        let plain = decrypt_segment(&testing::segment(&seg1, &key), &key).unwrap();
        assert_eq!(plain, seg1.concat());
        assert_eq!(decrypt_segment(&testing::segment(&seg2, &key), &key).unwrap(), seg2.concat());
        assert_ne!(decrypt_segment(&testing::segment(&seg1, &key), &[8; 16]).unwrap(), seg1.concat(), "wrong key");
        assert!(parse_init(b"junk").is_err());
        assert!(decrypt_segment(&testing::segment(&seg1, &key)[..60], &key).is_err(), "truncated");
    }
}
