//! Git's object, pkt-line, tree/commit, and packfile formats.
//!
//! This module deliberately has no Git-binary dependency.  In particular, SHA-1 is implemented here
//! so hashing object data and pack trailers does not allocate.

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};

pub const ZERO_OID: &str = "0000000000000000000000000000000000000000";
pub const PKT_MAX_PAYLOAD: usize = 65_516;
/// A side-band-64k data packet carries one band byte.
pub const SIDEBAND_MAX_DATA: usize = 65_515;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjType {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl ObjType {
    pub const fn type_code(self) -> u8 {
        match self {
            Self::Commit => 1,
            Self::Tree => 2,
            Self::Blob => 3,
            Self::Tag => 4,
        }
    }

    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Commit),
            2 => Some(Self::Tree),
            3 => Some(Self::Blob),
            4 => Some(Self::Tag),
            _ => None,
        }
    }

    const fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::Commit => b"commit",
            Self::Tree => b"tree",
            Self::Blob => b"blob",
            Self::Tag => b"tag",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitObject {
    pub kind: ObjType,
    pub data: Vec<u8>,
}

pub fn is_oid(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Standard FIPS 180-1 SHA-1.  All working storage is on the stack.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    sha1_parts(&[data])
}

fn sha1_parts(parts: &[&[u8]]) -> [u8; 20] {
    const INITIAL: [u32; 5] = [
        0x6745_2301,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];

    let mut state = INITIAL;
    let mut block = [0u8; 64];
    let mut used = 0usize;
    let mut total_len = 0u64;
    for part in parts {
        total_len = total_len.wrapping_add(part.len() as u64);
        let mut input = *part;
        if used != 0 {
            let take = (64 - used).min(input.len());
            block[used..used + take].copy_from_slice(&input[..take]);
            used += take;
            input = &input[take..];
            if used < 64 {
                continue;
            }
            sha1_compress(&mut state, &block);
        }
        let mut chunks = input.chunks_exact(64);
        for full in &mut chunks {
            sha1_compress(&mut state, full);
        }
        let remainder = chunks.remainder();
        block[..remainder.len()].copy_from_slice(remainder);
        used = remainder.len();
    }

    block[used] = 0x80;
    used += 1;
    if used > 56 {
        block[used..].fill(0);
        sha1_compress(&mut state, &block);
        block.fill(0);
    } else {
        block[used..56].fill(0);
    }
    block[56..].copy_from_slice(&total_len.wrapping_mul(8).to_be_bytes());
    sha1_compress(&mut state, &block);

    let mut digest = [0u8; 20];
    for (chunk, word) in digest.chunks_exact_mut(4).zip(state) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

fn sha1_compress(state: &mut [u32; 5], block: &[u8]) {
    let mut words = [0u32; 80];
    for (index, chunk) in block.chunks_exact(4).enumerate() {
        words[index] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    for index in 16..80 {
        words[index] =
            (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                .rotate_left(1);
    }

    let [mut a, mut b, mut c, mut d, mut e] = *state;
    for (index, word) in words.iter().enumerate() {
        let (function, constant) = match index {
            0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999),
            20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
            _ => (b ^ c ^ d, 0xca62_c1d6),
        };
        let temp = a
            .rotate_left(5)
            .wrapping_add(function)
            .wrapping_add(e)
            .wrapping_add(constant)
            .wrapping_add(*word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = temp;
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
}

pub fn hash_object(kind: ObjType, data: &[u8]) -> String {
    let mut header = [0u8; 64];
    let mut used = 0usize;
    let type_name = kind.as_bytes();
    header[..type_name.len()].copy_from_slice(type_name);
    used += type_name.len();
    header[used] = b' ';
    used += 1;

    let mut length = data.len();
    let mut digits = [0u8; 20];
    let mut digit_count = 0;
    loop {
        digits[digit_count] = b'0' + (length % 10) as u8;
        digit_count += 1;
        length /= 10;
        if length == 0 {
            break;
        }
    }
    for digit in digits[..digit_count].iter().rev() {
        header[used] = *digit;
        used += 1;
    }
    header[used] = 0;
    used += 1;
    hex_encode(&sha1_parts(&[&header[..used], data]))
}

// ---------------------------------------------------------------- pkt-line

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PktError(pub String);

impl fmt::Display for PktError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PktError {}

/// Fallible pkt-line encoder, useful when accepting untrusted payload sizes.
pub fn try_pkt(payload: &[u8]) -> Result<Vec<u8>, PktError> {
    if payload.len() > PKT_MAX_PAYLOAD {
        return Err(PktError(format!(
            "pkt-line payload of {} bytes is too long",
            payload.len()
        )));
    }
    let total = payload.len() + 4;
    let mut packet = Vec::with_capacity(total);
    packet.extend_from_slice(format!("{total:04x}").as_bytes());
    packet.extend_from_slice(payload);
    Ok(packet)
}

/// Encode a pkt-line.  An oversized payload is represented by an empty vector; callers that need
/// to distinguish this from valid encodings should use [`try_pkt`].
pub fn pkt(payload: &[u8]) -> Vec<u8> {
    try_pkt(payload).unwrap_or_default()
}

pub fn flush() -> Vec<u8> {
    b"0000".to_vec()
}

pub fn delim() -> Vec<u8> {
    b"0001".to_vec()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    Data(Vec<u8>),
    Flush,
    Delim,
    End,
}

pub fn read_packets(
    buf: &[u8],
    offset: usize,
    stop_at_flush: bool,
) -> Result<(Vec<Packet>, usize), PktError> {
    if offset > buf.len() {
        return Err(PktError(
            "pkt-line offset is past the end of the buffer".to_string(),
        ));
    }
    let mut packets = Vec::new();
    let mut pos = offset;
    while pos < buf.len() {
        let header_end = pos
            .checked_add(4)
            .ok_or_else(|| PktError("pkt-line offset overflow".to_string()))?;
        if header_end > buf.len() {
            return Err(PktError("truncated pkt-line header".to_string()));
        }
        let header = &buf[pos..header_end];
        if !header.iter().all(u8::is_ascii_hexdigit) {
            return Err(PktError(format!(
                "invalid pkt-line length {:?}",
                String::from_utf8_lossy(header)
            )));
        }
        let mut length = 0usize;
        for byte in header {
            length = (length << 4) | usize::from(hex_value(*byte).unwrap_or(0));
        }
        match length {
            0 => {
                packets.push(Packet::Flush);
                pos = header_end;
                if stop_at_flush {
                    break;
                }
            }
            1 => {
                packets.push(Packet::Delim);
                pos = header_end;
            }
            2 => {
                packets.push(Packet::End);
                pos = header_end;
            }
            3 => return Err(PktError("invalid pkt-line length 3".to_string())),
            _ => {
                let packet_end = pos
                    .checked_add(length)
                    .ok_or_else(|| PktError("pkt-line length overflow".to_string()))?;
                if packet_end > buf.len() {
                    return Err(PktError("truncated pkt-line".to_string()));
                }
                packets.push(Packet::Data(buf[header_end..packet_end].to_vec()));
                pos = packet_end;
            }
        }
    }
    Ok((packets, pos))
}

pub fn line_of(packet: &Packet) -> String {
    let Packet::Data(payload) = packet else {
        return String::new();
    };
    let mut text = String::from_utf8_lossy(payload).into_owned();
    if text.ends_with('\n') {
        text.pop();
    }
    text
}

pub fn sideband(band: u8, data: &[u8]) -> Vec<Vec<u8>> {
    let mut output = Vec::new();
    for chunk in data.chunks(SIDEBAND_MAX_DATA) {
        let mut payload = Vec::with_capacity(chunk.len() + 1);
        payload.push(band);
        payload.extend_from_slice(chunk);
        let packet = pkt(&payload);
        if !packet.is_empty() {
            output.push(packet);
        }
    }
    output
}

// ------------------------------------------------------------ trees, commits, tags

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    pub name: String,
    pub oid: String,
}

pub fn parse_tree(data: &[u8]) -> Result<Vec<TreeEntry>, String> {
    let mut entries = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let Some(space_relative) = data[pos..].iter().position(|byte| *byte == b' ') else {
            return Err("malformed tree object".to_string());
        };
        let space = pos + space_relative;
        let Some(nul_relative) = data[space + 1..].iter().position(|byte| *byte == 0) else {
            return Err("malformed tree object".to_string());
        };
        let nul = space + 1 + nul_relative;
        let oid_end = nul
            .checked_add(21)
            .ok_or_else(|| "malformed tree object".to_string())?;
        if oid_end > data.len() {
            return Err("malformed tree object".to_string());
        }
        let mode = data[pos..space]
            .iter()
            .map(|byte| char::from(byte & 0x7f))
            .collect();
        let name = String::from_utf8_lossy(&data[space + 1..nul]).into_owned();
        let oid = hex_encode(&data[nul + 1..oid_end]);
        entries.push(TreeEntry { mode, name, oid });
        pos = oid_end;
    }
    Ok(entries)
}

/// Git's tree order: byte order of the name, with directory names compared as if they ended in '/'.
pub fn encode_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut sorted: Vec<(Vec<u8>, &TreeEntry)> = entries
        .iter()
        .map(|entry| {
            let mut key = entry.name.as_bytes().to_vec();
            if entry.mode == "40000" {
                key.push(b'/');
            }
            (key, entry)
        })
        .collect();
    sorted.sort_by(|left, right| left.0.cmp(&right.0));

    let mut output = Vec::new();
    for (_, entry) in sorted {
        output.extend_from_slice(entry.mode.as_bytes());
        output.push(b' ');
        output.extend_from_slice(entry.name.as_bytes());
        output.push(0);
        output.extend_from_slice(&hex_decode_prefix(&entry.oid));
    }
    output
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub tree: String,
    pub parents: Vec<String>,
    pub author: String,
    pub committer: String,
    pub message: String,
}

pub fn parse_commit(data: &[u8]) -> Commit {
    let text = String::from_utf8_lossy(data);
    let (header, message) = text.split_once("\n\n").unwrap_or((&text, ""));
    let mut commit = Commit {
        tree: String::new(),
        parents: Vec::new(),
        author: String::new(),
        committer: String::new(),
        message: message.to_string(),
    };
    for line in header.split('\n') {
        if let Some(value) = line.strip_prefix("tree ") {
            commit.tree = value.to_string();
        } else if let Some(value) = line.strip_prefix("parent ") {
            commit.parents.push(value.to_string());
        } else if let Some(value) = line.strip_prefix("author ") {
            commit.author = value.to_string();
        } else if let Some(value) = line.strip_prefix("committer ") {
            commit.committer = value.to_string();
        }
    }
    commit
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub time: i64,
    pub tz: String,
}

pub fn parse_signature(text: &str) -> Signature {
    let fallback = || Signature {
        name: text.to_string(),
        email: String::new(),
        time: 0,
        tz: "+0000".to_string(),
    };
    if text.contains('\n') || text.contains('\r') || text.len() < 10 {
        return fallback();
    }
    let Some((before_tz, tz)) = text.rsplit_once(' ') else {
        return fallback();
    };
    let tz_bytes = tz.as_bytes();
    if tz_bytes.len() != 5
        || !(tz_bytes[0] == b'+' || tz_bytes[0] == b'-')
        || !tz_bytes[1..].iter().all(u8::is_ascii_digit)
    {
        return fallback();
    }
    let Some((before_time, time_text)) = before_tz.rsplit_once(' ') else {
        return fallback();
    };
    if time_text.is_empty() || !time_text.bytes().all(|byte| byte.is_ascii_digit()) {
        return fallback();
    }
    let Some((name, email_with_end)) = before_time.rsplit_once(" <") else {
        return fallback();
    };
    let Some(email) = email_with_end.strip_suffix('>') else {
        return fallback();
    };
    let Ok(time) = time_text.parse::<i64>() else {
        return fallback();
    };
    Signature {
        name: name.to_string(),
        email: email.to_string(),
        time,
        tz: tz.to_string(),
    }
}

pub fn format_signature(signature: &Signature) -> String {
    format!(
        "{} <{}> {} {}",
        signature.name, signature.email, signature.time, signature.tz
    )
}

pub fn encode_commit(
    tree: &str,
    parents: &[String],
    author: &Signature,
    committer: &Signature,
    message: &str,
) -> Vec<u8> {
    let mut output = String::new();
    output.push_str("tree ");
    output.push_str(tree);
    output.push('\n');
    for parent in parents {
        output.push_str("parent ");
        output.push_str(parent);
        output.push('\n');
    }
    output.push_str("author ");
    output.push_str(&format_signature(author));
    output.push_str("\ncommitter ");
    output.push_str(&format_signature(committer));
    output.push_str("\n\n");
    output.push_str(message);
    if !message.ends_with('\n') {
        output.push('\n');
    }
    output.into_bytes()
}

pub fn tag_target(data: &[u8]) -> (String, String) {
    let text = String::from_utf8_lossy(data);
    let mut object = String::new();
    let mut kind = String::new();
    for line in text.split('\n') {
        if object.is_empty() {
            if let Some(value) = line.strip_prefix("object ") {
                if is_oid(value) {
                    object = value.to_string();
                }
            }
        }
        if kind.is_empty() {
            if let Some(value) = line.strip_prefix("type ") {
                if !value.is_empty()
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    kind = value.to_string();
                }
            }
        }
    }
    (object, kind)
}

