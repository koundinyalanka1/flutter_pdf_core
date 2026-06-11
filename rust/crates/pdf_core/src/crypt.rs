//! Milestone 10: the Standard security handler.
//!
//! Decryption: RC4 40/128-bit (R2–R4), AES-128 (R4/AESV2) and AES-256
//! (R5/R6/AESV3). Encryption: AES-256 (R6, PDF 2.0).

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecryptMut, BlockEncrypt, BlockEncryptMut, KeyInit, KeyIvInit};
use aes::cipher::block_padding::{NoPadding, Pkcs7};
use md5::{Digest as _, Md5};
use sha2::{Sha256, Sha384, Sha512};

use crate::document::PdfDocument;
use crate::error::{PdfError, Result};
use crate::object::{Dictionary, ObjectId, PdfObject};

const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08,
    0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut s: [u8; 256] = [0; 256];
    for (i, slot) in s.iter_mut().enumerate() {
        *slot = i as u8;
    }
    let mut j: u8 = 0;
    for i in 0..256 {
        j = j
            .wrapping_add(s[i])
            .wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let mut out = Vec::with_capacity(data.len());
    let (mut i, mut j) = (0u8, 0u8);
    for &byte in data {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        let k = s[(s[i as usize].wrapping_add(s[j as usize])) as usize];
        out.push(byte ^ k);
    }
    out
}

fn md5(parts: &[&[u8]]) -> [u8; 16] {
    let mut hasher = Md5::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn aes_cbc_decrypt_padded(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 16 || (data.len() - 16) % 16 != 0 || data.len() == 16 {
        // No IV or empty ciphertext: treat as empty payload.
        return Ok(Vec::new());
    }
    let (iv, ct) = data.split_at(16);
    let mut buf = ct.to_vec();
    let out = match key.len() {
        16 => cbc::Decryptor::<aes::Aes128>::new_from_slices(key, iv)
            .map_err(|e| PdfError::crypt(e.to_string()))?
            .decrypt_padded_mut::<Pkcs7>(&mut buf)
            .map_err(|e| PdfError::crypt(e.to_string()))?
            .to_vec(),
        32 => cbc::Decryptor::<aes::Aes256>::new_from_slices(key, iv)
            .map_err(|e| PdfError::crypt(e.to_string()))?
            .decrypt_padded_mut::<Pkcs7>(&mut buf)
            .map_err(|e| PdfError::crypt(e.to_string()))?
            .to_vec(),
        other => return Err(PdfError::crypt(format!("bad AES key length {other}"))),
    };
    Ok(out)
}

fn aes_cbc_encrypt_padded(key: &[u8], iv: &[u8; 16], data: &[u8]) -> Result<Vec<u8>> {
    let mut out = iv.to_vec();
    let ct = match key.len() {
        16 => cbc::Encryptor::<aes::Aes128>::new_from_slices(key, iv)
            .map_err(|e| PdfError::crypt(e.to_string()))?
            .encrypt_padded_vec_mut::<Pkcs7>(data),
        32 => cbc::Encryptor::<aes::Aes256>::new_from_slices(key, iv)
            .map_err(|e| PdfError::crypt(e.to_string()))?
            .encrypt_padded_vec_mut::<Pkcs7>(data),
        other => return Err(PdfError::crypt(format!("bad AES key length {other}"))),
    };
    out.extend_from_slice(&ct);
    Ok(out)
}

fn aes256_cbc_nopad(key: &[u8; 32], iv: &[u8; 16], data: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    let mut buf = data.to_vec();
    if buf.len() % 16 != 0 {
        return Err(PdfError::crypt("AES no-pad input not block aligned"));
    }
    if encrypt {
        cbc::Encryptor::<aes::Aes256>::new_from_slices(key, iv)
            .map_err(|e| PdfError::crypt(e.to_string()))?
            .encrypt_padded_mut::<NoPadding>(&mut buf, data.len())
            .map_err(|e| PdfError::crypt(e.to_string()))?;
    } else {
        cbc::Decryptor::<aes::Aes256>::new_from_slices(key, iv)
            .map_err(|e| PdfError::crypt(e.to_string()))?
            .decrypt_padded_mut::<NoPadding>(&mut buf)
            .map_err(|e| PdfError::crypt(e.to_string()))?;
    }
    Ok(buf)
}

fn aes128_cbc_nopad_encrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let mut buf = data.to_vec();
    let len = buf.len();
    cbc::Encryptor::<aes::Aes128>::new_from_slices(key, iv)
        .map_err(|e| PdfError::crypt(e.to_string()))?
        .encrypt_padded_mut::<NoPadding>(&mut buf, len)
        .map_err(|e| PdfError::crypt(e.to_string()))?;
    Ok(buf)
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf).map_err(|e| PdfError::crypt(e.to_string()))?;
    Ok(buf)
}

