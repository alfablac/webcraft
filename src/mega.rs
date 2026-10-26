//! MEGA folder listing: parse a public folder URL, fetch its node list
//! from the MEGA API, and decrypt node keys/attributes into per-file links.
//! Keys are never logged.

use cbc::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

#[derive(Clone, Debug, PartialEq)]
pub struct MegaFile {
    pub name: String,
    pub size: Option<u64>,
    pub link: String,
    pub handle: String,
    /// 32-byte composite key for direct (no-debrid) download decryption.
    /// Never logged, never persisted.
    pub key: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MegaFolder {
    pub id: String,
    pub name: String,
    pub files: Vec<MegaFile>,
}

impl MegaFolder {
    pub fn pick(&self, want: &str) -> Option<&MegaFile> {
        if let Some(f) = self.files.iter().find(|f| f.name == want) {
            return Some(f);
        }
        if self.files.len() == 1 {
            return self.files.first();
        }
        None
    }
}

struct ParsedUrl {
    folder_id: String,
    key: String,
    selection: Option<String>,
}

fn parse_url(url: &str) -> Option<ParsedUrl> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let (host, after_host) = match rest.find(['/', '#', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if host != "mega.nz" && host != "mega.co.nz" && host != "mega.io" {
        return None;
    }
    // Legacy: https://mega.nz/#F!<id>!<key>
    if let Some(pos) = after_host.find("#F!") {
        let legacy = &after_host[pos + 3..];
        let mut parts = legacy.splitn(2, '!');
        let folder_id = parts.next().unwrap_or("");
        let key = parts.next().unwrap_or("");
        if folder_id.is_empty() || key.is_empty() {
            return None;
        }
        return Some(ParsedUrl {
            folder_id: folder_id.to_string(),
            key: key.to_string(),
            selection: None,
        });
    }
    let hash = after_host.rfind('#')?;
    let key = &after_host[hash + 1..];
    if key.is_empty() {
        return None;
    }
    let path = &after_host[..hash];
    let path = path.split('?').next().unwrap_or(path);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.first().copied() != Some("folder") {
        return None;
    }
    match segments.len() {
        2 if !segments[1].is_empty() => Some(ParsedUrl {
            folder_id: segments[1].to_string(),
            key: key.to_string(),
            selection: None,
        }),
        4 if segments[2] == "file" && !segments[1].is_empty() && !segments[3].is_empty() => {
            Some(ParsedUrl {
                folder_id: segments[1].to_string(),
                key: key.to_string(),
                selection: Some(segments[3].to_string()),
            })
        }
        _ => None,
    }
}

pub fn is_mega_folder_url(url: &str) -> bool {
    parse_url(url).is_some()
}

/// Canonical MEGA folder share link `https://mega.nz/folder/<id>#<key>`
/// for any accepted folder URL form (plain, `/file/<handle>` selection,
/// legacy `#F!`). The key fragment is a decryption secret: the result may
/// only go on the wire to a provider API, never into errors or logs.
/// `None` when `url` is not a MEGA folder URL.
pub fn folder_share_url(url: &str) -> Option<String> {
    let p = parse_url(url)?;
    Some(format!(
        "https://mega.nz/folder/{}#{}",
        p.folder_id, p.key
    ))
}

fn b64_val(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a' + 26) as u32),
        b'0'..=b'9' => Some((c - b'0' + 52) as u32),
        b'-' | b'+' => Some(62),
        b'_' | b'/' => Some(63),
        _ => None,
    }
}

/// Base64 decode (URL-safe or standard), tolerating missing padding and commas.
pub fn b64d(s: &str) -> Option<Vec<u8>> {
    let cleaned: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b',')
        .collect();
    let mut end = cleaned.len();
    while end > 0 && cleaned[end - 1] == b'=' {
        end -= 1;
    }
    let data = &cleaned[..end];
    if data.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4 + 3);
    let mut acc: u32 = 0;
    let mut nbits: u32 = 0;
    for &c in data {
        acc = (acc << 6) | b64_val(c)?;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Some(out)
}

/// Base64url encode without padding. Used to build keys in tests.
#[cfg(test)]
fn b64e(data: &[u8]) -> String {
    const ALPH: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPH[(n >> 18) as usize & 63] as char);
        out.push(ALPH[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPH[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPH[n as usize & 63] as char);
        }
    }
    out
}

