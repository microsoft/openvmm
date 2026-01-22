// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test-only cryptographic implementations used by the IGVM agent tests.
//!
//! NOTE: This is a test implementation and should not be used in production.
//! The cryptographic crates (`rsa`) and implementations (`sha1`, `aes-ecb`, `aes-key-wrap`)
//! are not vetted for production use and are *exclusively* for this test module on the
//! Windows platform.

use rsa::rand_core::CryptoRng;
use rsa::rand_core::RngCore;
use rsa::rand_core::SeedableRng;
use sha2::digest;
use sha2::digest::consts::U20;
use sha2::digest::consts::U64;
use sha2::digest::core_api::BlockSizeUser;

/// Minimal, non-constant-time SHA-1 implementation sufficient to satisfy the
/// `digest::Digest` trait required by `rsa::Oaep`. Do NOT use in production.
#[derive(Clone)]
pub(crate) struct TestSha1 {
    state: [u32; 5],
    buf: [u8; 64],
    buf_len: usize,
    length_bits: u64, // total length processed (in bits)
}

impl TestSha1 {
    fn new_inner() -> Self {
        Self {
            state: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            buf: [0; 64],
            buf_len: 0,
            length_bits: 0,
        }
    }

    fn process_block(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 80];

        for (i, chunk) in block.chunks(4).take(16).enumerate() {
            w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let mut a = self.state[0];
        let mut b = self.state[1];
        let mut c = self.state[2];
        let mut d = self.state[3];
        let mut e = self.state[4];

        for (i, &w_i) in w.iter().enumerate() {
            // 0..80
            let (f, k) = match i {
                0..=19 => (((b & c) | ((!b) & d)), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => (((b & c) | (b & d) | (c & d)), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w_i);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
    }

    fn finalize_inner(mut self) -> [u8; 20] {
        // Append 0x80
        self.buf[self.buf_len] = 0x80;
        self.buf_len += 1;

        // If not enough space for length (8 bytes), pad with zeros and process
        if self.buf_len > 56 {
            for b in &mut self.buf[self.buf_len..] {
                *b = 0;
            }
            let block = self.buf;
            self.process_block(&block);
            self.buf = [0u8; 64];
            self.buf_len = 0;
        }

        // Pad zeros until 56
        for b in &mut self.buf[self.buf_len..56] {
            *b = 0;
        }

        // Append length (before padding) in bits big-endian
        let len_bytes = self.length_bits.to_be_bytes();
        self.buf[56..64].copy_from_slice(&len_bytes);
        let final_block = self.buf;
        self.process_block(&final_block);

        // Produce digest
        let mut out = [0u8; 20];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }

        out
    }
}

/// Implement the digest::Digest trait set.
impl digest::OutputSizeUser for TestSha1 {
    type OutputSize = U20;
}

impl BlockSizeUser for TestSha1 {
    type BlockSize = U64;
}

impl digest::Reset for TestSha1 {
    fn reset(&mut self) {
        *self = TestSha1::new_inner();
    }
}

impl digest::Update for TestSha1 {
    fn update(&mut self, data: &[u8]) {
        let mut input = data;

        while !input.is_empty() {
            let take = core::cmp::min(64 - self.buf_len, input.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&input[..take]);
            self.buf_len += take;
            self.length_bits = self.length_bits.wrapping_add((take as u64) * 8);
            input = &input[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.process_block(&block);
                self.buf_len = 0;
            }
        }
    }
}

impl digest::FixedOutput for TestSha1 {
    fn finalize_into(self, out: &mut digest::Output<Self>) {
        let digest = self.finalize_inner();
        out.copy_from_slice(&digest);
    }
}

impl digest::FixedOutputReset for TestSha1 {
    fn finalize_into_reset(&mut self, out: &mut digest::Output<Self>) {
        let digest = self.clone().finalize_inner();
        out.copy_from_slice(&digest);
        <Self as digest::Reset>::reset(self);
    }
}

impl digest::HashMarker for TestSha1 {}

impl Default for TestSha1 {
    fn default() -> Self {
        TestSha1::new_inner()
    }
}

/// Simplified, test-only implementation of AES Key Wrap with Padding based on RFC 5649 (wrap only, no unwrap).
pub(crate) fn aes_key_wrap_with_padding(kek: &[u8; 32], key_data: &[u8]) -> Vec<u8> {
    // Pad key_data to 8-byte multiple with zeros, record original length
    let mli = key_data.len() as u32;
    let mut padded = key_data.to_vec();
    if !padded.len().is_multiple_of(8) {
        padded.resize(padded.len().div_ceil(8) * 8, 0);
    }

    let n = padded.len() / 8; // number of 64-bit blocks
    let mut a = {
        let mut v = [0u8; 8];
        v[0..4].copy_from_slice(&[0xA6, 0x59, 0x59, 0xA6]);
        v[4..8].copy_from_slice(&mli.to_be_bytes());
        v
    };
    let cipher = Aes256::new(kek);
    let mut r: Vec<[u8; 8]> = padded
        .chunks(8)
        .map(|c| {
            let mut b = [0u8; 8];
            b.copy_from_slice(c);
            b
        })
        .collect();

    if n == 1 {
        // single-block special case
        let mut block = [0u8; 16];

        block[..8].copy_from_slice(&a);
        block[8..].copy_from_slice(&r[0]);
        cipher.encrypt_block(&mut block);
        a.copy_from_slice(&block[..8]);
        r[0].copy_from_slice(&block[8..]);
        let mut out = Vec::with_capacity(16);
        out.extend_from_slice(&a);
        out.extend_from_slice(&r[0]);

        return out;
    }

    for j in 0..6 {
        // 6 rounds like RFC3394
        for (i, blk) in r.iter_mut().enumerate() {
            let mut block = [0u8; 16];
            block[..8].copy_from_slice(&a);
            block[8..].copy_from_slice(blk);
            cipher.encrypt_block(&mut block);
            let t = (j * n + (i + 1)) as u64; // XOR round counter after encryption
            let mut a_tmp = [0u8; 8];
            a_tmp.copy_from_slice(&block[..8]);
            let a_num = u64::from_be_bytes(a_tmp) ^ t;
            a = a_num.to_be_bytes();
            blk.copy_from_slice(&block[8..]);
        }
    }

    let mut out = Vec::with_capacity((n + 1) * 8);
    out.extend_from_slice(&a);
    for blk in r {
        out.extend_from_slice(&blk);
    }

    out
}

/// Minimal, test-only implementation of AES-256 for ECB mode.
/// Do NOT use this code in production or security sensitive contexts.
#[derive(Clone)]
pub(crate) struct Aes256 {
    // 15 round keys * 16 bytes = 240 bytes (Nr = 14, plus initial round key)
    round_keys: [u8; 240],
}

impl Aes256 {
    pub(crate) fn new(key: &[u8; 32]) -> Self {
        let mut w = [0u32; 60]; // 60 words (4 bytes) -> 240 bytes

        // Load initial key
        for (word_index, chunk) in key.chunks_exact(4).enumerate() {
            let bytes: [u8; 4] = chunk.try_into().expect("chunk size is always 4");
            w[word_index] = u32::from_be_bytes(bytes);
        }

        let mut i = 8; // Nk = 8
        let nr = 14; // AES-256
        let total_words = 4 * (nr + 1); // 60
        while i < total_words {
            let mut temp = w[i - 1];
            if i % 8 == 0 {
                temp = sub_word(rot_word(temp)) ^ (RCON[(i / 8) - 1] as u32) << 24;
            } else if i % 8 == 4 {
                temp = sub_word(temp);
            }
            w[i] = w[i - 8] ^ temp;
            i += 1;
        }

        // Flatten into bytes
        let mut round_keys = [0u8; 240];
        for (wi, word) in w.iter().enumerate() {
            let b = word.to_be_bytes();
            round_keys[wi * 4..wi * 4 + 4].copy_from_slice(&b);
        }

        Self { round_keys }
    }

    pub(crate) fn encrypt_block(&self, block: &mut [u8; 16]) {
        let nr = 14;

        add_round_key(block, &self.round_keys[0..16]);
        for round in 1..nr {
            // rounds 1..13
            sub_bytes(block);
            shift_rows(block);
            mix_columns(block);
            let rk_start = round * 16;
            add_round_key(block, &self.round_keys[rk_start..rk_start + 16]);
        }

        // Final round (no MixColumns)
        sub_bytes(block);
        shift_rows(block);
        add_round_key(block, &self.round_keys[nr * 16..nr * 16 + 16]);
    }
}

// --- AES Primitives ---

const S_BOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

/// Round constants (we only need first 7 for AES-256 key expansion: (Nr=14 => i/8 up to 7)).
const RCON: [u8; 7] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40];

#[inline]
fn rot_word(w: u32) -> u32 {
    w.rotate_left(8)
}

#[inline]
fn sub_word(w: u32) -> u32 {
    let b0 = S_BOX[((w >> 24) & 0xff) as usize] as u32;
    let b1 = S_BOX[((w >> 16) & 0xff) as usize] as u32;
    let b2 = S_BOX[((w >> 8) & 0xff) as usize] as u32;
    let b3 = S_BOX[(w & 0xff) as usize] as u32;
    (b0 << 24) | (b1 << 16) | (b2 << 8) | b3
}

fn add_round_key(state: &mut [u8; 16], rk: &[u8]) {
    for i in 0..16 {
        state[i] ^= rk[i];
    }
}

fn sub_bytes(state: &mut [u8; 16]) {
    for b in state {
        *b = S_BOX[*b as usize];
    }
}

fn shift_rows(state: &mut [u8; 16]) {
    // Row 1: positions 1,5,9,13
    let r1 = [state[1], state[5], state[9], state[13]];
    state[1] = r1[1];
    state[5] = r1[2];
    state[9] = r1[3];
    state[13] = r1[0];
    // Row 2: positions 2,6,10,14
    let r2 = [state[2], state[6], state[10], state[14]];
    state[2] = r2[2];
    state[6] = r2[3];
    state[10] = r2[0];
    state[14] = r2[1];
    // Row 3: positions 3,7,11,15
    let r3 = [state[3], state[7], state[11], state[15]];
    state[3] = r3[3];
    state[7] = r3[0];
    state[11] = r3[1];
    state[15] = r3[2];
}

#[inline]
fn xtime(x: u8) -> u8 {
    (x << 1) ^ (((x >> 7) & 1) * 0x1b)
}

fn mix_columns(state: &mut [u8; 16]) {
    for c in 0..4 {
        let i = 4 * c;
        let a0 = state[i];
        let a1 = state[i + 1];
        let a2 = state[i + 2];
        let a3 = state[i + 3];
        let t = a0 ^ a1 ^ a2 ^ a3;
        let u = a0; // save for last computation
        state[i] ^= t ^ xtime(a0 ^ a1);
        state[i + 1] ^= t ^ xtime(a1 ^ a2);
        state[i + 2] ^= t ^ xtime(a2 ^ a3);
        state[i + 3] ^= t ^ xtime(a3 ^ u);
    }
}

/// A simple deterministic RNG used only for testing (not cryptographically secure).
///
/// This avoids the high cost of using `OsRng` during RSA key generation,
/// making `initialize_keys` run faster and with consistent timing across test runs.
/// In contrast, `OsRng` can introduce significant variability and may cause
/// tests to run slowly or even hit the default 5-second timeouts.
pub struct DummyRng {
    state: u64,
}

impl SeedableRng for DummyRng {
    type Seed = [u8; 8]; // 64-bit seed