/// ISO 32000-2 algorithm 2.B — the iterated R6 password hash.
fn hash_2b(password: &[u8], salt: &[u8], udata: &[u8]) -> Result<[u8; 32]> {
    let pw = &password[..password.len().min(127)];
    let mut k: Vec<u8> = sha256(&[pw, salt, udata]).to_vec();
    let mut round = 0usize;
    loop {
        let chunk_len = pw.len() + k.len() + udata.len();
        let mut k1 = Vec::with_capacity(chunk_len * 64);
        for _ in 0..64 {
            k1.extend_from_slice(pw);
            k1.extend_from_slice(&k);
            k1.extend_from_slice(udata);
        }
        let e = aes128_cbc_nopad_encrypt(&k[0..16], &k[16..32], &k1)?;
        let modulo = e[0..16].iter().map(|&b| b as usize).sum::<usize>() % 3;
        k = match modulo {
            0 => Sha256::digest(&e).to_vec(),
            1 => Sha384::digest(&e).to_vec(),
            _ => Sha512::digest(&e).to_vec(),
        };
        round += 1;
        if round >= 64 && (*e.last().unwrap() as usize) <= round - 32 {
            break;
        }
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&k[0..32]);
    Ok(out)
}

fn string_bytes(object: &PdfObject) -> Option<Vec<u8>> {
    match object {
        PdfObject::LiteralString(b) | PdfObject::HexString(b) => Some(b.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Decryptor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Cipher {
    Identity,
    Rc4,
    Aes128,
    Aes256,
}

pub struct Decryptor {
    cipher: Cipher,
    file_key: Vec<u8>,
    revision: i64,
    encrypt_metadata: bool,
}

impl Decryptor {
    pub fn new(encrypt: &Dictionary, trailer: &Dictionary, password: &[u8]) -> Result<Self> {
        if encrypt.get("Filter").and_then(PdfObject::as_name) != Some("Standard") {
            return Err(PdfError::Unsupported("non-standard security handler"));
        }
        let v = encrypt.get("V").and_then(PdfObject::as_i64).unwrap_or(0);
        let r = encrypt.get("R").and_then(PdfObject::as_i64).unwrap_or(0);
        let o = encrypt
            .get("O")
            .and_then(string_bytes)
            .ok_or_else(|| PdfError::crypt("missing /O"))?;
        let u = encrypt
            .get("U")
            .and_then(string_bytes)
            .ok_or_else(|| PdfError::crypt("missing /U"))?;
        let p = encrypt.get("P").and_then(PdfObject::as_i64).unwrap_or(-1) as i32;
        let encrypt_metadata = match encrypt.get("EncryptMetadata") {
            Some(PdfObject::Bool(b)) => *b,
            _ => true,
        };
        let id0 = trailer
            .get("ID")
            .and_then(|o| match o {
                PdfObject::Array(items) => items.first().and_then(string_bytes),
                _ => None,
            })
            .unwrap_or_default();

        match v {
            1 | 2 | 4 => {
                let key_len = if v == 1 {
                    5
                } else {
                    (encrypt.get("Length").and_then(PdfObject::as_i64).unwrap_or(40) / 8)
                        .clamp(5, 16) as usize
                };
                let cipher = if v == 4 {
                    match crypt_filter_method(encrypt) {
                        Some("AESV2") => Cipher::Aes128,
                        Some("V2") | None => Cipher::Rc4,
                        Some("Identity") => Cipher::Identity,
                        Some(other) => {
                            return Err(PdfError::Crypt(format!(
                                "unsupported crypt filter {other}"
                            )))
                        }
                    }
                } else {
                    Cipher::Rc4
                };
                let key_len = if cipher == Cipher::Aes128 { 16 } else { key_len };
                let file_key = authenticate_legacy(
                    password,
                    &o,
                    &u,
                    p,
                    &id0,
                    r,
                    key_len,
                    encrypt_metadata,
                )?;
                Ok(Self {
                    cipher,
                    file_key,
                    revision: r,
                    encrypt_metadata,
                })
            }
            5 => {
                let oe = encrypt
                    .get("OE")
                    .and_then(string_bytes)
                    .ok_or_else(|| PdfError::crypt("missing /OE"))?;
                let ue = encrypt
                    .get("UE")
                    .and_then(string_bytes)
                    .ok_or_else(|| PdfError::crypt("missing /UE"))?;
                let file_key = authenticate_v5(password, &o, &u, &oe, &ue, r)?;
                Ok(Self {
                    cipher: Cipher::Aes256,
                    file_key,
                    revision: r,
                    encrypt_metadata,
                })
            }
            other => Err(PdfError::Crypt(format!("unsupported /V {other}"))),
        }
    }

    fn object_key(&self, id: ObjectId) -> Vec<u8> {
        if self.cipher == Cipher::Aes256 {
            return self.file_key.clone();
        }
        let num = id.number.to_le_bytes();
        let gen = id.generation.to_le_bytes();
        let mut parts: Vec<&[u8]> = vec![&self.file_key, &num[0..3], &gen[0..2]];
        let salt = [0x73, 0x41, 0x6C, 0x54]; // "sAlT"
        if self.cipher == Cipher::Aes128 {
            parts.push(&salt);
        }
        let digest = md5(&parts);
        let take = (self.file_key.len() + 5).min(16);
        digest[..take].to_vec()
    }

    fn decrypt_bytes(&self, id: ObjectId, data: &[u8]) -> Vec<u8> {
        let key = self.object_key(id);
        match self.cipher {
            Cipher::Identity => data.to_vec(),
            Cipher::Rc4 => rc4(&key, data),
            Cipher::Aes128 | Cipher::Aes256 => {
                aes_cbc_decrypt_padded(&key, data).unwrap_or_default()
            }
        }
    }

    /// Recursively decrypt all strings and stream payloads in an object.
    pub fn decrypt_object(&self, id: ObjectId, object: &mut PdfObject) {
        match object {
            PdfObject::LiteralString(bytes) | PdfObject::HexString(bytes) => {
                *bytes = self.decrypt_bytes(id, bytes);
            }
            PdfObject::Array(items) => {
                for item in items {
                    self.decrypt_object(id, item);
                }
            }
            PdfObject::Dictionary(dict) => {
                for value in dict.values_mut() {
                    self.decrypt_object(id, value);
                }
            }
            PdfObject::Stream(stream) => {
                let type_name = stream
                    .dictionary
                    .get("Type")
                    .and_then(PdfObject::as_name)
                    .map(str::to_owned);
                // Xref streams are never encrypted; metadata streams only
                // when /EncryptMetadata is true.
                let skip_data = type_name.as_deref() == Some("XRef")
                    || (type_name.as_deref() == Some("Metadata") && !self.encrypt_metadata);
                for value in stream.dictionary.values_mut() {
                    self.decrypt_object(id, value);
                }
                if !skip_data {
                    stream.data = self.decrypt_bytes(id, &stream.data);
                }
            }
            _ => {}
        }
    }

    pub fn revision(&self) -> i64 {
        self.revision
    }
}

fn crypt_filter_method(encrypt: &Dictionary) -> Option<&str> {
    let stmf = encrypt.get("StmF").and_then(PdfObject::as_name).unwrap_or("Identity");
    let cf = encrypt.get("CF").and_then(PdfObject::as_dict)?;
    let filter = cf.get(stmf).and_then(PdfObject::as_dict)?;
    filter.get("CFM").and_then(PdfObject::as_name)
}

// ---------------------------------------------------------------------------
// Legacy (R2–R4) authentication
// ---------------------------------------------------------------------------

fn pad_password(password: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = password.len().min(32);
    out[..n].copy_from_slice(&password[..n]);
    out[n..].copy_from_slice(&PAD[..32 - n]);
    out
}

#[allow(clippy::too_many_arguments)]
fn compute_legacy_key(
    padded: &[u8; 32],
    o: &[u8],
    p: i32,
    id0: &[u8],
    r: i64,
    key_len: usize,
    encrypt_metadata: bool,
) -> Vec<u8> {
    let p_bytes = p.to_le_bytes();
    let mut parts: Vec<&[u8]> = vec![padded, &o[..o.len().min(32)], &p_bytes, id0];
    let no_meta = [0xFFu8; 4];
    if r >= 4 && !encrypt_metadata {
        parts.push(&no_meta);
    }
    let mut key = md5(&parts).to_vec();
    if r >= 3 {
        for _ in 0..50 {
            key = md5(&[&key[..key_len]]).to_vec();
        }
    }
    key.truncate(key_len);
    key
}

fn compute_user_check(key: &[u8], id0: &[u8], r: i64) -> Vec<u8> {
    if r == 2 {
        rc4(key, &PAD)
    } else {
        let hash = md5(&[&PAD, id0]);
        let mut out = rc4(key, &hash);
        for i in 1..=19u8 {
            let xored: Vec<u8> = key.iter().map(|&b| b ^ i).collect();
            out = rc4(&xored, &out);
        }
        out
    }
}

#[allow(clippy::too_many_arguments)]
fn authenticate_legacy(
    password: &[u8],
    o: &[u8],
    u: &[u8],
    p: i32,
    id0: &[u8],
    r: i64,
    key_len: usize,
    encrypt_metadata: bool,
) -> Result<Vec<u8>> {
    // Try as the user password.
    let padded = pad_password(password);
    let key = compute_legacy_key(&padded, o, p, id0, r, key_len, encrypt_metadata);
    let check = compute_user_check(&key, id0, r);
    let matches = if r == 2 {
        check.get(..32) == u.get(..32)
    } else {
        check.get(..16) == u.get(..16)
    };
    if matches {
        return Ok(key);
    }

    // Try as the owner password: recover the user password from /O.
    let mut okey = md5(&[&pad_password(password)]).to_vec();
    if r >= 3 {
        for _ in 0..50 {
            okey = md5(&[&okey]).to_vec();
        }
    }
    okey.truncate(key_len);
    let mut user_pw = o[..o.len().min(32)].to_vec();
    if r == 2 {
        user_pw = rc4(&okey, &user_pw);
    } else {
        for i in (0..=19u8).rev() {
            let xored: Vec<u8> = okey.iter().map(|&b| b ^ i).collect();
            user_pw = rc4(&xored, &user_pw);
        }
    }
    let mut padded_user = PAD;
    let n = user_pw.len().min(32);
    padded_user[..n].copy_from_slice(&user_pw[..n]);
    let key = compute_legacy_key(&padded_user, o, p, id0, r, key_len, encrypt_metadata);
    let check = compute_user_check(&key, id0, r);
    let matches = if r == 2 {
        check.get(..32) == u.get(..32)
    } else {
        check.get(..16) == u.get(..16)
    };
    if matches {
        return Ok(key);
    }
    if password.is_empty() {
        Err(PdfError::Encrypted)
    } else {
        Err(PdfError::WrongPassword)
    }
}

// ---------------------------------------------------------------------------
// V5 (R5/R6) authentication
// ---------------------------------------------------------------------------

fn v5_hash(password: &[u8], salt: &[u8], udata: &[u8], r: i64) -> Result<[u8; 32]> {
    if r == 5 {
        Ok(sha256(&[password, salt, udata]))
    } else {
        hash_2b(password, salt, udata)
    }
}

fn authenticate_v5(
    password: &[u8],
    o: &[u8],
    u: &[u8],
    oe: &[u8],
    ue: &[u8],
    r: i64,
) -> Result<Vec<u8>> {
    if o.len() < 48 || u.len() < 48 || oe.len() < 32 || ue.len() < 32 {
        return Err(PdfError::crypt("malformed V5 password entries"));
    }
    let pw = &password[..password.len().min(127)];
    let u48 = &u[..48];

    // Owner password first (it hashes over U, so order matters for tests).
    let o_hash = v5_hash(pw, &o[32..40], u48, r)?;
    if o_hash == o[..32] {
        let ikey = v5_hash(pw, &o[40..48], u48, r)?;
        let key = aes256_cbc_nopad(&ikey, &[0u8; 16], &oe[..32], false)?;
        let mut out = [0u8; 32];
        out.copy_from_slice(&key);
        return Ok(out.to_vec());
    }

    // User password.
    let u_hash = v5_hash(pw, &u[32..40], &[], r)?;
    if u_hash == u[..32] {
        let ikey = v5_hash(pw, &u[40..48], &[], r)?;
        let key = aes256_cbc_nopad(&ikey, &[0u8; 16], &ue[..32], false)?;
        return Ok(key);
    }

    if password.is_empty() {
        Err(PdfError::Encrypted)
    } else {
        Err(PdfError::WrongPassword)
    }
}

// ---------------------------------------------------------------------------
// Encryption (AES-256, R6)
// ---------------------------------------------------------------------------

/// Produce a clone of `doc` whose strings and streams are AES-256 encrypted,
/// with a matching /Encrypt dictionary and fresh /ID. Writing the result with
/// the normal writer yields a password-protected PDF.
pub fn encrypt_document(
    doc: &PdfDocument,
    user_password: &str,
    owner_password: &str,
) -> Result<PdfDocument> {
    let owner_password = if owner_password.is_empty() {
        user_password
    } else {
        owner_password
    };
    let upw = &user_password.as_bytes()[..user_password.len().min(127)];
    let opw = &owner_password.as_bytes()[..owner_password.len().min(127)];

    let file_key: [u8; 32] = random_bytes()?;

    // /U and /UE
    let uvs: [u8; 8] = random_bytes()?;
    let uks: [u8; 8] = random_bytes()?;
    let mut u = hash_2b(upw, &uvs, &[])?.to_vec();
    u.extend_from_slice(&uvs);
    u.extend_from_slice(&uks);
    let ue_key = hash_2b(upw, &uks, &[])?;
    let ue = aes256_cbc_nopad(&ue_key, &[0u8; 16], &file_key, true)?;

    // /O and /OE (hashed over the 48-byte /U)
    let ovs: [u8; 8] = random_bytes()?;
    let oks: [u8; 8] = random_bytes()?;
    let mut o = hash_2b(opw, &ovs, &u[..48])?.to_vec();
    o.extend_from_slice(&ovs);
    o.extend_from_slice(&oks);
    let oe_key = hash_2b(opw, &oks, &u[..48])?;
    let oe = aes256_cbc_nopad(&oe_key, &[0u8; 16], &file_key, true)?;

    // /Perms
    let p: i32 = -4; // all permissions granted
    let mut perms = [0u8; 16];
    perms[0..4].copy_from_slice(&p.to_le_bytes());
    perms[4..8].copy_from_slice(&[0xFF; 4]);
    perms[8] = b'T'; // EncryptMetadata = true
    perms[9..12].copy_from_slice(b"adb");
    let tail: [u8; 4] = random_bytes()?;
    perms[12..16].copy_from_slice(&tail);
    let cipher = aes::Aes256::new(GenericArray::from_slice(&file_key));
    let mut perms_block = GenericArray::clone_from_slice(&perms);
    cipher.encrypt_block(&mut perms_block);

    // Encrypt all strings and stream payloads.
    let mut encrypted = doc.clone();
    for object in encrypted.objects.values_mut() {
        encrypt_object_tree(&file_key, &mut object.value)?;
    }

    // Build the /Encrypt dictionary.
    let mut stdcf = Dictionary::new();
    stdcf.insert("CFM".into(), PdfObject::Name("AESV3".into()));
    stdcf.insert("AuthEvent".into(), PdfObject::Name("DocOpen".into()));
    stdcf.insert("Length".into(), PdfObject::Integer(32));
    let mut cf = Dictionary::new();
    cf.insert("StdCF".into(), PdfObject::Dictionary(stdcf));
    let mut enc = Dictionary::new();
    enc.insert("Filter".into(), PdfObject::Name("Standard".into()));
    enc.insert("V".into(), PdfObject::Integer(5));
    enc.insert("R".into(), PdfObject::Integer(6));
    enc.insert("Length".into(), PdfObject::Integer(256));
    enc.insert("CF".into(), PdfObject::Dictionary(cf));
    enc.insert("StmF".into(), PdfObject::Name("StdCF".into()));
    enc.insert("StrF".into(), PdfObject::Name("StdCF".into()));
    enc.insert("O".into(), PdfObject::HexString(o));
    enc.insert("U".into(), PdfObject::HexString(u));
    enc.insert("OE".into(), PdfObject::HexString(oe));
    enc.insert("UE".into(), PdfObject::HexString(ue));
    enc.insert("P".into(), PdfObject::Integer(p as i64));
    enc.insert(
        "Perms".into(),
        PdfObject::HexString(perms_block.to_vec()),
    );
    enc.insert("EncryptMetadata".into(), PdfObject::Bool(true));

    let enc_id = encrypted.add_object(PdfObject::Dictionary(enc));
    encrypted.set_trailer_key("Encrypt", PdfObject::Reference(enc_id));

    let id_a: [u8; 16] = random_bytes()?;
    let id_b: [u8; 16] = random_bytes()?;
    encrypted.set_trailer_key(
        "ID",
        PdfObject::Array(vec![
            PdfObject::HexString(id_a.to_vec()),
            PdfObject::HexString(id_b.to_vec()),
        ]),
    );
    // PDF 2.0 feature; bump the header version.
    if encrypted.version.as_str() < "2.0" {
        encrypted.version = "2.0".to_owned();
    }
    encrypted.was_encrypted = true;
    Ok(encrypted)
}

fn encrypt_object_tree(file_key: &[u8; 32], object: &mut PdfObject) -> Result<()> {
    match object {
        PdfObject::LiteralString(bytes) | PdfObject::HexString(bytes) => {
            let iv: [u8; 16] = random_bytes()?;
            *bytes = aes_cbc_encrypt_padded(file_key, &iv, bytes)?;
        }
        PdfObject::Array(items) => {
            for item in items {
                encrypt_object_tree(file_key, item)?;
            }
        }
        PdfObject::Dictionary(dict) => {
            for value in dict.values_mut() {
                encrypt_object_tree(file_key, value)?;
            }
        }
        PdfObject::Stream(stream) => {
            for value in stream.dictionary.values_mut() {
                encrypt_object_tree(file_key, value)?;
            }
            let iv: [u8; 16] = random_bytes()?;
            stream.data = aes_cbc_encrypt_padded(file_key, &iv, &stream.data)?;
            stream
                .dictionary
                .insert("Length".into(), PdfObject::Integer(stream.data.len() as i64));
        }
        _ => {}
    }
    Ok(())
}

/// Writing an encrypted document to bytes in one call.
pub fn encrypt_to_bytes(
    doc: &PdfDocument,
    user_password: &str,
    owner_password: &str,
) -> Result<Vec<u8>> {
    let encrypted = encrypt_document(doc, user_password, owner_password)?;
    write_with_encrypt(&encrypted)
}

/// The standard writer strips /Encrypt (it writes decrypted documents), so
/// encrypted output goes through this thin wrapper that re-adds it.
fn write_with_encrypt(doc: &PdfDocument) -> Result<Vec<u8>> {
    let bytes = crate::writer::PdfWriter::write_document_to_vec_keep_encrypt(doc)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::PdfDocument;

    #[test]
    fn rc4_known_vector() {
        // RFC 6229-ish spot check: RC4("Key", "Plaintext") = BBF316E8D940AF0AD3
        let out = rc4(b"Key", b"Plaintext");
        assert_eq!(
            out,
            vec![0xBB, 0xF3, 0x16, 0xE8, 0xD9, 0x40, 0xAF, 0x0A, 0xD3]
        );
    }

    #[test]
    fn aes_round_trip() {
        let key = [7u8; 32];
        let iv = [9u8; 16];
        let ct = aes_cbc_encrypt_padded(&key, &iv, b"secret payload").unwrap();
        assert_ne!(&ct[16..], b"secret payload".as_slice());
        let pt = aes_cbc_decrypt_padded(&key, &ct).unwrap();
        assert_eq!(pt, b"secret payload");
    }

    #[test]
    fn encrypt_then_open_with_user_password() {
        let pdf = include_bytes!("../../../fixtures/simple.pdf");
        let doc = PdfDocument::from_bytes(pdf).unwrap();
        let bytes = encrypt_to_bytes(&doc, "user-pw", "owner-pw").unwrap();

        // Without a password: refused.
        let err = PdfDocument::from_bytes(&bytes).unwrap_err();
        assert!(matches!(err, PdfError::Encrypted));
        // Wrong password: refused.
        let err = PdfDocument::from_bytes_with_password(&bytes, "nope").unwrap_err();
        assert!(matches!(err, PdfError::WrongPassword));

        // User password opens and content matches.
        let reopened = PdfDocument::from_bytes_with_password(&bytes, "user-pw").unwrap();
        assert!(reopened.was_encrypted);
        assert_eq!(reopened.page_count(), doc.page_count());

        // Owner password also opens.
        let reopened = PdfDocument::from_bytes_with_password(&bytes, "owner-pw").unwrap();
        assert_eq!(reopened.page_count(), doc.page_count());
    }

    #[test]
    fn decrypted_save_produces_plain_pdf() {
        let pdf = include_bytes!("../../../fixtures/simple.pdf");
        let doc = PdfDocument::from_bytes(pdf).unwrap();
        let bytes = encrypt_to_bytes(&doc, "pw", "").unwrap();
        let opened = PdfDocument::from_bytes_with_password(&bytes, "pw").unwrap();
        // Normal save drops encryption.
        let plain = opened.to_bytes().unwrap();
        let replain = PdfDocument::from_bytes(&plain).unwrap();
        assert!(!replain.was_encrypted);
        assert_eq!(replain.page_count(), doc.page_count());
    }
}