// ------------------------------------------------------------ packfiles

fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, String> {
    let mut pos = 0usize;
    let source_size = read_delta_varint(delta, &mut pos)?;
    let target_size = read_delta_varint(delta, &mut pos)?;
    if source_size != base.len() {
        return Err(format!(
            "delta base is {} bytes, the delta expects {source_size}",
            base.len()
        ));
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(target_size)
        .map_err(|error| format!("delta result allocation failed: {error}"))?;

    while pos < delta.len() {
        let opcode = delta[pos];
        pos += 1;
        if opcode & 0x80 != 0 {
            let mut offset = 0usize;
            let mut size = 0usize;
            for bit in 0..4 {
                if opcode & (1 << bit) != 0 {
                    let byte = *delta
                        .get(pos)
                        .ok_or_else(|| "truncated delta copy".to_string())?;
                    pos += 1;
                    offset = offset
                        .checked_add(usize::from(byte) << (8 * bit))
                        .ok_or_else(|| "delta copy offset overflow".to_string())?;
                }
            }
            for bit in 0..3 {
                if opcode & (1 << (4 + bit)) != 0 {
                    let byte = *delta
                        .get(pos)
                        .ok_or_else(|| "truncated delta copy".to_string())?;
                    pos += 1;
                    size = size
                        .checked_add(usize::from(byte) << (8 * bit))
                        .ok_or_else(|| "delta copy size overflow".to_string())?;
                }
            }
            if size == 0 {
                size = 0x1_0000;
            }
            let end = offset
                .checked_add(size)
                .ok_or_else(|| "delta copy range overflow".to_string())?;
            let written = output
                .len()
                .checked_add(size)
                .ok_or_else(|| "delta result size overflow".to_string())?;
            if end > base.len() || written > target_size {
                return Err("delta copy out of range".to_string());
            }
            output.extend_from_slice(&base[offset..end]);
        } else if opcode != 0 {
            let count = usize::from(opcode);
            let insert_end = pos
                .checked_add(count)
                .ok_or_else(|| "delta insert range overflow".to_string())?;
            let written = output
                .len()
                .checked_add(count)
                .ok_or_else(|| "delta result size overflow".to_string())?;
            if insert_end > delta.len() || written > target_size {
                return Err("delta insert out of range".to_string());
            }
            output.extend_from_slice(&delta[pos..insert_end]);
            pos = insert_end;
        } else {
            return Err("delta opcode 0 is reserved".to_string());
        }
    }
    if output.len() != target_size {
        return Err("delta produced the wrong size".to_string());
    }
    Ok(output)
}

fn read_delta_varint(delta: &[u8], pos: &mut usize) -> Result<usize, String> {
    let mut value = 0usize;
    let mut shift = 0u32;
    loop {
        let byte = *delta
            .get(*pos)
            .ok_or_else(|| "truncated delta header".to_string())?;
        *pos += 1;
        let part = checked_varint_part(byte & 0x7f, shift)
            .ok_or_else(|| "delta size overflow".to_string())?;
        value = value
            .checked_add(part)
            .ok_or_else(|| "delta size overflow".to_string())?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift = shift
            .checked_add(7)
            .filter(|next| *next < usize::BITS)
            .ok_or_else(|| "delta size overflow".to_string())?;
    }
}

struct RawPackEntry {
    code: u8,
    data: Vec<u8>,
    base_offset: Option<usize>,
    base_oid: Option<String>,
    resolved: Option<GitObject>,
}

fn checked_varint_part(value: u8, shift: u32) -> Option<usize> {
    let value = usize::from(value);
    if shift >= usize::BITS || value > (usize::MAX >> shift) {
        None
    } else {
        Some(value << shift)
    }
}

fn inflate_at(
    input: &[u8],
    offset: usize,
    expected_size: usize,
) -> Result<(Vec<u8>, usize), String> {
    if offset > input.len() {
        return Err("truncated pack entry".to_string());
    }
    let mut decoder = ZlibDecoder::new(&input[offset..]);
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = decoder
            .read(&mut buffer)
            .map_err(|error| format!("invalid pack zlib stream: {error}"))?;
        if count == 0 {
            break;
        }
        let new_len = data
            .len()
            .checked_add(count)
            .ok_or_else(|| "inflated pack object size overflow".to_string())?;
        if new_len > expected_size {
            return Err("pack entry inflated to the wrong size".to_string());
        }
        data.try_reserve(count)
            .map_err(|error| format!("inflated pack object allocation failed: {error}"))?;
        data.extend_from_slice(&buffer[..count]);
    }
    if data.len() != expected_size {
        return Err("pack entry inflated to the wrong size".to_string());
    }
    let consumed = usize::try_from(decoder.total_in())
        .map_err(|_| "pack zlib stream length overflow".to_string())?;
    if consumed == 0 {
        return Err("empty pack zlib stream".to_string());
    }
    let next = offset
        .checked_add(consumed)
        .filter(|end| *end <= input.len())
        .ok_or_else(|| "truncated pack zlib stream".to_string())?;
    Ok((data, next))
}