/// Decrypt a MEGA key blob: each 16-byte block is decrypted independently
/// with AES-CBC IV=0 (not chained CBC over the whole buffer). Key length
/// selects AES-128 (16 bytes) or AES-256 (32 bytes).
fn dec_key(enc: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    if enc.is_empty() || !enc.len().is_multiple_of(16) {
        return None;
    }
    let iv = [0u8; 16];
    let mut out = Vec::with_capacity(enc.len());
    for chunk in enc.chunks(16) {
        let mut buf = chunk.to_vec();
        let pt: &[u8] = match key.len() {
            16 => {
                let k: &[u8; 16] = key.try_into().ok()?;
                Aes128CbcDec::new(k.into(), (&iv).into())
                    .decrypt_padded_mut::<NoPadding>(&mut buf)
                    .ok()?
            }
            32 => {
                let k: &[u8; 32] = key.try_into().ok()?;
                Aes256CbcDec::new(k.into(), (&iv).into())
                    .decrypt_padded_mut::<NoPadding>(&mut buf)
                    .ok()?
            }
            _ => return None,
        };
        out.extend_from_slice(pt);
    }
    Some(out)
}

/// Unwrap a share-style node key: the 32-byte encrypted blob decrypted
/// under a 16- or 32-byte wrapping key (AES-CBC per-16-byte-block, IV=0,
/// the same scheme MEGA uses for folder/share node keys). Returns the
/// 32-byte composite key when the blob is exactly 32 bytes.
pub fn unwrap_node_key(enc: &[u8], wrapping_key: &[u8]) -> Option<Vec<u8>> {
    let out = dec_key(enc, wrapping_key)?;
    if out.len() == 32 {
        Some(out)
    } else {
        None
    }
}

/// Fold a 32-byte key to 16 bytes by XORing the two halves bytewise.
fn fold32(k: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = k[i] ^ k[i + 16];
    }
    out
}

/// Decrypt a node's encrypted attributes and return its `n` (name) field.
pub fn decrypt_attrs(enc: &str, key: &[u8]) -> Option<String> {
    if key.len() != 16 {
        return None;
    }
    let enc_bytes = b64d(enc)?;
    if enc_bytes.is_empty() || !enc_bytes.len().is_multiple_of(16) {
        return None;
    }
    let k: &[u8; 16] = key.try_into().ok()?;
    let iv = [0u8; 16];
    let mut buf = enc_bytes;
    let pt = Aes128CbcDec::new(k.into(), (&iv).into())
        .decrypt_padded_mut::<NoPadding>(&mut buf)
        .ok()?;
    let mut bytes = pt.to_vec();
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    let text = String::from_utf8(bytes).ok()?;
    let json = text.strip_prefix("MEGA")?;
    if !json.starts_with('{') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get("n")?.as_str().map(|s| s.to_string())
}

/// Pick the `owner:b64` entry of a node's `k` field whose owner matches
/// `root_h`, else the last entry. Returns the decoded entry bytes.
fn pick_key_entry(k: &str, root_h: &str) -> Option<Vec<u8>> {
    let entries: Vec<&str> = k.split('/').filter(|e| !e.is_empty()).collect();
    if entries.is_empty() {
        return None;
    }
    let chosen = entries
        .iter()
        .find(|e| !root_h.is_empty() && e.split(':').next().unwrap_or("") == root_h)
        .copied()
        .unwrap_or_else(|| entries[entries.len() - 1]);
    let b64 = chosen.split_once(':').map(|(_, b)| b).unwrap_or(chosen);
    b64d(b64)
}

