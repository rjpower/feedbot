//! Give an iepub-built MOBI the NCX index a Kindle needs for its chapter list.
//!
//! iepub writes only an inline HTML table of contents; the Kindle's native
//! navigation (the "Go To" chapter list) reads a separate NCX *index* — a set
//! of `INDX`/`TAGX`/`CNCX` records the MOBI header points at via
//! `indx_record_offset`. Without it the chapter list shows only
//! cover/beginning/end. This splices that index in after the fact.
//!
//! The record layout and byte encodings follow Calibre's `writer2/indexer.py`
//! flat-book path exactly, so the result is what a Kindle already knows how to
//! read. The chapter offsets and titles are recovered from the inline TOC iepub
//! embeds (`<blockquote ...><a filepos=NNNNNNNNNN>title</a></blockquote>`),
//! which — because iepub leaves the text uncompressed — are byte offsets into
//! the concatenated text records.
//!
//! Anything unexpected (compressed text, an index already present, no parseable
//! chapters) makes [`add_toc`] return the book untouched: a missing chapter list
//! is a small loss, a corrupted book is not.

use anyhow::{Context, Result, ensure};
use std::sync::LazyLock;

/// Fixed INDX header length used throughout, matching KindleGen/Calibre.
const HDR: usize = 192;

/// MOBI-header field offsets, relative to the start of record 0 (the MOBIDOC
/// header; the MOBI header itself begins 16 bytes in). Derived from iepub's
/// writer, verified against real output.
const OFF_COMPRESSION: usize = 0; // MOBIDOC header
const OFF_TEXT_LENGTH: usize = 4;
const OFF_TEXT_RECORDS: usize = 8;
const OFF_FIRST_NON_BOOK: usize = 80;
const OFF_FIRST_IMAGE: usize = 108;
const OFF_FCIS_REC: usize = 200;
const OFF_FLIS_REC: usize = 208;
const OFF_INDX_REC: usize = 244;

static BLOCKQUOTE: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(
        r#"<blockquote height="0pt" width="0pt"><a filepos=(\d{10})>(.*?)</a></blockquote>"#,
    )
    .unwrap()
});

// ---------------------------------------------------------------------------
// Byte encodings (ports of calibre.ebooks.mobi.utils)
// ---------------------------------------------------------------------------

/// Variable-width integer, big-endian, 7 bits/byte, high bit set on the last
/// byte (calibre's forward `encint`).
fn encint(mut value: u32) -> Vec<u8> {
    let mut byts = Vec::new();
    loop {
        byts.push((value & 0x7f) as u8);
        value >>= 7;
        if value == 0 {
            break;
        }
    }
    byts[0] |= 0x80;
    byts.reverse();
    byts
}

/// A single-byte length prefix followed by the bytes.
fn encode_string(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 1);
    out.push(raw.len() as u8);
    out.extend_from_slice(raw);
    out
}

/// `num` as even-length uppercase hex, length-prefixed — the id form INDX
/// entries carry.
fn encode_number_as_hex(num: usize) -> Vec<u8> {
    let mut h = format!("{num:X}").into_bytes();
    if !h.len().is_multiple_of(2) {
        h.insert(0, b'0');
    }
    encode_string(&h)
}

fn align4(mut v: Vec<u8>) -> Vec<u8> {
    while !v.len().is_multiple_of(4) {
        v.push(0);
    }
    v
}

// ---------------------------------------------------------------------------
// CNCX — the label strings, addressed by byte offset
// ---------------------------------------------------------------------------

struct Cncx {
    records: Vec<Vec<u8>>,
    /// title bytes -> their offset in the CNCX address space
    offsets: std::collections::HashMap<Vec<u8>, u32>,
}

fn build_cncx(labels: &[Vec<u8>]) -> Cncx {
    const LIMIT: usize = 0x10000 - 1024;
    let mut records = Vec::new();
    let mut offsets = std::collections::HashMap::new();
    let mut buf = Vec::new();
    let mut offset: u32 = 0;
    for label in labels {
        if offsets.contains_key(label) {
            continue; // strings are stored once, shared by every entry using them
        }
        let mut s = label.clone();
        truncate_utf8(&mut s, 500);
        let mut raw = encint(s.len() as u32);
        raw.extend_from_slice(&s);
        if buf.len() + raw.len() > LIMIT {
            records.push(align4(std::mem::take(&mut buf)));
            offset = records.len() as u32 * 0x10000;
        }
        offsets.insert(label.clone(), offset);
        offset += raw.len() as u32;
        buf.extend_from_slice(&raw);
    }
    if !buf.is_empty() {
        records.push(align4(buf));
    }
    Cncx { records, offsets }
}