/// Read a pack (version 2 or 3) and resolve every object. `external` supplies REF_DELTA bases that
/// were omitted from a thin pack but are already present in the repository.
pub fn read_pack(
    pack: &[u8],
    external: &dyn Fn(&str) -> Option<GitObject>,
) -> Result<HashMap<String, GitObject>, String> {
    if pack.len() < 32 || &pack[..4] != b"PACK" {
        return Err("not a packfile".to_string());
    }
    let version = read_be_u32(pack, 4)?;
    if version != 2 && version != 3 {
        return Err(format!("unsupported pack version {version}"));
    }
    let count = usize::try_from(read_be_u32(pack, 8)?)
        .map_err(|_| "pack object count overflow".to_string())?;
    let end = pack.len() - 20;
    if count > end.saturating_sub(12) {
        return Err("pack object count exceeds available data".to_string());
    }
    if sha1(&pack[..end]) != pack[end..] {
        return Err("pack checksum mismatch".to_string());
    }

    let body = &pack[..end];
    let mut raws = Vec::new();
    raws.try_reserve(count)
        .map_err(|error| format!("pack entry allocation failed: {error}"))?;
    let mut by_offset = HashMap::new();
    by_offset
        .try_reserve(count)
        .map_err(|error| format!("pack index allocation failed: {error}"))?;
    let mut pos = 12usize;

    for index in 0..count {
        let start = pos;
        let mut byte = *body
            .get(pos)
            .ok_or_else(|| format!("truncated pack entry {index} header"))?;
        pos += 1;
        let code = (byte >> 4) & 7;
        let mut size = usize::from(byte & 0x0f);
        let mut shift = 4u32;
        while byte & 0x80 != 0 {
            byte = *body
                .get(pos)
                .ok_or_else(|| format!("truncated pack entry {index} size"))?;
            pos += 1;
            let part = checked_varint_part(byte & 0x7f, shift)
                .ok_or_else(|| format!("pack entry {index} size overflow"))?;
            size = size
                .checked_add(part)
                .ok_or_else(|| format!("pack entry {index} size overflow"))?;
            if byte & 0x80 != 0 {
                shift = shift
                    .checked_add(7)
                    .filter(|next| *next < usize::BITS)
                    .ok_or_else(|| format!("pack entry {index} size overflow"))?;
            }
        }

        let mut raw = RawPackEntry {
            code,
            data: Vec::new(),
            base_offset: None,
            base_oid: None,
            resolved: None,
        };
        match code {
            6 => {
                let first = *body
                    .get(pos)
                    .ok_or_else(|| format!("truncated OFS_DELTA base in entry {index}"))?;
                pos += 1;
                let mut value = usize::from(first & 0x7f);
                byte = first;
                while byte & 0x80 != 0 {
                    byte = *body
                        .get(pos)
                        .ok_or_else(|| format!("truncated OFS_DELTA base in entry {index}"))?;
                    pos += 1;
                    value = value
                        .checked_add(1)
                        .and_then(|value| value.checked_mul(128))
                        .and_then(|value| value.checked_add(usize::from(byte & 0x7f)))
                        .ok_or_else(|| format!("OFS_DELTA offset overflow in entry {index}"))?;
                }
                if value == 0 {
                    return Err(format!("invalid OFS_DELTA offset in entry {index}"));
                }
                raw.base_offset = Some(
                    start
                        .checked_sub(value)
                        .ok_or_else(|| format!("OFS_DELTA base before pack in entry {index}"))?,
                );
            }
            7 => {
                let oid_end = pos
                    .checked_add(20)
                    .ok_or_else(|| format!("REF_DELTA base offset overflow in entry {index}"))?;
                if oid_end > body.len() {
                    return Err(format!("truncated REF_DELTA base in entry {index}"));
                }
                raw.base_oid = Some(hex_encode(&body[pos..oid_end]));
                pos = oid_end;
            }
            1..=4 => {}
            _ => return Err(format!("pack entry {index} has unknown type {code}")),
        }

        let (data, next) =
            inflate_at(body, pos, size).map_err(|error| format!("pack entry {index}: {error}"))?;
        raw.data = data;
        pos = next;
        by_offset.insert(start, raws.len());
        raws.push(raw);
    }
    if pos != end {
        return Err("pack has trailing bytes before its checksum".to_string());
    }

    let mut objects = HashMap::new();
    let mut pending = Vec::new();
    for (index, raw) in raws.iter_mut().enumerate() {
        if raw.code == 6 || raw.code == 7 {
            pending.push(index);
        } else {
            let kind = ObjType::from_code(raw.code)
                .ok_or_else(|| format!("pack entry {index} has unknown type {}", raw.code))?;
            let object = GitObject {
                kind,
                data: std::mem::take(&mut raw.data),
            };
            objects.insert(hash_object(object.kind, &object.data), object.clone());
            raw.resolved = Some(object);
        }
    }

    while !pending.is_empty() {
        let mut retry = Vec::new();
        let mut progress = false;
        for index in pending {
            let raw = &raws[index];
            let base = if raw.code == 6 {
                let base_offset = raw
                    .base_offset
                    .ok_or_else(|| "OFS_DELTA base is not in the pack".to_string())?;
                let base_index = by_offset
                    .get(&base_offset)
                    .ok_or_else(|| "ofs-delta base is not in the pack".to_string())?;
                raws[*base_index].resolved.clone()
            } else {
                let oid = raw.base_oid.as_deref().unwrap_or_default();
                objects.get(oid).cloned().or_else(|| external(oid))
            };
            let Some(base) = base else {
                retry.push(index);
                continue;
            };
            let result = apply_delta(&base.data, &raw.data)?;
            let object = GitObject {
                kind: base.kind,
                data: result,
            };
            let oid = hash_object(object.kind, &object.data);
            objects.insert(oid, object.clone());
            raws[index].resolved = Some(object);
            progress = true;
        }
        if !progress {
            if let Some(index) = retry.iter().copied().find(|index| raws[*index].code == 7) {
                let oid = raws[index].base_oid.as_deref().unwrap_or("?");
                let found_base = objects.contains_key(oid) || external(oid).is_some();
                if !found_base {
                    return Err(format!("ref-delta base {oid} is missing"));
                }
            }
            return Err("delta cycle in pack".to_string());
        }
        pending = retry;
    }
    Ok(objects)
}