fn decrypt_nodes(folder_id: &str, key_bytes: &[u8], nodes: &serde_json::Value) -> MegaFolder {
    let empty = Vec::new();
    let nodes = nodes.as_array().unwrap_or(&empty);
    let root = nodes
        .iter()
        .find(|n| n.get("t").and_then(|t| t.as_u64()) == Some(1));
    let root_h = root
        .and_then(|n| n.get("h").and_then(|h| h.as_str()))
        .unwrap_or("")
        .to_string();

    let folder_name = (|| -> Option<String> {
        let root = root?;
        let k = root.get("k")?.as_str()?;
        let raw = pick_key_entry(k, &root_h).and_then(|enc| dec_key(&enc, key_bytes))?;
        let attr_key: Vec<u8> = match raw.len() {
            16 => raw,
            32 => fold32(&raw).to_vec(),
            _ => return None,
        };
        decrypt_attrs(root.get("a")?.as_str()?, &attr_key)
    })()
    .unwrap_or_else(|| "MEGA folder".to_string());

    let mut files = Vec::new();
    for node in nodes.iter().filter(|n| n.get("t").and_then(|t| t.as_u64()) == Some(0)) {
        let Some(handle) = node.get("h").and_then(|h| h.as_str()) else {
            continue;
        };
        let Some(k) = node.get("k").and_then(|k| k.as_str()) else {
            continue;
        };
        let Some(raw) = pick_key_entry(k, &root_h).and_then(|enc| dec_key(&enc, key_bytes))
        else {
            continue;
        };
        let attr_key: Vec<u8> = match raw.len() {
            16 => raw.clone(),
            32 => fold32(&raw).to_vec(),
            _ => continue,
        };
        let Some(a) = node.get("a").and_then(|a| a.as_str()) else {
            continue;
        };
        let Some(name) = decrypt_attrs(a, &attr_key) else {
            continue;
        };
        files.push(MegaFile {
            name,
            size: node.get("s").and_then(|s| s.as_u64()),
            // MEGA's own per-file form: bare node handles are not public
            // file handles (`g p=<node>` -> -9), a node resolves only with
            // folder context (`?n=<folderId>`). The fragment key is the
            // folder key, which decrypts every child node — `decrypt_nodes`
            // regenerates this form for any selected handle.
            link: format!(
                "https://mega.nz/folder/{}/file/{}",
                folder_id, handle
            ),
            handle: handle.to_string(),
            key: match raw.len() {
                32 => raw.clone(),
                16 => {
                    let mut k = raw.clone();
                    k.extend_from_slice(&raw);
                    k
                }
                _ => continue,
            },
        });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    MegaFolder {
        id: folder_id.to_string(),
        name: folder_name,
        files,
    }
}

/// Decrypt a file node's attributes with its plain composite key
/// (transfer.it stores keys unwrapped): the AES key is the XOR fold of
/// the 32-byte key — same fold `decrypt_nodes` applies to folder files.
pub fn decrypt_file_attrs(enc: &str, composite_key: &[u8]) -> Option<String> {
    if composite_key.len() != 32 {
        return None;
    }
    decrypt_attrs(enc, &fold32(composite_key))
}

/// Base64url-encode bytes without padding (MEGA's key encoding).
pub fn b64e_nopad(data: &[u8]) -> String {
    const ALPH: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPH[(n >> 18) as usize & 63] as char);
        out.push(ALPH[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPH[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPH[n as usize & 63] as char);
        }
    }
    out
}

pub async fn list_folder(http: reqwest::Client, url: &str) -> Result<MegaFolder, String> {
    let parsed = parse_url(url).ok_or_else(|| "not a MEGA folder URL".to_string())?;
    let key_bytes = b64d(&parsed.key).ok_or_else(|| "invalid MEGA folder key".to_string())?;
    if key_bytes.len() != 16 && key_bytes.len() != 32 {
        return Err(format!(
            "invalid MEGA folder key length: {} bytes",
            key_bytes.len()
        ));
    }
    let api_url = format!("https://g.api.mega.co.nz/cs?id=0&n={}", parsed.folder_id);
    let resp = http
        .post(&api_url)
        .json(&serde_json::json!([{"a": "f", "c": 1, "r": 1, "ca": 1}]))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let text = resp.text().await.map_err(|e| e.to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("invalid MEGA API response: {e}"))?;
    let negative = match &value {
        serde_json::Value::Number(n) => n.as_i64().filter(|n| *n < 0),
        serde_json::Value::Array(arr) => match arr.first() {
            Some(serde_json::Value::Number(n)) => n.as_i64().filter(|n| *n < 0),
            _ => None,
        },
        _ => None,
    };
    if let Some(n) = negative {
        return Err(format!("MEGA folder unavailable (API {n})"));
    }
    let nodes = value
        .get(0)
        .and_then(|o| o.get("f"))
        .and_then(|f| f.as_array())
        .ok_or_else(|| "MEGA API response missing file list".to_string())?;
    let nodes_value = serde_json::Value::Array(nodes.clone());
    let mut folder = decrypt_nodes(&parsed.folder_id, &key_bytes, &nodes_value);
    if let Some(selection) = &parsed.selection {
        folder.files.retain(|f| &f.handle == selection);
    }
    if folder.files.is_empty() {
        return Err("MEGA folder contains no downloadable files".to_string());
    }
    Ok(folder)
}

/// Decrypt a completed MEGA file in place (chunked, whole file).
pub fn decrypt_file_in_place(path: &std::path::Path, key: &[u8]) -> Result<(), String> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut cipher = crate::mega::FileCipher::new(key)?;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 1024 * 1024];
    let mut pos: u64 = 0;
    loop {
        f.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        cipher.decrypt_chunk(&mut buf[..n]);
        f.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
        f.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        pos += n as u64;
    }
    // Whole file decrypted: the chunk MACs must condense to key[24..32].
    cipher.verify()
}