/// Shrink `s` to at most `max` bytes without splitting a UTF-8 sequence.
fn truncate_utf8(s: &mut Vec<u8>, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && (s[end] & 0xC0) == 0x80 {
        end -= 1; // back off continuation bytes
    }
    s.truncate(end);
}

// ---------------------------------------------------------------------------
// INDX records
// ---------------------------------------------------------------------------

/// TAGX for a flat book index: tags offset(1), size(2), label(3), depth(4).
fn flat_tagx() -> Vec<u8> {
    let mut byts = Vec::new();
    for (tag, nvals, mask, eof) in [
        (1u8, 1u8, 1u8, 0u8),
        (2, 1, 2, 0),
        (3, 1, 4, 0),
        (4, 1, 8, 0),
        (0, 0, 0, 1),
    ] {
        byts.extend_from_slice(&[tag, nvals, mask, eof]);
    }
    let mut out = b"TAGX".to_vec();
    out.extend_from_slice(&((12 + byts.len()) as u32).to_be_bytes());
    out.extend_from_slice(&1u32.to_be_bytes()); // one control byte
    out.extend_from_slice(&byts);
    out
}

/// One flat entry: id, control byte (tags 1|2|4|8 present), then offset, size,
/// label offset, depth as vwis.
fn entry_bytes(index: usize, offset: u32, size: u32, label_off: u32) -> Vec<u8> {
    let mut o = encode_number_as_hex(index);
    o.push(0x0F);
    o.extend(encint(offset));
    o.extend(encint(size));
    o.extend(encint(label_off));
    o.extend(encint(0)); // depth
    o
}

fn index_record(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut positions = Vec::with_capacity(entries.len());
    let mut block = Vec::new();
    for e in entries {
        positions.push(block.len());
        block.extend_from_slice(e);
    }
    let index_block = align4(block);

    let mut idxt = b"IDXT".to_vec();
    for &p in &positions {
        idxt.extend_from_slice(&((HDR + p) as u16).to_be_bytes());
    }
    let idxt_block = align4(idxt);

    let mut h = Vec::with_capacity(HDR);
    h.extend_from_slice(b"INDX");
    h.extend_from_slice(&(HDR as u32).to_be_bytes());
    h.extend_from_slice(&[0; 4]);
    h.extend_from_slice(&1u32.to_be_bytes());
    h.extend_from_slice(&[0; 4]);
    h.extend_from_slice(&((HDR + index_block.len()) as u32).to_be_bytes()); // IDXT offset
    h.extend_from_slice(&(positions.len() as u32).to_be_bytes()); // entry count
    h.extend_from_slice(&[0xff; 8]);
    h.extend_from_slice(&[0; 156]);
    debug_assert_eq!(h.len(), HDR);

    h.extend(index_block);
    h.extend(idxt_block);
    h
}