/// One non-delta object's pack entry (type/size header plus a zlib stream).
pub fn pack_entry(object: &GitObject) -> Vec<u8> {
    let mut output = Vec::new();
    let mut size = object.data.len();
    let mut first = (object.kind.type_code() << 4) | (size as u8 & 0x0f);
    size >>= 4;
    while size > 0 {
        output.push(first | 0x80);
        first = (size & 0x7f) as u8;
        size >>= 7;
    }
    output.push(first);

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    if encoder.write_all(&object.data).is_err() {
        return Vec::new();
    }
    match encoder.finish() {
        Ok(compressed) => {
            output.extend_from_slice(&compressed);
            output
        }
        Err(_) => Vec::new(),
    }
}

/// Write a version-2 pack from prebuilt entries in the order supplied.
pub fn write_pack(entries: &[Vec<u8>]) -> Vec<u8> {
    let Ok(count) = u32::try_from(entries.len()) else {
        return Vec::new();
    };
    let capacity = match entries
        .iter()
        .try_fold(32usize, |total, entry| total.checked_add(entry.len()))
    {
        Some(capacity) => capacity,
        None => return Vec::new(),
    };
    let mut pack = Vec::new();
    if pack.try_reserve_exact(capacity).is_err() {
        return Vec::new();
    }
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&count.to_be_bytes());
    for entry in entries {
        pack.extend_from_slice(entry);
    }
    let checksum = sha1(&pack);
    pack.extend_from_slice(&checksum);
    pack
}