/// Direct download URL for a folder child node, for the no-debrid path.
/// Returns `(url, size)`. The URL serves the still-encrypted file; the
/// caller decrypts with [`decrypt_file`] while streaming. Never log it.
pub async fn node_download_url(
    http: &reqwest::Client,
    folder_id: &str,
    handle: &str,
) -> Result<(String, Option<u64>), String> {
    let api_url = format!("https://g.api.mega.co.nz/cs?id=0&n={folder_id}");
    let resp = http
        .post(&api_url)
        .json(&serde_json::json!([{"a": "g", "g": 1, "n": handle}]))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let text = resp.text().await.map_err(|e| e.to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("invalid MEGA API response: {e}"))?;
    let obj = value
        .get(0)
        .ok_or_else(|| "MEGA API response missing download info".to_string())?;
    if let Some(n) = obj.as_i64().filter(|n| *n < 0) {
        return Err(format!("MEGA file unavailable (API {n})"));
    }
    let url = obj
        .get("g")
        .and_then(|g| g.as_str())
        .filter(|g| !g.is_empty())
        .ok_or_else(|| "MEGA API response missing download URL".to_string())?
        .to_string();
    Ok((url, obj.get("s").and_then(|s| s.as_u64())))
}

/// CTR state for one MEGA file. The 32-byte composite key is XOR-folded
/// to the 16-byte AES-128 key (same fold as attribute decryption);
/// IV = key[16..24] || 0 and the 128-bit big-endian counter starts at 0
/// (key[24..32] is the meta-MAC seed, not a counter). Verified live
/// against a known SFV file.
pub struct FileCipher {
    key: [u8; 16],
    nonce_hi: [u8; 8],
    ctr: u64,
    leftover: Vec<u8>,
    /// `key[24..32]`: condensed chunk-MAC expected value (meta-MAC).
    meta_mac: [u8; 8],
    /// Running CBC-MAC state within the current chunk. Starts at
    /// `nonce || nonce` for every chunk (SDK `SymmCipher::ctr_crypt`).
    mac_state: [u8; 16],
    /// macsmac accumulator: starts as 16 zero bytes; each finished
    /// chunk MAC is XORed in and the result ECB-encrypted (SDK
    /// `chunkmac_map::macsmac`).
    mac_condensed: [u8; 16],
    /// Buffered plaintext not yet MACed (trailing partial block).
    mac_pending: Vec<u8>,
    /// Index / in-chunk position of the MAC stream.
    mac_chunk_index: u64,
    mac_in_chunk: usize,
    /// Plaintext bytes fed so far.
    fed: u64,
}

/// MEGA data-chunk size for a 0-based chunk index: 128 KiB growing by
/// 128 KiB per chunk up to 1 MiB, then constant 1 MiB. MACs are computed
/// per chunk and condensed into the meta-MAC.
fn chunk_size(index: u64) -> u64 {
    const KIB: u64 = 1024;
    (128 * KIB * (index + 1)).min(1024 * KIB)
}

impl FileCipher {
    pub fn new(composite_key: &[u8]) -> Result<Self, String> {
        if composite_key.len() != 32 {
            return Err("invalid MEGA file key length".into());
        }
        let mut nonce_hi = [0u8; 8];
        nonce_hi.copy_from_slice(&composite_key[16..24]);
        let mut meta_mac = [0u8; 8];
        meta_mac.copy_from_slice(&composite_key[24..32]);
        let mut mac_iv = [0u8; 16];
        mac_iv[..8].copy_from_slice(&nonce_hi);
        mac_iv[8..].copy_from_slice(&nonce_hi);
        Ok(Self {
            key: fold32(composite_key),
            nonce_hi,
            ctr: 0,
            leftover: Vec::new(),
            meta_mac,
            mac_state: mac_iv,
            mac_condensed: [0u8; 16],
            mac_pending: Vec::new(),
            mac_chunk_index: 0,
            mac_in_chunk: 0,
            fed: 0,
        })
    }

    /// Feed decrypted plaintext into the per-chunk CBC-MAC. Chunks
    /// follow [`chunk_size`]; only full 16-byte blocks inside the
    /// current chunk are MACed immediately — a trailing partial block
    /// might yet grow with the next feed, so it is buffered and
    /// zero-padded only when the chunk completes (or at `verify`).
    fn feed_mac(&mut self, plain: &[u8]) {
        self.mac_pending.extend_from_slice(plain);
        loop {
            let csize = chunk_size(self.mac_chunk_index) as usize;
            let in_chunk = csize - self.mac_in_chunk;
            if self.mac_pending.is_empty() {
                break;
            }
            let avail = self.mac_pending.len().min(in_chunk);
            let chunk_end = avail == in_chunk;
            // Only whole blocks may leave the buffer mid-chunk; a
            // trailing partial block stays buffered — it might yet grow
            // with the next feed. It is zero-padded (SDK `ctr_crypt`
            // semantics) only when the chunk actually completes.
            let take = if chunk_end { avail } else { avail / 16 * 16 };
            if take == 0 {
                break;
            }
            let buf: Vec<u8> = self.mac_pending.drain(..take).collect();
            self.mac_chunk(&buf, chunk_end);
            self.mac_in_chunk += take;
            if self.mac_in_chunk == csize {
                self.condense_chunk();
            }
        }
        self.fed += plain.len() as u64;
    }