fn header_record(num_entries: usize, num_cncx: usize, last_index: usize) -> Vec<u8> {
    let tagx = flat_tagx();
    let mut b = Vec::with_capacity(HDR + tagx.len() + 32);
    b.extend_from_slice(b"INDX");
    b.extend_from_slice(&(HDR as u32).to_be_bytes());
    b.extend_from_slice(&[0; 8]);
    b.extend_from_slice(&2u32.to_be_bytes()); // index type
    b.extend_from_slice(&0u32.to_be_bytes()); // IDXT offset, patched below
    b.extend_from_slice(&1u32.to_be_bytes()); // number of index (data) records
    b.extend_from_slice(&65001u32.to_be_bytes()); // utf-8
    b.extend_from_slice(&[0xff; 4]);
    b.extend_from_slice(&(num_entries as u32).to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // ORDT
    b.extend_from_slice(&0u32.to_be_bytes()); // LIGT
    b.extend_from_slice(&0u32.to_be_bytes()); // nLIGT
    b.extend_from_slice(&(num_cncx as u32).to_be_bytes());
    b.extend_from_slice(&[0; 124]);
    b.extend_from_slice(&(HDR as u32).to_be_bytes()); // TAGX offset
    b.extend_from_slice(&[0; 8]);
    debug_assert_eq!(b.len(), HDR);

    b.extend_from_slice(&tagx);
    b.extend(encode_number_as_hex(last_index));
    b.extend_from_slice(&(num_entries as u16).to_be_bytes());
    while !b.len().is_multiple_of(4) {
        b.push(0);
    }
    let idxt_off = b.len() as u32;
    b.extend_from_slice(b"IDXT");
    b.extend_from_slice(&((HDR + tagx.len()) as u16).to_be_bytes());
    b.push(0);
    b[20..24].copy_from_slice(&idxt_off.to_be_bytes());
    align4(b)
}

// ---------------------------------------------------------------------------
// Splice
// ---------------------------------------------------------------------------

/// Add an NCX index to `mobi` (an iepub-produced Mobipocket book) and return the
/// new bytes. Returns the input unchanged if the book already has an index, its
/// text is compressed, or no chapters can be recovered.
pub fn add_toc(mobi: &[u8]) -> Result<Vec<u8>> {
    ensure!(mobi.len() > 78, "not a mobi");
    let n = u16::from_be_bytes([mobi[76], mobi[77]]) as usize;
    ensure!(n > 3 && mobi.len() >= 78 + n * 8, "truncated record table");

    let mut offs = Vec::with_capacity(n + 1);
    for i in 0..n {
        let p = 78 + i * 8;
        offs.push(u32::from_be_bytes(mobi[p..p + 4].try_into().unwrap()) as usize);
    }
    offs.push(mobi.len());
    let rec0 = offs[0];
    let ru32 = |o: usize| u32::from_be_bytes(mobi[rec0 + o..rec0 + o + 4].try_into().unwrap());

    if ru32(OFF_INDX_REC) != 0xffff_ffff {
        return Ok(mobi.to_vec()); // already indexed
    }
    let compression = u16::from_be_bytes([mobi[rec0 + OFF_COMPRESSION], mobi[rec0 + OFF_COMPRESSION + 1]]);
    ensure!(compression == 1, "text is compressed; refusing to add an index");

    let text_len = ru32(OFF_TEXT_LENGTH) as usize;
    let text_recs = u16::from_be_bytes([mobi[rec0 + OFF_TEXT_RECORDS], mobi[rec0 + OFF_TEXT_RECORDS + 1]]) as usize;
    let first_image = ru32(OFF_FIRST_IMAGE) as usize;
    ensure!(first_image > text_recs && first_image < n, "unexpected image layout");

    // Recover chapter (offset, title) from iepub's inline TOC in the text.
    let mut text = Vec::with_capacity(text_len);
    for i in 1..=text_recs {
        text.extend_from_slice(&mobi[offs[i]..offs[i + 1]]);
    }
    text.truncate(text_len);

    let mut chapters: Vec<(u32, Vec<u8>)> = BLOCKQUOTE
        .captures_iter(&text)
        .filter_map(|c| {
            let off: u32 = std::str::from_utf8(&c[1]).ok()?.parse().ok()?;
            Some((off, c[2].to_vec()))
        })
        .filter(|(off, _)| (*off as usize) < text_len)
        .collect();
    if chapters.is_empty() {
        return Ok(mobi.to_vec()); // single-article export, or nothing to index
    }
    chapters.sort_by_key(|(off, _)| *off);
    chapters.dedup_by_key(|(off, _)| *off);

    // Build CNCX + flat index entries.
    let labels: Vec<Vec<u8>> = chapters.iter().map(|(_, t)| t.clone()).collect();
    let cncx = build_cncx(&labels);
    let entries: Vec<Vec<u8>> = chapters
        .iter()
        .enumerate()
        .map(|(i, (off, title))| {
            let next = chapters.get(i + 1).map_or(text_len as u32, |(o, _)| *o);
            let label_off = cncx.offsets[title];
            entry_bytes(i, *off, next - *off, label_off)
        })
        .collect();

    let ncx_records: Vec<Vec<u8>> = std::iter::once(header_record(entries.len(), cncx.records.len(), entries.len() - 1))
        .chain(std::iter::once(index_record(&entries)))
        .chain(cncx.records)
        .collect();
    let k = ncx_records.len();

    // Insert the NCX block right before the first image record.
    let p = first_image;
    let mut records: Vec<Vec<u8>> = Vec::with_capacity(n + k);
    for i in 0..p {
        records.push(mobi[offs[i]..offs[i + 1]].to_vec());
    }
    records.extend(ncx_records);
    for i in p..n {
        records.push(mobi[offs[i]..offs[i + 1]].to_vec());
    }
    let new_n = records.len();

    let flis = records.iter().position(|r| r.starts_with(b"FLIS")).context("no FLIS record")?;
    let fcis = records.iter().position(|r| r.starts_with(b"FCIS")).context("no FCIS record")?;

    // Repoint the MOBI header (record 0), blob-relative.
    let r0 = &mut records[0];
    ensure!(r0.len() > OFF_INDX_REC + 4, "record 0 shorter than the MOBI header");
    r0[OFF_FIRST_IMAGE..OFF_FIRST_IMAGE + 4].copy_from_slice(&((p + k) as u32).to_be_bytes());
    r0[OFF_INDX_REC..OFF_INDX_REC + 4].copy_from_slice(&(p as u32).to_be_bytes());
    r0[OFF_FIRST_NON_BOOK..OFF_FIRST_NON_BOOK + 4].copy_from_slice(&(p as u32).to_be_bytes());
    r0[OFF_FCIS_REC..OFF_FCIS_REC + 4].copy_from_slice(&(fcis as u32).to_be_bytes());
    r0[OFF_FLIS_REC..OFF_FLIS_REC + 4].copy_from_slice(&(flis as u32).to_be_bytes());

    // Reassemble the PalmDB: header, new record table, filler, records.
    let gap = rec0 - (78 + n * 8);
    let mut out = Vec::with_capacity(mobi.len() + k * 4096);
    out.extend_from_slice(&mobi[..76]);
    out.extend_from_slice(&(new_n as u16).to_be_bytes());
    let mut cur = 78 + new_n * 8 + gap;
    for (i, r) in records.iter().enumerate() {
        out.extend_from_slice(&(cur as u32).to_be_bytes());
        out.push(0); // record attributes
        out.extend_from_slice(&((i * 2) as u32).to_be_bytes()[1..]); // 3-byte unique id
        cur += r.len();
    }
    out.extend(std::iter::repeat_n(0u8, gap));
    for r in &records {
        out.extend_from_slice(r);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encint_matches_known_vectors() {
        assert_eq!(encint(0), vec![0x80]);
        assert_eq!(encint(0x7f), vec![0xff]);
        assert_eq!(encint(0x80), vec![0x01, 0x80]);
        // calibre's documented example: 0x11111 -> 0x04 0x22 0x91
        assert_eq!(encint(0x11111), vec![0x04, 0x22, 0x91]);
    }

    #[test]
    fn hex_ids_are_even_length_and_prefixed() {
        assert_eq!(encode_number_as_hex(0), vec![2, b'0', b'0']);
        assert_eq!(encode_number_as_hex(0x1a), vec![2, b'1', b'A']);
        assert_eq!(encode_number_as_hex(0x100), vec![4, b'0', b'1', b'0', b'0']);
    }

    #[test]
    fn tagx_is_well_formed() {
        let t = flat_tagx();
        assert_eq!(&t[..4], b"TAGX");
        // table length = 12 + 5*4 = 32, one control byte
        assert_eq!(u32::from_be_bytes(t[4..8].try_into().unwrap()), 32);
        assert_eq!(u32::from_be_bytes(t[8..12].try_into().unwrap()), 1);
        assert_eq!(t.len(), 32);
    }

    #[test]
    fn header_and_index_records_carry_their_magic_and_counts() {
        let entries: Vec<Vec<u8>> = (0..3).map(|i| entry_bytes(i, i as u32 * 10, 10, 0)).collect();
        let hdr = header_record(entries.len(), 1, entries.len() - 1);
        assert_eq!(&hdr[..4], b"INDX");
        assert_eq!(u32::from_be_bytes(hdr[36..40].try_into().unwrap()), 3); // entry count
        assert_eq!(u32::from_be_bytes(hdr[52..56].try_into().unwrap()), 1); // cncx count
        let idx = index_record(&entries);
        assert_eq!(&idx[..4], b"INDX");
        assert_eq!(u32::from_be_bytes(idx[24..28].try_into().unwrap()), 3); // count field
    }

    #[test]
    fn cncx_dedupes_and_addresses_strings() {
        let labels: Vec<Vec<u8>> = vec![b"alpha".to_vec(), b"beta".to_vec(), b"alpha".to_vec()];
        let c = build_cncx(&labels);
        assert_eq!(c.records.len(), 1);
        assert_eq!(c.offsets[&b"alpha".to_vec()], 0);
        // "alpha" -> encint(5)=1 byte + 5 bytes = 6; "beta" starts at 6
        assert_eq!(c.offsets[&b"beta".to_vec()], 6);
    }

    #[test]
    fn add_toc_leaves_a_non_mobi_alone() {
        // No valid record table -> returned unchanged rather than panicking.
        assert!(add_toc(b"not a mobi at all, really").is_err());
    }
}