fn read_be_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| "pack header offset overflow".to_string())?;
    let slice = bytes
        .get(offset..end)
        .ok_or_else(|| "truncated pack header".to_string())?;
    Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn hex_decode_prefix(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len() / 2);
    let mut pos = 0;
    while pos + 1 < bytes.len() {
        let (Some(high), Some(low)) = (hex_value(bytes[pos]), hex_value(bytes[pos + 1])) else {
            break;
        };
        output.push((high << 4) | low);
        pos += 2;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_object_matches_empty_blob_oid() {
        assert_eq!(
            hash_object(ObjType::Blob, b""),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            hex_encode(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn pkt_read_packets_round_trip() {
        let mut encoded = pkt(b"hello\n");
        encoded.extend_from_slice(&delim());
        encoded.extend_from_slice(&flush());
        let (packets, next) = read_packets(&encoded, 0, false).expect("valid pkt-lines");
        assert_eq!(next, encoded.len());
        assert_eq!(packets[0], Packet::Data(b"hello\n".to_vec()));
        assert_eq!(line_of(&packets[0]), "hello");
        assert_eq!(packets[1], Packet::Delim);
        assert_eq!(packets[2], Packet::Flush);
    }

    #[test]
    fn tree_round_trip() {
        let entries = vec![
            TreeEntry {
                mode: "100644".to_string(),
                name: "file".to_string(),
                oid: "0123456789012345678901234567890123456789".to_string(),
            },
            TreeEntry {
                mode: "40000".to_string(),
                name: "dir".to_string(),
                oid: "fedcba9876543210fedcba9876543210fedcba98".to_string(),
            },
        ];
        let encoded = encode_tree(&entries);
        let parsed = parse_tree(&encoded).expect("encoded tree parses");
        assert_eq!(parsed, vec![entries[1].clone(), entries[0].clone()]);
        assert_eq!(encode_tree(&parsed), encoded);
    }

    #[test]
    fn pack_entries_and_pack_round_trip_three_objects() {
        let blob = GitObject {
            kind: ObjType::Blob,
            data: b"hello\n".to_vec(),
        };
        let blob_oid = hash_object(blob.kind, &blob.data);
        let tree = GitObject {
            kind: ObjType::Tree,
            data: encode_tree(&[TreeEntry {
                mode: "100644".to_string(),
                name: "hello.txt".to_string(),
                oid: blob_oid,
            }]),
        };
        let tree_oid = hash_object(tree.kind, &tree.data);
        let commit = GitObject {
            kind: ObjType::Commit,
            data: format!("tree {tree_oid}\n\nmessage\n").into_bytes(),
        };
        let expected: Vec<(String, GitObject)> = [blob, tree, commit]
            .into_iter()
            .map(|object| (hash_object(object.kind, &object.data), object))
            .collect();
        let entries: Vec<Vec<u8>> = expected
            .iter()
            .map(|(_, object)| pack_entry(object))
            .collect();
        let pack = write_pack(&entries);
        let objects = read_pack(&pack, &|_| None).expect("pack parses");
        assert_eq!(objects.len(), 3);
        for (oid, object) in expected {
            assert_eq!(objects.get(&oid), Some(&object));
        }
    }

    #[test]
    fn write_pack_trailer_is_sha1_of_prefix() {
        let entries = vec![pack_entry(&GitObject {
            kind: ObjType::Blob,
            data: b"trailer test".to_vec(),
        })];
        let pack = write_pack(&entries);
        assert_eq!(&pack[..4], b"PACK");
        assert_eq!(&pack[pack.len() - 20..], &sha1(&pack[..pack.len() - 20]));
    }
}