    /// CBC-MAC `data` (already bounded to the current chunk) under the
    /// folded key starting from `mac_state`; a trailing partial block
    /// is zero-padded only when `chunk_end` is true.
    fn mac_chunk(&mut self, data: &[u8], chunk_end: bool) {
        use cbc::cipher::{BlockEncryptMut, KeyIvInit};
        type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
        let mut off = 0usize;
        while off + 16 <= data.len() {
            for i in 0..16 {
                self.mac_state[i] ^= data[off + i];
            }
            Aes128CbcEnc::new((&self.key).into(), (&[0u8; 16]).into())
                .encrypt_block_mut((&mut self.mac_state).into());
            off += 16;
        }
        if chunk_end && off < data.len() {
            let mut padded = [0u8; 16];
            padded[..data.len() - off].copy_from_slice(&data[off..]);
            for (i, b) in padded.iter().enumerate() {
                self.mac_state[i] ^= *b;
            }
            Aes128CbcEnc::new((&self.key).into(), (&[0u8; 16]).into())
                .encrypt_block_mut((&mut self.mac_state).into());
        }
    }

    /// Fold the finished chunk's MAC into the macsmac accumulator
    /// (XOR in, then ECB-encrypt) and reset the per-chunk MAC state to
    /// `nonce || nonce` for the next chunk (SDK `chunkmac_map`).
    fn condense_chunk(&mut self) {
        use cbc::cipher::{BlockEncryptMut, KeyIvInit};
        type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
        for i in 0..16 {
            self.mac_condensed[i] ^= self.mac_state[i];
        }
        Aes128CbcEnc::new((&self.key).into(), (&[0u8; 16]).into())
            .encrypt_block_mut((&mut self.mac_condensed).into());
        self.mac_chunk_index += 1;
        self.mac_in_chunk = 0;
        self.mac_state = [0u8; 16];
        self.mac_state[..8].copy_from_slice(&self.nonce_hi);
        self.mac_state[8..].copy_from_slice(&self.nonce_hi);
    }

    /// Verify whole-file integrity: the macsmac of all chunk MACs,
    /// folded to 8 bytes, must equal meta-MAC `key[24..32]`. Call
    /// after all bytes were decrypted. Empty files have no chunks and
    /// nothing to verify.
    pub fn verify(&mut self) -> Result<(), String> {
        if self.fed == 0 {
            return Ok(());
        }
        // Flush the final (possibly partial) chunk.
        if self.mac_in_chunk > 0 || !self.mac_pending.is_empty() {
            let buf = std::mem::take(&mut self.mac_pending);
            self.mac_chunk(&buf, true);
            self.condense_chunk();
        }
        let mut folded = [0u8; 8];
        for i in 0..4 {
            folded[i] = self.mac_condensed[i] ^ self.mac_condensed[i + 4];
            folded[i + 4] = self.mac_condensed[i + 8] ^ self.mac_condensed[i + 12];
        }
        if folded == self.meta_mac {
            Ok(())
        } else {
            Err("MEGA integrity check failed (chunk MAC mismatch)".into())
        }
    }

    /// Decrypt `data` at the current stream position. MEGA CTR uses the
    /// full 128-bit counter (hi = iv[0], lo = iv[1]) and discards the
    /// trailing partial block of each chunk exactly like the reference
    /// `mega` JS (`encryptWork` drops `i0` remainder): positions advance
    /// in whole 16-byte blocks, chunks are expected block-aligned except
    /// the final one.
    pub fn decrypt_chunk(&mut self, data: &mut [u8]) {
        use aes::cipher::StreamCipher;
        let mut off = 0usize;
        if !self.leftover.is_empty() {
            let n = self.leftover.len().min(data.len());
            for (i, byte) in data.iter_mut().enumerate().take(n) {
                *byte ^= self.leftover[i];
            }
            self.leftover.drain(..n);
            off = n;
        }
        let rest = &mut data[off..];
        let full = rest.len() / 16 * 16;
        if full > 0 {
            let mut iv = [0u8; 16];
            iv[..8].copy_from_slice(&self.nonce_hi);
            iv[8..].copy_from_slice(&self.ctr.to_be_bytes());
            type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;
            let mut cipher = Aes128Ctr::new((&self.key).into(), (&iv).into());
            cipher.apply_keystream(&mut rest[..full]);
            self.ctr = self.ctr.wrapping_add((full / 16) as u64);
        }
        let tail = &mut rest[full..];
        if !tail.is_empty() {
            let mut iv = [0u8; 16];
            iv[..8].copy_from_slice(&self.nonce_hi);
            iv[8..].copy_from_slice(&self.ctr.to_be_bytes());
            type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;
            let mut cipher = Aes128Ctr::new((&self.key).into(), (&iv).into());
            let mut block = [0u8; 16];
            cipher.apply_keystream(&mut block);
            for (i, b) in tail.iter_mut().enumerate() {
                *b ^= block[i];
            }
            self.leftover = block[tail.len()..].to_vec();
            self.ctr = self.ctr.wrapping_add(1);
        }
        // `data` is plaintext now: feed the chunk-MAC stream.
        self.feed_mac(data);
    }
}