    fn from_seed(seed: Self::Seed) -> Self {
        DummyRng {
            state: u64::from_le_bytes(seed),
        }
    }
}

impl RngCore for DummyRng {
    fn next_u32(&mut self) -> u32 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1);
        (self.state >> 32) as u32
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.state
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(8) {
            let n = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&n[..chunk.len()]);
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rsa::rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

/// Marker trait to satisfy `rsa::RsaPrivateKey::new`.
impl CryptoRng for DummyRng {}

#[cfg(test)]
mod tests {
    use super::Aes256;
    use super::TestSha1;
    use super::aes_key_wrap_with_padding;
    use sha2::digest::Digest;

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }

    #[test]
    fn sha1_empty() {
        let out = TestSha1::digest(b"");
        assert_eq!(to_hex(&out), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn sha1_abc() {
        let out = TestSha1::digest(b"abc");
        assert_eq!(to_hex(&out), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn wrap_basic_len() {
        let kek = [0x11u8; 32];
        let key = b"EXAMPLE KEY MATERIAL"; // length 20 -> padded to 24 -> 3 blocks, output 32 bytes
        let wrapped = aes_key_wrap_with_padding(&kek, key);
        assert_eq!(wrapped.len(), 32);
    }

    #[test]
    fn aes256_ecb_single_block_vector() {
        // NIST SP 800-38A F.5 AES-256 ECB – first block
        let key = [
            0x60, 0x3d, 0xeb, 0x10, 0x15, 0xca, 0x71, 0xbe, 0x2b, 0x73, 0xae, 0xf0, 0x85, 0x7d,
            0x77, 0x81, 0x1f, 0x35, 0x2c, 0x07, 0x3b, 0x61, 0x08, 0xd7, 0x2d, 0x98, 0x10, 0xa3,
            0x09, 0x14, 0xdf, 0xf4,
        ];
        let mut block = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a,
        ];
        let cipher = Aes256::new(&key);
        cipher.encrypt_block(&mut block);
        let expected = [
            0xf3, 0xee, 0xd1, 0xbd, 0xb5, 0xd2, 0xa0, 0x3c, 0x06, 0x4b, 0x5a, 0x7e, 0x3d, 0xb1,
            0x81, 0xf8,
        ];
        assert_eq!(block, expected, "AES-256 ECB test vector mismatch");
    }
}