/// Decrypt a complete MEGA file body (test/small-file helper). The
/// chunk MACs are verified against the key's meta-MAC (skipped for
/// zero-length files), so corruption fails instead of passing silently.
pub fn decrypt_file(composite_key: &[u8], data: &mut [u8]) -> Result<(), String> {
    let mut cipher = FileCipher::new(composite_key)?;
    cipher.decrypt_chunk(data);
    cipher.verify()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_nodes() -> serde_json::Value {
        serde_json::json!([
            {"h": "DcsCBbqR", "t": 1, "a": "XLIuSJmeQ8TahnqflHa0tQ", "k": "DcsCBbqR:dmSIkmbqNtPOr8IPcjtVgg"},
            {"h": "3d1kHLiT", "t": 0, "s": 754318168u64, "a": "e4cIWDbNMWoDq-Bru42RcdQALDmTM6kq6Mii-mB5lRWHH1N72fBobYsDVjT5YYBLk5P0Q78Hsv_N1oM8Kmv3yA", "k": "DcsCBbqR:qgd70YLuwfPC0n1eG_aMqIp9UfQs6uesI6fCKMzNuNA"},
            {"h": "zBkyABZR", "t": 0, "s": 838860800u64, "a": "5QgsBiRsG4gIScM-3VtVmMYmg6wDLNy5Siv1PYaAkzJji4CQTUOXIVGsH4SKRqiSHTA0olTw2JzvmU4gV-Cvfw", "k": "DcsCBbqR:meNf76m2WFdKHS5R3cvivNsUDW-hH6oRq6aUFgfj0BQ"}
        ])
    }

    #[test]
    fn decrypts_live_fixture() {
        let key = b64d("FIBCO0O4W6plbvVKNOc4cQ").unwrap();
        assert_eq!(key.len(), 16);
        let folder = decrypt_nodes("WRMSVYAS", &key, &fixture_nodes());
        assert_eq!(folder.id, "WRMSVYAS");
        assert_eq!(folder.name, "0272");
        assert_eq!(folder.files.len(), 2);
        // Sorted by name: part08 before part09.
        assert_eq!(folder.files[0].name, "0272.part08.rar");
        assert_eq!(folder.files[1].name, "0272.part09.rar");

        let f1 = folder
            .files
            .iter()
            .find(|f| f.handle == "3d1kHLiT")
            .unwrap();
        assert_eq!(f1.name, "0272.part09.rar");
        assert_eq!(f1.size, Some(754318168));
        assert_eq!(
            f1.link,
            "https://mega.nz/folder/WRMSVYAS/file/3d1kHLiT"
        );
        // The folder-selection link carries no fragment key; re-listing it
        // with the folder key narrows to exactly this file.
        assert!(is_mega_folder_url(
            "https://mega.nz/folder/WRMSVYAS/file/3d1kHLiT#FIBCO0O4W6plbvVKNOc4cQ"
        ));

        let f2 = folder
            .files
            .iter()
            .find(|f| f.handle == "zBkyABZR")
            .unwrap();
        assert_eq!(f2.name, "0272.part08.rar");
        assert_eq!(f2.size, Some(838860800));
    }

    #[test]
    fn folder_url_detection() {
        assert!(is_mega_folder_url(
            "https://mega.nz/folder/WRMSVYAS#FIBCO0O4W6plbvVKNOc4cQ"
        ));
        assert!(is_mega_folder_url(
            "https://mega.nz/folder/WRMSVYAS/file/3d1kHLiT#FIBCO0O4W6plbvVKNOc4cQ"
        ));
        assert!(is_mega_folder_url(
            "https://mega.nz/#F!WRMSVYAS!FIBCO0O4W6plbvVKNOc4cQ"
        ));
        assert!(is_mega_folder_url(
            "https://mega.co.nz/folder/WRMSVYAS#FIBCO0O4W6plbvVKNOc4cQ"
        ));
        assert!(!is_mega_folder_url(
            "https://mega.nz/file/3d1kHLiT#U-BlUBWRxh-b7nQVNiujMxwB1Vc0hW23TzYpDQhioaw"
        ));
        assert!(!is_mega_folder_url(
            "https://example.com/folder/WRMSVYAS#FIBCO0O4W6plbvVKNOc4cQ"
        ));
        assert!(!is_mega_folder_url("https://mega.nz/folder/WRMSVYAS"));
        assert!(!is_mega_folder_url("not a url"));
    }

    #[test]
    fn folder_share_url_forms() {
        let canonical = "https://mega.nz/folder/WRMSVYAS#FIBCO0O4W6plbvVKNOc4cQ";
        assert_eq!(
            folder_share_url(canonical).as_deref(),
            Some(canonical)
        );
        // Per-file selection form loses the `/file/<handle>` part.
        assert_eq!(
            folder_share_url(
                "https://mega.nz/folder/WRMSVYAS/file/3d1kHLiT#FIBCO0O4W6plbvVKNOc4cQ"
            )
            .as_deref(),
            Some(canonical)
        );
        // Legacy #F! and alias hosts normalize to mega.nz.
        assert_eq!(
            folder_share_url("https://mega.nz/#F!WRMSVYAS!FIBCO0O4W6plbvVKNOc4cQ").as_deref(),
            Some(canonical)
        );
        assert_eq!(
            folder_share_url("https://mega.co.nz/folder/WRMSVYAS#FIBCO0O4W6plbvVKNOc4cQ")
                .as_deref(),
            Some(canonical)
        );
        // Non-folder URLs rejected.
        assert_eq!(
            folder_share_url("https://mega.nz/file/3d1kHLiT#U-BlUBWRxh-b7nQVNiujMxwB1Vc0hW23TzYpDQhioaw"),
            None
        );
        assert_eq!(folder_share_url("https://mega.nz/folder/WRMSVYAS"), None);
        assert_eq!(
            folder_share_url("https://example.com/folder/WRMSVYAS#FIBCO0O4W6plbvVKNOc4cQ"),
            None
        );
        assert_eq!(folder_share_url("not a url"), None);
    }

    #[test]
    fn pick_exact_single_and_none() {
        let key = b64d("FIBCO0O4W6plbvVKNOc4cQ").unwrap();
        let folder = decrypt_nodes("WRMSVYAS", &key, &fixture_nodes());
        assert_eq!(
            folder.pick("0272.part08.rar").map(|f| f.handle.as_str()),
            Some("zBkyABZR")
        );
        assert_eq!(folder.pick("missing.rar"), None);

        let single = MegaFolder {
            id: "X".to_string(),
            name: "f".to_string(),
            files: vec![MegaFile {
                name: "only.bin".to_string(),
                size: Some(1),
                link: "https://mega.nz/file/h#k".to_string(),
                handle: "h".to_string(),
                key: vec![0u8; 32],
            }],
        };
        assert_eq!(
            single.pick("anything").map(|f| f.name.as_str()),
            Some("only.bin")
        );
        assert_eq!(
            single.pick("only.bin").map(|f| f.name.as_str()),
            Some("only.bin")
        );
    }

    #[test]
    fn b64_roundtrip_and_legacy_parse() {
        assert_eq!(b64e(&[0xfb, 0xff, 0xfe]), "-__-");
        assert_eq!(b64d("-__-").unwrap(), vec![0xfb, 0xff, 0xfe]);
        assert_eq!(b64d("QQ").unwrap(), vec![b'A']);
        assert_eq!(b64d("QQ==").unwrap(), vec![b'A']);
        let p = parse_url("https://mega.nz/#F!WRMSVYAS!FIBCO0O4W6plbvVKNOc4cQ").unwrap();
        assert_eq!(p.folder_id, "WRMSVYAS");
        assert_eq!(p.key, "FIBCO0O4W6plbvVKNOc4cQ");
        assert_eq!(p.selection, None);
        let p = parse_url(
            "https://mega.nz/folder/WRMSVYAS/file/3d1kHLiT#FIBCO0O4W6plbvVKNOc4cQ",
        )
        .unwrap();
        assert_eq!(p.selection.as_deref(), Some("3d1kHLiT"));
    }

    #[test]
    fn transfer_plain_key_helpers() {
        // transfer.it fixture (public share material): node key is the
        // plain 32-byte composite key — attrs fold32-decrypt, share URL
        // re-encodes identically.
        let a = "nPf9vTIxvG_BxIidKRSBwgEhtCafbDENrLntolCWQ-Bn1zC69xe4HLKehbgVtYBi8lH4nvFWYYuoHCy7oI0VrA";
        let k = b64d("TPEpgreX18iRLIZWiOVHVDGhY3bVtz2tlGn6Bhnz03w").unwrap();
        assert_eq!(k.len(), 32);
        assert_eq!(decrypt_file_attrs(a, &k).as_deref(), Some("thunderbirds.iso"));
        assert_eq!(b64e_nopad(&k), "TPEpgreX18iRLIZWiOVHVDGhY3bVtz2tlGn6Bhnz03w");
        assert_eq!(decrypt_file_attrs(a, &k[..16]), None);
    }

    #[test]
    fn file_cipher_roundtrip() {
        // Encrypt with the same construction (CTR is symmetric), split
        // into unaligned chunks, decrypt with FileCipher: must round-trip.
        let key: Vec<u8> = (0u8..32).collect();
        let plain: Vec<u8> = (0u32..1000).map(|i| (i % 251) as u8).collect();
        let mut cipher = FileCipher::new(&key).unwrap();
        let mut enc = plain.clone();
        cipher.decrypt_chunk(&mut enc);
        assert_ne!(enc, plain);
        let mut dec = FileCipher::new(&key).unwrap();
        let mut out = Vec::new();
        for chunk in enc.chunks(100) {
            let mut c = chunk.to_vec();
            dec.decrypt_chunk(&mut c);
            out.extend_from_slice(&c);
        }
        assert_eq!(out, plain);
    }

    /// Chunk-MAC verification against independently computed vectors.
    /// The meta-MACs below were computed in Python (pycryptodome) from
    /// the SDK scheme — per-chunk CBC-MAC starting from `nonce||nonce`
    /// under the folded key, macsmac = ECB-encrypted XOR chain starting
    /// from zero, folded 16→8 — NOT from this Rust code, so a wrong
    /// construction here fails these tests.
    fn check_mac_vector(key: &[u8; 32], plain: &[u8], tamper: bool) -> Result<(), String> {
        // CTR-encrypt with a fresh cipher (symmetric), then decrypt in
        // odd-sized feeds through another and verify its MAC.
        let mut enc = FileCipher::new(key)?;
        let mut ct = plain.to_vec();
        enc.decrypt_chunk(&mut ct);
        assert_ne!(ct[..plain.len().min(64)], plain[..plain.len().min(64)]);
        if tamper {
            let mid = ct.len() / 2;
            ct[mid] ^= 0xFF;
        }
        let mut dec = FileCipher::new(key)?;
        let mut out = Vec::new();
        for chunk in ct.chunks(4097) {
            let mut c = chunk.to_vec();
            dec.decrypt_chunk(&mut c);
            out.extend_from_slice(&c);
        }
        if !tamper {
            assert_eq!(out, plain);
        }
        dec.verify()
    }

    #[test]
    fn chunk_mac_verification() {
        // Vector A: spans 3 chunks (128 KiB + 256 KiB growth, partial tail).
        let key_a: [u8; 32] = [
            0xb0, 0xb0, 0xb0, 0xb0, 0xb0, 0xb0, 0xb0, 0xb0, 0xb3, 0x71, 0x13, 0x27, 0x89, 0x85,
            0xd9, 0x72, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x1b, 0xd8, 0xb9, 0x8c,
            0x25, 0x28, 0x77, 0xdd,
        ];
        let plain_a: Vec<u8> = (0..(128 * 1024 + 100_000 + 37))
            .map(|i| ((i * 7 + 3) % 256) as u8)
            .collect();
        check_mac_vector(&key_a, &plain_a, false).unwrap();
        assert!(check_mac_vector(&key_a, &plain_a, true).is_err());

        // Vector B: single partial-block-heavy chunk (1000 bytes).
        let key_b: [u8; 32] = [
            0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0xb1, 0x95, 0x8c, 0x20, 0x11, 0xf7,
            0xd3, 0xb6, 0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0xf9, 0xdc, 0xc6, 0x6b,
            0x5d, 0xba, 0x9d, 0xf9,
        ];
        let plain_b: Vec<u8> = (0..1000).map(|i| ((i * 7 + 3) % 256) as u8).collect();
        check_mac_vector(&key_b, &plain_b, false).unwrap();
        assert!(check_mac_vector(&key_b, &plain_b, true).is_err());

        // Vector C: exactly one full 128 KiB chunk (boundary: condensed
        // eagerly inside feed_mac, verify must not re-flush).
        let key_c: [u8; 32] = [
            0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0xee, 0x4e, 0x0e, 0xb1, 0xde, 0xe2,
            0x11, 0xd2, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0xc6, 0x67, 0x24, 0x9a,
            0xf2, 0xcf, 0x3f, 0xfd,
        ];
        let plain_c: Vec<u8> = (0..128 * 1024).map(|i| ((i * 7 + 3) % 256) as u8).collect();
        check_mac_vector(&key_c, &plain_c, false).unwrap();

        // Zero-length file: nothing to verify.
        let mut c = FileCipher::new(&key_c).unwrap();
        c.verify().unwrap();
    }
}
