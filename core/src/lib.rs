//! Axmzip Core v5.1 — Axiom-Based Binary Compression
//!
//! Changes over v5.0:
//!   - Original filename stored in header → decompress restores correct extension
//!   - Real progress reporting via Arc<Mutex<f32>> (0.0–1.0)
//!   - Progress updates inside pattern scan loop (was the stuck-at-30% bug)
//!   - Cleaner public API with ProgressFn type alias

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use md5;

// ─────────────────────────────────────────────────────────────────
// PUBLIC TYPES
// ─────────────────────────────────────────────────────────────────

/// Shared progress float — set by worker, read by UI. Range 0.0–1.0.
pub type Progress = Arc<Mutex<f32>>;

pub fn new_progress() -> Progress { Arc::new(Mutex::new(0.0)) }

fn set_progress(p: &Option<Progress>, v: f32) {
    if let Some(arc) = p { if let Ok(mut g) = arc.lock() { *g = v.clamp(0.0, 1.0); } }
}

#[derive(Debug, Clone)]
pub struct CompressStats {
    pub original_bytes:   usize,
    pub compressed_bytes: usize,
    pub ratio_pct:        f64,
    pub mode:             String,
    pub lossless:         bool,
    pub psnr_db:          f64,
    pub max_error:        u8,
    pub lib_entries:      usize,
    pub elapsed_ms:       u64,
}

#[derive(Debug, Clone)]
pub struct DecompressResult {
    pub data:              Vec<u8>,
    pub original_filename: Option<String>,  // restored from header
}

#[derive(Debug)]
pub enum AxmzipError {
    BadMagic,
    BadVersion,
    ChecksumMismatch,
    InvalidData(String),
}

impl std::fmt::Display for AxmzipError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            AxmzipError::BadMagic         => write!(f, "Not an axmzip file (bad magic)"),
            AxmzipError::BadVersion       => write!(f, "Unsupported format version"),
            AxmzipError::ChecksumMismatch => write!(f, "Checksum mismatch — file may be corrupted"),
            AxmzipError::InvalidData(s)   => write!(f, "Invalid data: {s}"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────────────────────────

const MAGIC:        &[u8] = b"AXM5";
const VERSION:      u8    = 0x02;  // bumped: filename now in header
const INNER_MAGIC:  &[u8] = b"AXi\x05";

const MODE_PLAIN:   u8 = 0x00;
const MODE_STRIDE:  u8 = 0x01;
const MODE_LOSSY:   u8 = 0x02;

const MAX_ATOMICS:  usize = 256;
const MAX_PASSES:   usize = 12;
const MAX_PAT_LEN:  usize = 64;

// ─────────────────────────────────────────────────────────────────
// VARINT (LEB128)
// ─────────────────────────────────────────────────────────────────

fn encode_varint(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let b = (n & 0x7F) as u8;
        n >>= 7;
        if n > 0 { out.push(b | 0x80); } else { out.push(b); break; }
    }
    out
}

fn decode_varint(data: &[u8], offset: usize) -> Result<(u64, usize), AxmzipError> {
    let mut result = 0u64;
    let mut shift  = 0u32;
    let mut off    = offset;
    loop {
        if off >= data.len() {
            return Err(AxmzipError::InvalidData("Unexpected end of varint".into()));
        }
        let b = data[off]; off += 1;
        result |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 { break; }
        shift += 7;
        if shift > 63 { return Err(AxmzipError::InvalidData("Varint too large".into())); }
    }
    Ok((result, off))
}

fn varint_len(mut n: u64) -> usize {
    let mut l = 1;
    while n >= 0x80 { n >>= 7; l += 1; }
    l
}

// ─────────────────────────────────────────────────────────────────
// ENTROPY
// ─────────────────────────────────────────────────────────────────

fn entropy(data: &[u8]) -> f64 {
    if data.is_empty() { return 0.0; }
    let mut freq = [0u64; 256];
    for &b in data { freq[b as usize] += 1; }
    let n = data.len() as f64;
    freq.iter()
        .filter(|&&c| c > 0)
        .map(|&c| { let p = c as f64 / n; -p * p.log2() })
        .sum()
}

// ─────────────────────────────────────────────────────────────────
// DELTA FILTERS
// ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u8)]
enum Filter { None=0, Delta1=1, Delta2=2, DeltaRgb=3, DeltaRgba=4 }

impl Filter {
    fn from_u8(v: u8) -> Self {
        match v { 1=>Self::Delta1, 2=>Self::Delta2, 3=>Self::DeltaRgb, 4=>Self::DeltaRgba, _=>Self::None }
    }
}

fn delta1(data: &[u8]) -> Vec<u8> {
    let mut o = data.to_vec();
    for i in (1..data.len()).rev() { o[i] = data[i].wrapping_sub(data[i-1]); }
    o
}
fn undelta1(data: &[u8]) -> Vec<u8> {
    let mut o = data.to_vec();
    for i in 1..data.len() { o[i] = o[i].wrapping_add(o[i-1]); }
    o
}
fn delta2(data: &[u8]) -> Vec<u8> {
    if data.len() % 2 != 0 { return data.to_vec(); }
    let words: Vec<u16> = (0..data.len()).step_by(2)
        .map(|i| u16::from_be_bytes([data[i], data[i+1]])).collect();
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&words[0].to_be_bytes());
    for i in 1..words.len() {
        out.extend_from_slice(&words[i].wrapping_sub(words[i-1]).to_be_bytes());
    }
    out
}
fn undelta2(data: &[u8]) -> Vec<u8> {
    if data.len() % 2 != 0 { return data.to_vec(); }
    let words: Vec<u16> = (0..data.len()).step_by(2)
        .map(|i| u16::from_be_bytes([data[i], data[i+1]])).collect();
    let mut prev = words[0];
    let mut out  = Vec::with_capacity(data.len());
    out.extend_from_slice(&prev.to_be_bytes());
    for i in 1..words.len() { prev = prev.wrapping_add(words[i]); out.extend_from_slice(&prev.to_be_bytes()); }
    out
}
fn delta_ch(data: &[u8], ch: usize) -> Vec<u8> {
    if data.len() % ch != 0 { return data.to_vec(); }
    let mut o = data.to_vec();
    for c in 0..ch {
        let mut prev = 0u8;
        let mut i = c;
        while i < data.len() { o[i] = data[i].wrapping_sub(prev); prev = data[i]; i += ch; }
    }
    o
}
fn undelta_ch(data: &[u8], ch: usize) -> Vec<u8> {
    if data.len() % ch != 0 { return data.to_vec(); }
    let mut o = data.to_vec();
    for c in 0..ch {
        let mut prev = 0u8;
        let mut i = c;
        while i < data.len() { o[i] = data[i].wrapping_add(prev); prev = o[i]; i += ch; }
    }
    o
}

fn apply_filter(data: &[u8], f: Filter) -> Vec<u8> {
    match f {
        Filter::None      => data.to_vec(),
        Filter::Delta1    => delta1(data),
        Filter::Delta2    => delta2(data),
        Filter::DeltaRgb  => delta_ch(data, 3),
        Filter::DeltaRgba => delta_ch(data, 4),
    }
}
fn reverse_filter(data: &[u8], f: Filter) -> Vec<u8> {
    match f {
        Filter::None      => data.to_vec(),
        Filter::Delta1    => undelta1(data),
        Filter::Delta2    => undelta2(data),
        Filter::DeltaRgb  => undelta_ch(data, 3),
        Filter::DeltaRgba => undelta_ch(data, 4),
    }
}

fn best_filter(data: &[u8], channels: u8) -> (Filter, Vec<u8>) {
    let mut cands = vec![Filter::None, Filter::Delta1];
    if data.len() % 2 == 0 { cands.push(Filter::Delta2); }
    if channels == 3 && data.len() % 3 == 0 { cands.push(Filter::DeltaRgb); }
    if channels == 4 && data.len() % 4 == 0 { cands.push(Filter::DeltaRgba); }
    let mut best_f   = Filter::None;
    let mut best_ent = entropy(data);
    let mut best_d   = data.to_vec();
    for f in cands {
        let fd = apply_filter(data, f);
        let e  = entropy(&fd);
        if e < best_ent { best_ent = e; best_f = f; best_d = fd; }
    }
    (best_f, best_d)
}

// ─────────────────────────────────────────────────────────────────
// STRIDE-FIELD DELTA
// ─────────────────────────────────────────────────────────────────

fn stride_delta(data: &[u8], s: usize) -> Vec<u8> {
    if data.len() % s != 0 || s < 2 { return data.to_vec(); }
    let mut o = data.to_vec();
    for i in s..data.len() { o[i] = data[i].wrapping_sub(data[i-s]); }
    o
}
fn stride_undelta(data: &[u8], s: usize) -> Vec<u8> {
    if data.len() % s != 0 || s < 2 { return data.to_vec(); }
    let mut o = data.to_vec();
    for i in s..data.len() { o[i] = data[i].wrapping_add(o[i-s]); }
    o
}
fn best_stride(data: &[u8]) -> (usize, Vec<u8>) {
    let base = entropy(data);
    let mut best_s = 0usize;
    let mut best_d = data.to_vec();
    let mut best_e = base;
    for s in 2..=32usize {
        if data.len() % s != 0 { continue; }
        let t = stride_delta(data, s);
        let e = entropy(&t);
        if e < best_e - 0.05 { best_e = e; best_s = s; best_d = t; }
    }
    (best_s, best_d)
}

// ─────────────────────────────────────────────────────────────────
// DFA INFERENCE
// ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum DfaKind {
    Raw(Vec<u8>),
    Constant(u8, usize),
    Arithmetic(u8, u8, usize),
    Alternating(u8, u8, usize),
    Periodic(Vec<u8>, usize),
}

impl DfaKind {
    fn generate(&self) -> Vec<u8> {
        match self {
            DfaKind::Raw(d)              => d.clone(),
            DfaKind::Constant(v,n)       => vec![*v; *n],
            DfaKind::Arithmetic(s,st,n)  => (0..*n).map(|i| s.wrapping_add((*st as usize * i) as u8)).collect(),
            DfaKind::Alternating(a,b,n)  => (0..*n).map(|i| if i%2==0 {*a} else {*b}).collect(),
            DfaKind::Periodic(p,r)       => p.repeat(*r),
        }
    }
    fn serial_cost(&self) -> usize {
        match self {
            DfaKind::Raw(d)             => d.len(),
            DfaKind::Constant(_,n)      => 1 + varint_len(*n as u64),
            DfaKind::Arithmetic(_,_,n)  => 2 + varint_len(*n as u64),
            DfaKind::Alternating(_,_,n) => 2 + varint_len(*n as u64),
            DfaKind::Periodic(p,r)      => varint_len(p.len() as u64) + p.len() + varint_len(*r as u64),
        }
    }
}

fn infer_dfa(data: &[u8]) -> DfaKind {
    let n = data.len();
    let mut best      = DfaKind::Raw(data.to_vec());
    let mut best_cost = n;

    if n > 0 && data.iter().all(|&b| b == data[0]) {
        let c = DfaKind::Constant(data[0], n);
        if c.serial_cost() < best_cost { best_cost = c.serial_cost(); best = c; }
    }
    if n >= 3 {
        let step = data[1].wrapping_sub(data[0]);
        if data.windows(2).all(|w| w[1].wrapping_sub(w[0]) == step) {
            let c = DfaKind::Arithmetic(data[0], step, n);
            if c.serial_cost() < best_cost { best_cost = c.serial_cost(); best = c; }
        }
    }
    if n >= 4 && n % 2 == 0 {
        let (a, b) = (data[0], data[1]);
        if a != b && data.iter().enumerate().all(|(i,&v)| v == if i%2==0 {a} else {b}) {
            let c = DfaKind::Alternating(a, b, n);
            if c.serial_cost() < best_cost { best_cost = c.serial_cost(); best = c; }
        }
    }
    'outer: for pl in 2..=(n/2).min(32) {
        if n % pl != 0 { continue; }
        let period  = &data[..pl];
        let repeats = n / pl;
        for i in 0..n { if data[i] != period[i % pl] { continue 'outer; } }
        let c = DfaKind::Periodic(period.to_vec(), repeats);
        if c.serial_cost() < best_cost { best = c; }
        break;
    }
    best
}

// ─────────────────────────────────────────────────────────────────
// AXIOM LIBRARY
// ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum AxiomEntry { Dfa(DfaKind), Compound(Vec<u32>) }

struct AxiomLib {
    entries: Vec<AxiomEntry>,
    reverse: HashMap<Vec<u8>, u32>,
}

impl AxiomLib {
    fn new() -> Self { Self { entries: Vec::new(), reverse: HashMap::new() } }

    fn register(&mut self, entry: AxiomEntry, resolved: Vec<u8>) -> u32 {
        if let Some(&id) = self.reverse.get(&resolved) { return id; }
        let id = self.entries.len() as u32;
        self.entries.push(entry);
        self.reverse.insert(resolved, id);
        id
    }
    fn add_dfa(&mut self, kind: DfaKind) -> u32 {
        let r = kind.generate();
        self.register(AxiomEntry::Dfa(kind), r)
    }
    fn add_compound(&mut self, refs: Vec<u32>, resolved: Vec<u8>) -> u32 {
        self.register(AxiomEntry::Compound(refs), resolved)
    }
    fn resolve(&self, id: u32) -> Vec<u8> {
        match &self.entries[id as usize] {
            AxiomEntry::Dfa(k)       => k.generate(),
            AxiomEntry::Compound(rs) => rs.iter().flat_map(|&r| self.resolve(r)).collect(),
        }
    }
    fn reindex(&self, seq: &[u32]) -> (Vec<AxiomEntry>, Vec<u32>) {
        let mut used = std::collections::HashSet::new();
        fn collect_used(lib: &AxiomLib, id: u32, used: &mut std::collections::HashSet<u32>) {
            if used.contains(&id) { return; }
            used.insert(id);
            if let AxiomEntry::Compound(refs) = &lib.entries[id as usize] {
                for &r in refs { collect_used(lib, r, used); }
            }
        }
        for &id in seq { collect_used(self, id, &mut used); }
        let mut sorted: Vec<u32> = used.into_iter().collect();
        sorted.sort_unstable();
        let o2n: HashMap<u32,u32> = sorted.iter().enumerate().map(|(n,&o)| (o, n as u32)).collect();
        let new_entries = sorted.iter().map(|&old| match &self.entries[old as usize] {
            AxiomEntry::Dfa(k)       => AxiomEntry::Dfa(k.clone()),
            AxiomEntry::Compound(rs) => AxiomEntry::Compound(rs.iter().map(|r| o2n[r]).collect()),
        }).collect();
        let new_seq = seq.iter().map(|id| o2n[id]).collect();
        (new_entries, new_seq)
    }
}

// ─────────────────────────────────────────────────────────────────
// INNER STREAM SERIALISATION
// ─────────────────────────────────────────────────────────────────

const TC_RAW: u8=1; const TC_COMPOUND: u8=2; const TC_CONSTANT: u8=3;
const TC_ARITHMETIC: u8=4; const TC_ALTERNATING: u8=5; const TC_PERIODIC: u8=6;

fn ser_inner(entries: &[AxiomEntry], seq: &[u32], filter: Filter) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(INNER_MAGIC);
    out.push(filter as u8);
    out.extend(encode_varint(entries.len() as u64));
    out.extend(encode_varint(seq.len() as u64));
    for (id, e) in entries.iter().enumerate() {
        out.extend(encode_varint(id as u64));
        match e {
            AxiomEntry::Dfa(DfaKind::Raw(d)) => {
                out.push(TC_RAW); out.extend(encode_varint(d.len() as u64)); out.extend_from_slice(d);
            }
            AxiomEntry::Compound(refs) => {
                out.push(TC_COMPOUND); out.extend(encode_varint(refs.len() as u64));
                for &r in refs { out.extend(encode_varint(r as u64)); }
            }
            AxiomEntry::Dfa(DfaKind::Constant(v,n)) => {
                out.push(TC_CONSTANT); out.push(*v); out.extend(encode_varint(*n as u64));
            }
            AxiomEntry::Dfa(DfaKind::Arithmetic(s,st,n)) => {
                out.push(TC_ARITHMETIC); out.push(*s); out.push(*st); out.extend(encode_varint(*n as u64));
            }
            AxiomEntry::Dfa(DfaKind::Alternating(a,b,n)) => {
                out.push(TC_ALTERNATING); out.push(*a); out.push(*b); out.extend(encode_varint(*n as u64));
            }
            AxiomEntry::Dfa(DfaKind::Periodic(p,r)) => {
                out.push(TC_PERIODIC);
                out.extend(encode_varint(p.len() as u64)); out.extend_from_slice(p);
                out.extend(encode_varint(*r as u64));
            }
        }
    }
    for &id in seq { out.extend(encode_varint(id as u64)); }
    out
}

fn des_inner(blob: &[u8]) -> Result<(Vec<AxiomEntry>, Vec<u32>, Filter), AxmzipError> {
    if blob.len() < 6 || &blob[..4] != INNER_MAGIC {
        return Err(AxmzipError::InvalidData("Bad inner magic".into()));
    }
    let filter = Filter::from_u8(blob[4]);
    let mut off = 5usize;
    let (lc, o) = decode_varint(blob, off)?; off = o;
    let (sc, o) = decode_varint(blob, off)?; off = o;

    let mut raw_entries: Vec<(u32, AxiomEntry)> = Vec::new();
    for _ in 0..lc {
        let (eid, o) = decode_varint(blob, off)?; off = o;
        let tc = blob[off]; off += 1;
        let entry = match tc {
            TC_RAW => {
                let (l, o) = decode_varint(blob, off)?; off = o;
                let d = blob[off..off+l as usize].to_vec(); off += l as usize;
                AxiomEntry::Dfa(DfaKind::Raw(d))
            }
            TC_COMPOUND => {
                let (rc, o) = decode_varint(blob, off)?; off = o;
                let mut refs = Vec::new();
                for _ in 0..rc { let (r,o)=decode_varint(blob,off)?; off=o; refs.push(r as u32); }
                AxiomEntry::Compound(refs)
            }
            TC_CONSTANT => {
                let v=blob[off]; off+=1;
                let (n,o)=decode_varint(blob,off)?; off=o;
                AxiomEntry::Dfa(DfaKind::Constant(v, n as usize))
            }
            TC_ARITHMETIC => {
                let s=blob[off]; off+=1; let st=blob[off]; off+=1;
                let (n,o)=decode_varint(blob,off)?; off=o;
                AxiomEntry::Dfa(DfaKind::Arithmetic(s,st,n as usize))
            }
            TC_ALTERNATING => {
                let a=blob[off]; off+=1; let b=blob[off]; off+=1;
                let (n,o)=decode_varint(blob,off)?; off=o;
                AxiomEntry::Dfa(DfaKind::Alternating(a,b,n as usize))
            }
            TC_PERIODIC => {
                let (pl,o)=decode_varint(blob,off)?; off=o;
                let p=blob[off..off+pl as usize].to_vec(); off+=pl as usize;
                let (r,o)=decode_varint(blob,off)?; off=o;
                AxiomEntry::Dfa(DfaKind::Periodic(p, r as usize))
            }
            _ => return Err(AxmzipError::InvalidData(format!("Unknown type {tc}")))
        };
        raw_entries.push((eid as u32, entry));
    }
    raw_entries.sort_by_key(|(id,_)| *id);
    let entries: Vec<AxiomEntry> = raw_entries.into_iter().map(|(_,e)| e).collect();
    let mut seq = Vec::with_capacity(sc as usize);
    for _ in 0..sc { let (id,o)=decode_varint(blob,off)?; off=o; seq.push(id as u32); }
    Ok((entries, seq, filter))
}

// ─────────────────────────────────────────────────────────────────
// CORE COMPRESS STREAM  (with progress reporting)
// ─────────────────────────────────────────────────────────────────

fn compress_stream(data: &[u8], channels: u8, prog: &Option<Progress>,
                   p_start: f32, p_end: f32) -> Vec<u8> {
    let span = p_end - p_start;
    let prog_at = |frac: f32| set_progress(prog, p_start + frac * span);

    prog_at(0.0);
    let (filter, filtered) = best_filter(data, channels);
    let n = filtered.len();
    let mut lib = AxiomLib::new();

    // ── Pattern frequency scan (slow for large data) ──────────────
    // We update progress every 8 pattern lengths so the UI stays live.
    let max_len = MAX_PAT_LEN.min(n);
    let mut freq: HashMap<Vec<u8>, usize> = HashMap::new();
    for (li, length) in (2..=max_len).enumerate() {
        // Progress: scan takes 0%–45% of this stream's budget
        if li % 4 == 0 {
            prog_at(0.01 + (li as f32 / max_len as f32) * 0.44);
        }
        for i in 0..=(n.saturating_sub(length)) {
            *freq.entry(filtered[i..i+length].to_vec()).or_insert(0) += 1;
        }
    }
    prog_at(0.45);

    // ── Score and select top axioms ───────────────────────────────
    let mut candidates: Vec<(i64, DfaKind)> = freq.into_iter()
        .filter(|(_,f)| *f >= 2)
        .filter_map(|(pat, f)| {
            let dfa  = infer_dfa(&pat);
            let dc   = dfa.serial_cost();
            let entry_cost = varint_len(MAX_ATOMICS as u64) + 1 + dc;
            let ref_sz     = varint_len(MAX_ATOMICS as u64);
            let savings    = f as i64 * (pat.len() as i64 - ref_sz as i64) - entry_cost as i64;
            if savings > 0 { Some((savings, dfa)) } else { None }
        })
        .collect();
    candidates.sort_by(|a,b| b.0.cmp(&a.0));
    for (_, dfa) in candidates.into_iter().take(MAX_ATOMICS) { lib.add_dfa(dfa); }
    prog_at(0.50);

    // ── Greedy encode ─────────────────────────────────────────────
    let mut seq = Vec::new();
    let mut i = 0usize;
    let encode_report = (n / 20).max(1);
    while i < n {
        if i % encode_report == 0 {
            prog_at(0.50 + (i as f32 / n as f32) * 0.25);
        }
        let mut found = false;
        for length in (2..=MAX_PAT_LEN.min(n-i)).rev() {
            if let Some(&id) = lib.reverse.get(&filtered[i..i+length]) {
                seq.push(id); i += length; found = true; break;
            }
        }
        if !found {
            seq.push(lib.add_dfa(DfaKind::Raw(vec![filtered[i]]))); i += 1;
        }
    }
    prog_at(0.75);

    // ── Compound passes ───────────────────────────────────────────
    for pass in 0..MAX_PASSES {
        prog_at(0.75 + (pass as f32 / MAX_PASSES as f32) * 0.15);
        let mut pf: HashMap<(u32,u32), usize> = HashMap::new();
        for w in seq.windows(2) { *pf.entry((w[0],w[1])).or_insert(0) += 1; }
        let mut useful: Vec<((u32,u32),usize)> = pf.into_iter().filter(|(_,f)| *f>5).collect();
        if useful.is_empty() { break; }
        useful.sort_by(|a,b| b.1.cmp(&a.1));
        let mut pm: HashMap<(u32,u32),u32> = HashMap::new();
        for (pair,_) in useful {
            let r: Vec<u8> = lib.resolve(pair.0).into_iter().chain(lib.resolve(pair.1)).collect();
            pm.insert(pair, lib.add_compound(vec![pair.0, pair.1], r));
        }
        let prev_len = seq.len();
        let mut ns = Vec::new();
        let mut j  = 0;
        while j < seq.len() {
            if j+1 < seq.len() {
                if let Some(&id) = pm.get(&(seq[j], seq[j+1])) { ns.push(id); j+=2; continue; }
            }
            ns.push(seq[j]); j+=1;
        }
        seq = ns;
        if seq.len() == prev_len { break; }
    }
    prog_at(0.90);

    let (final_entries, final_seq) = lib.reindex(&seq);
    prog_at(0.98);
    ser_inner(&final_entries, &final_seq, filter)
}

fn decompress_stream(blob: &[u8]) -> Result<Vec<u8>, AxmzipError> {
    let (entries, seq, filter) = des_inner(blob)?;
    fn resolve(entries: &[AxiomEntry], id: u32) -> Vec<u8> {
        match &entries[id as usize] {
            AxiomEntry::Dfa(k)      => k.generate(),
            AxiomEntry::Compound(rs)=> rs.iter().flat_map(|&r| resolve(entries, r)).collect(),
        }
    }
    let filtered: Vec<u8> = seq.iter().flat_map(|&id| resolve(&entries, id)).collect();
    Ok(reverse_filter(&filtered, filter))
}

// ─────────────────────────────────────────────────────────────────
// QUANTISATION
// ─────────────────────────────────────────────────────────────────

pub fn quality_to_step(quality: u8) -> u8 {
    ((128.0 * (1.0 - quality as f64 / 100.0)).round() as u8).max(1)
}

fn quantize(data: &[u8], quality: u8) -> Vec<u8> {
    let step = quality_to_step(quality) as u16;
    data.iter().map(|&b| ((b as u16 + step/2) / step * step).min(255) as u8).collect()
}

pub fn psnr(original: &[u8], reconstructed: &[u8]) -> f64 {
    if original.len() != reconstructed.len() || original.is_empty() { return 0.0; }
    let mse: f64 = original.iter().zip(reconstructed)
        .map(|(&a,&b)| { let d = a as f64 - b as f64; d*d })
        .sum::<f64>() / original.len() as f64;
    if mse == 0.0 { return f64::INFINITY; }
    10.0 * (255.0f64.powi(2) / mse).log10()
}

pub fn max_error(original: &[u8], reconstructed: &[u8]) -> u8 {
    original.iter().zip(reconstructed)
        .map(|(&a,&b)| (a as i16 - b as i16).unsigned_abs() as u8)
        .max().unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────
// OUTER SERIALISATION  (filename stored in header)
// ─────────────────────────────────────────────────────────────────
// Format:
//   [MAGIC 4B][VERSION 1B][MD5 16B][mode 1B]
//   [fname_len varint][fname UTF-8 bytes]   ← original filename
//   mode=PLAIN:  [inner_len varint][inner blob]
//   mode=STRIDE: [stride 1B][inner_len varint][inner blob]
//   mode=LOSSY:  [quality 1B][channels 1B][inner_len varint][inner blob]

fn write_header(checksum: &[u8;16], mode: u8, fname: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(checksum);
    out.push(mode);
    let fb = fname.as_bytes();
    out.extend(encode_varint(fb.len() as u64));
    out.extend_from_slice(fb);
    out
}

fn read_header(blob: &[u8]) -> Result<(u8, String, usize), AxmzipError> {
    // Returns (mode, original_filename, offset_after_header)
    if blob.len() < 22 { return Err(AxmzipError::InvalidData("File too short".into())); }
    if &blob[..4] != MAGIC   { return Err(AxmzipError::BadMagic); }
    if blob[4] != VERSION    { return Err(AxmzipError::BadVersion); }
    // [5..21] = MD5, [21] = mode
    let mode = blob[21];
    let mut off = 22usize;
    let (fl, o) = decode_varint(blob, off)?; off = o;
    let fname_bytes = blob.get(off..off+fl as usize)
        .ok_or_else(|| AxmzipError::InvalidData("Filename truncated".into()))?;
    let fname = String::from_utf8_lossy(fname_bytes).to_string();
    off += fl as usize;
    Ok((mode, fname, off))
}

// ─────────────────────────────────────────────────────────────────
// PUBLIC API
// ─────────────────────────────────────────────────────────────────

/// Compress data.
/// - `quality`: 100 = lossless, 0–99 = lossy
/// - `channels`: 1 = mono/gray, 3 = RGB, 4 = RGBA
/// - `original_filename`: stored in header so decompress can restore extension
/// - `progress`: optional shared float updated 0.0→1.0 during compression
pub fn compress(
    data:              &[u8],
    quality:           u8,
    channels:          u8,
    original_filename: &str,
    progress:          Option<Progress>,
) -> (Vec<u8>, CompressStats) {
    let t0   = std::time::Instant::now();
    let orig = data.len();

    set_progress(&progress, 0.02);

    // ── Lossy path ────────────────────────────────────────────────
    if quality < 100 {
        set_progress(&progress, 0.05);
        let quantized        = quantize(data, quality);
        let psnr_val         = psnr(data, &quantized);
        let max_err          = max_error(data, &quantized);
        let checksum: [u8;16] = *md5::compute(&quantized);
        let inner = compress_stream(&quantized, channels, &progress, 0.1, 0.9);
        set_progress(&progress, 0.95);
        let mut blob = write_header(&checksum, MODE_LOSSY, original_filename);
        blob.push(quality); blob.push(channels);
        blob.extend(encode_varint(inner.len() as u64));
        blob.extend_from_slice(&inner);
        set_progress(&progress, 1.0);
        let comp  = blob.len();
        let ratio = (1.0 - comp as f64 / orig as f64) * 100.0;
        let step  = quality_to_step(quality);
        return (blob, CompressStats {
            original_bytes: orig, compressed_bytes: comp,
            ratio_pct: ratio, mode: format!("lossy(q={quality},step={step})"),
            lossless: false, psnr_db: psnr_val, max_error: max_err,
            lib_entries: 0, elapsed_ms: t0.elapsed().as_millis() as u64,
        });
    }

    // ── Lossless: try plain then stride ──────────────────────────
    let checksum: [u8;16] = *md5::compute(data);
    set_progress(&progress, 0.05);

    // Plain (delta + DFA + compound) — uses 5%–52% of progress budget
    let plain_inner = compress_stream(data, channels, &progress, 0.05, 0.52);
    let mut plain_blob = write_header(&checksum, MODE_PLAIN, original_filename);
    plain_blob.extend(encode_varint(plain_inner.len() as u64));
    plain_blob.extend_from_slice(&plain_inner);

    set_progress(&progress, 0.55);

    // Stride detection
    let (best_s, stride_data) = best_stride(data);
    let (blob, mode) = if best_s > 0 {
        // Stride (uses 55%–95% of progress budget)
        let stride_inner = compress_stream(&stride_data, channels, &progress, 0.55, 0.93);
        let mut stride_blob = write_header(&checksum, MODE_STRIDE, original_filename);
        stride_blob.push(best_s as u8);
        stride_blob.extend(encode_varint(stride_inner.len() as u64));
        stride_blob.extend_from_slice(&stride_inner);
        if stride_blob.len() < plain_blob.len() {
            (stride_blob, format!("stride(s={best_s})"))
        } else {
            (plain_blob, "plain".into())
        }
    } else {
        set_progress(&progress, 0.95);
        (plain_blob, "plain".into())
    };

    set_progress(&progress, 1.0);
    let comp  = blob.len();
    let ratio = (1.0 - comp as f64 / orig as f64) * 100.0;
    (blob, CompressStats {
        original_bytes: orig, compressed_bytes: comp,
        ratio_pct: ratio, mode,
        lossless: true, psnr_db: f64::INFINITY, max_error: 0,
        lib_entries: 0, elapsed_ms: t0.elapsed().as_millis() as u64,
    })
}

/// Decompress an axmzip blob.
/// Returns DecompressResult which includes the original filename for extension restoration.
pub fn decompress(blob: &[u8]) -> Result<DecompressResult, AxmzipError> {
    let checksum_stored = &blob[5..21];
    let (mode, original_filename, mut off) = read_header(blob)?;

    let data = match mode {
        MODE_PLAIN => {
            let (blen, o) = decode_varint(blob, off)?; off = o;
            let end = off + blen as usize;
            if end > blob.len() { return Err(AxmzipError::InvalidData("Truncated plain blob".into())); }
            decompress_stream(&blob[off..end])?
        }
        MODE_STRIDE => {
            let stride = blob[off] as usize; off += 1;
            let (blen, o) = decode_varint(blob, off)?; off = o;
            let end = off + blen as usize;
            if end > blob.len() { return Err(AxmzipError::InvalidData("Truncated stride blob".into())); }
            let raw = decompress_stream(&blob[off..end])?;
            stride_undelta(&raw, stride)
        }
        MODE_LOSSY => {
            off += 2; // quality + channels (already in inner stream filter)
            let (blen, o) = decode_varint(blob, off)?; off = o;
            let end = off + blen as usize;
            if end > blob.len() { return Err(AxmzipError::InvalidData("Truncated lossy blob".into())); }
            decompress_stream(&blob[off..end])?
        }
        _ => return Err(AxmzipError::InvalidData(format!("Unknown mode {mode}")))
    };

    let actual: [u8;16] = *md5::compute(&data);
    if actual.as_ref() != checksum_stored { return Err(AxmzipError::ChecksumMismatch); }

    Ok(DecompressResult {
        data,
        original_filename: if original_filename.is_empty() { None } else { Some(original_filename) },
    })
}

/// Quick probe — return mode string without decompressing.
pub fn probe(blob: &[u8]) -> Option<String> {
    // `off` points to the first byte after the header (mode-specific data starts here)
    if let Ok((mode, _fname, off)) = read_header(blob) {
        let mode_str = match mode {
            MODE_PLAIN  => "lossless/plain".into(),
            // For STRIDE: first byte after header is the stride value
            MODE_STRIDE => format!("lossless/stride(s={})", blob.get(off).unwrap_or(&0)),
            // For LOSSY: first byte is quality, second is channels
            MODE_LOSSY  => format!("lossy(q={})", blob.get(off).unwrap_or(&0)),
            _           => "unknown".into(),
        };
        Some(mode_str)
    } else { None }
}

pub fn is_axmzip(blob: &[u8]) -> bool {
    blob.len() >= 22 && &blob[..4] == MAGIC && blob[4] == VERSION
}

// ─────────────────────────────────────────────────────────────────
// TESTS
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8], quality: u8) {
        let (blob, stats) = compress(data, quality, 1, "test.bin", None);
        let result = decompress(&blob).unwrap();
        if quality == 100 {
            assert_eq!(result.data, data, "Lossless mismatch");
        }
        let _ = stats;
    }

    #[test] fn rt_repetitive()  { roundtrip(&b"\xAB\xCD\xEF\x01".repeat(2000), 100); }
    #[test] fn rt_counter()     { roundtrip(&(0u8..=255).cycle().take(8000).collect::<Vec<_>>(), 100); }
    #[test] fn rt_lossy()       { roundtrip(&(0u8..=255).cycle().take(8000).collect::<Vec<_>>(), 75); }

    #[test] fn filename_preserved() {
        let data = b"hello world".repeat(200);
        let (blob, _) = compress(&data, 100, 1, "document.txt", None);
        let result = decompress(&blob).unwrap();
        assert_eq!(result.original_filename.as_deref(), Some("document.txt"));
        assert_eq!(result.data, data);
    }

    #[test] fn sensor_stride() {
        let mut data = Vec::new();
        for i in 0u16..800 { data.extend_from_slice(&[0xFF, (i%64) as u8, (i*3%256) as u8, 0x04, 0x00]); }
        let (blob, stats) = compress(&data, 100, 1, "sensor.bin", None);
        assert!(stats.ratio_pct > 50.0, "Expected >50% got {:.1}%", stats.ratio_pct);
        let result = decompress(&blob).unwrap();
        assert_eq!(result.data, data);
    }

    #[test] fn varint_rt() {
        for n in [0u64, 1, 127, 128, 16383, 16384, u32::MAX as u64] {
            let enc = encode_varint(n);
            let (dec, _) = decode_varint(&enc, 0).unwrap();
            assert_eq!(n, dec);
        }
    }

    #[test] fn dfa_types() {
        let d = vec![0xFFu8; 100]; assert_eq!(infer_dfa(&d).generate(), d);
        let d: Vec<u8> = (0..100).collect(); assert_eq!(infer_dfa(&d).generate(), d);
        let d: Vec<u8> = (0..100).map(|i| if i%2==0 {0xAA} else {0x55}).collect();
        assert_eq!(infer_dfa(&d).generate(), d);
        let d = b"ABC".repeat(30); assert_eq!(infer_dfa(&d).generate(), d);
    }
}

// ─────────────────────────────────────────────────────────────────
// ARCHIVE (multi-file / folder) API
// ─────────────────────────────────────────────────────────────────
//
// Format:
//   [MAGIC "AXMA" 4B][VERSION 1B]
//   [root_name_len varint][root_name UTF-8]   <- original folder name
//   [num_files varint]
//   For each file:
//     [relpath_len varint][relpath UTF-8]      <- "/" separated on all platforms
//     [orig_size varint]
//     [blob_len varint][axmzip single-file blob]
//   [MD5 16B]                                 <- over raw content of all files in order

const ARCHIVE_MAGIC:   &[u8] = b"AXMA";
const ARCHIVE_VERSION: u8    = 0x01;

use std::thread;

/// A single file entry inside an archive.
#[derive(Debug, Clone)]
pub struct ArchiveEntry {
    /// Relative path from archive root, always "/" separated.
    pub rel_path: String,
    /// Raw file bytes.
    pub data:     Vec<u8>,
}

/// Stats from compress_archive.
#[derive(Debug, Clone)]
pub struct ArchiveStats {
    pub root_name:        String,
    pub file_count:       usize,
    pub original_bytes:   usize,
    pub compressed_bytes: usize,
    pub ratio_pct:        f64,
    pub elapsed_ms:       u64,
}

/// Compress a slice of ArchiveEntry into one archive blob.
///
/// Progress is updated smoothly: each file occupies a proportional
/// slice of 0.0–1.0 based on its uncompressed size.
pub fn compress_archive(
    root_name: &str,
    entries:   &[ArchiveEntry],
    quality:   u8,
    progress:  Option<Progress>,
) -> (Vec<u8>, ArchiveStats) {
    let t0 = std::time::Instant::now();
    let n  = entries.len();
    let total_orig: usize = entries.iter().map(|e| e.data.len()).sum();

    // Header
    let mut out = Vec::new();
    out.extend_from_slice(ARCHIVE_MAGIC);
    out.push(ARCHIVE_VERSION);
    let rn = root_name.as_bytes();
    out.extend(encode_varint(rn.len() as u64));
    out.extend_from_slice(rn);
    out.extend(encode_varint(n as u64));

    let mut md5_ctx    = md5::Context::new();
    let mut total_comp = 0usize;
    // Cumulative size offsets for proportional progress slices
    let mut cum_before = 0usize;

    for entry in entries.iter() {
        // This file's share of total work (by byte count, minimum 1 to avoid /0)
        let this_sz  = entry.data.len().max(1);
        let denom    = total_orig.max(1);
        let p_start  = cum_before as f32 / denom as f32 * 0.95;
        let p_end    = (cum_before + this_sz) as f32 / denom as f32 * 0.95;

        // Build a bridge Progress that maps 0–1 into [p_start, p_end] in outer
        let bridge: Option<Progress> = match &progress {
            None => None,
            Some(outer_arc) => {
                let outer_clone = Arc::clone(outer_arc);
                let inner_arc   = Arc::new(Mutex::new(0.0f32));
                let inner_poll  = Arc::clone(&inner_arc);
                let ps = p_start;
                let pe = p_end;
                thread::spawn(move || {
                    loop {
                        let v = { *inner_poll.lock().unwrap() };
                        { *outer_clone.lock().unwrap() = ps + v * (pe - ps); }
                        if v >= 1.0 { break; }
                        std::thread::sleep(std::time::Duration::from_millis(15));
                    }
                });
                Some(inner_arc)
            }
        };

        // Infer channel hint from extension
        let ext = entry.rel_path.rsplit('.').next().unwrap_or("").to_lowercase();
        let channels: u8 = match ext.as_str() {
            "ppm" | "rgb" => 3,
            "rgba"        => 4,
            _             => 1,
        };

        let fname = entry.rel_path.rsplit('/').next().unwrap_or(&entry.rel_path);
        let (blob, _) = compress(&entry.data, quality, channels, fname, bridge);

        // Write entry record
        let rp = entry.rel_path.as_bytes();
        out.extend(encode_varint(rp.len() as u64));
        out.extend_from_slice(rp);
        out.extend(encode_varint(entry.data.len() as u64));
        out.extend(encode_varint(blob.len() as u64));
        out.extend_from_slice(&blob);

        md5_ctx.consume(&entry.data);
        total_comp  += blob.len();
        cum_before  += this_sz;
    }

    // Trailer checksum
    let checksum: [u8; 16] = *md5_ctx.compute();
    out.extend_from_slice(&checksum);

    set_progress(&progress, 1.0);

    let comp  = out.len();
    let ratio = if total_orig > 0 { (1.0 - comp as f64 / total_orig as f64) * 100.0 } else { 0.0 };

    (out, ArchiveStats {
        root_name:        root_name.to_string(),
        file_count:       n,
        original_bytes:   total_orig,
        compressed_bytes: comp,
        ratio_pct:        ratio,
        elapsed_ms:       t0.elapsed().as_millis() as u64,
    })
}

/// Decompress an axmzip archive blob.
/// Returns `(root_name, entries)`.
pub fn decompress_archive(blob: &[u8]) -> Result<(String, Vec<ArchiveEntry>), AxmzipError> {
    if blob.len() < 6 { return Err(AxmzipError::InvalidData("Archive too short".into())); }
    if &blob[..4] != ARCHIVE_MAGIC {
        if &blob[..4] == MAGIC {
            return Err(AxmzipError::InvalidData(
                "This is a single-file .axm — use decompress() instead".into()
            ));
        }
        return Err(AxmzipError::BadMagic);
    }
    if blob[4] != ARCHIVE_VERSION { return Err(AxmzipError::BadVersion); }

    let mut off = 5usize;

    // Root name
    let (rn_len, o) = decode_varint(blob, off)?; off = o;
    let root_bytes  = blob.get(off..off + rn_len as usize)
        .ok_or_else(|| AxmzipError::InvalidData("Root name truncated".into()))?;
    let root_name = std::str::from_utf8(root_bytes)
        .map_err(|_| AxmzipError::InvalidData("Root name not UTF-8".into()))?.to_string();
    off += rn_len as usize;

    // File count
    let (num_files, o) = decode_varint(blob, off)?; off = o;

    let mut entries = Vec::with_capacity(num_files as usize);
    let mut md5_ctx = md5::Context::new();

    for file_idx in 0..num_files {
        // rel_path
        let (rp_len, o) = decode_varint(blob, off)?; off = o;
        let rp_bytes    = blob.get(off..off + rp_len as usize)
            .ok_or_else(|| AxmzipError::InvalidData(format!("Path #{file_idx} truncated")))?;
        let rel_path = std::str::from_utf8(rp_bytes)
            .map_err(|_| AxmzipError::InvalidData(format!("Path #{file_idx} not UTF-8")))?.to_string();
        off += rp_len as usize;

        // orig_size
        let (orig_size, o) = decode_varint(blob, off)?; off = o;

        // blob_len + blob
        let (blob_len, o) = decode_varint(blob, off)?; off = o;
        // Guard: leave room for 16-byte trailer
        let end = off + blob_len as usize;
        if end + 16 > blob.len() {
            return Err(AxmzipError::InvalidData(
                format!("Entry '{}' blob overruns archive", rel_path)
            ));
        }
        let file_blob = &blob[off..end];
        off = end;

        let result = decompress(file_blob)?;
        if result.data.len() != orig_size as usize {
            return Err(AxmzipError::InvalidData(format!(
                "'{}': expected {} bytes, got {}",
                rel_path, orig_size, result.data.len()
            )));
        }

        md5_ctx.consume(&result.data);
        entries.push(ArchiveEntry { rel_path, data: result.data });
    }

    // Trailer checksum
    if off + 16 > blob.len() {
        return Err(AxmzipError::InvalidData("Missing archive checksum".into()));
    }
    let stored:   &[u8]   = &blob[off..off + 16];
    let computed: [u8;16] = *md5_ctx.compute();
    if computed.as_ref() != stored { return Err(AxmzipError::ChecksumMismatch); }

    Ok((root_name, entries))
}

/// Returns true if the blob is a multi-file axmzip archive.
pub fn is_axmzip_archive(blob: &[u8]) -> bool {
    blob.len() >= 6 && &blob[..4] == ARCHIVE_MAGIC && blob[4] == ARCHIVE_VERSION
}

/// Probe without decompressing: returns (root_name, file_count).
pub fn probe_archive(blob: &[u8]) -> Option<(String, u64)> {
    if !is_axmzip_archive(blob) { return None; }
    let mut off = 5usize;
    let (rn_len, o) = decode_varint(blob, off).ok()?; off = o;
    let root = std::str::from_utf8(blob.get(off..off + rn_len as usize)?).ok()?.to_string();
    off += rn_len as usize;
    let (count, _) = decode_varint(blob, off).ok()?;
    Some((root, count))
}


// ─────────────────────────────────────────────────────────────────
// FAST ARCHIVE LISTING  (no decompression — reads headers only)
// ─────────────────────────────────────────────────────────────────

/// Info about a single file inside an archive, obtained without decompressing.
#[derive(Debug, Clone)]
pub struct ArchiveFileInfo {
    pub rel_path:    String,
    pub orig_size:   usize,   // uncompressed size
    pub packed_size: usize,   // compressed blob size (includes axmzip framing)
}

impl ArchiveFileInfo {
    pub fn ratio_pct(&self) -> f64 {
        if self.orig_size == 0 { return 0.0; }
        (1.0 - self.packed_size as f64 / self.orig_size as f64) * 100.0
    }

    /// File extension, lower-case, empty string if none.
    pub fn ext(&self) -> String {
        self.rel_path.rsplit('.').next()
            .map(|e| e.to_lowercase())
            .unwrap_or_default()
    }
}

/// List all files in an archive **without decompressing** anything.
/// Returns (root_name, Vec<ArchiveFileInfo>).
/// Fast — reads only the header of each entry blob, skips the data.
pub fn list_archive(blob: &[u8]) -> Result<(String, Vec<ArchiveFileInfo>), AxmzipError> {
    if blob.len() < 6 { return Err(AxmzipError::InvalidData("Too short".into())); }
    if &blob[..4] != ARCHIVE_MAGIC { return Err(AxmzipError::BadMagic); }
    if blob[4] != ARCHIVE_VERSION  { return Err(AxmzipError::BadVersion); }

    let mut off = 5usize;
    let (rn_len, o) = decode_varint(blob, off)?; off = o;
    let root = std::str::from_utf8(
        blob.get(off..off + rn_len as usize)
            .ok_or_else(|| AxmzipError::InvalidData("Root name truncated".into()))?
    ).map_err(|_| AxmzipError::InvalidData("Root not UTF-8".into()))?.to_string();
    off += rn_len as usize;

    let (num_files, o) = decode_varint(blob, off)?; off = o;
    let mut infos = Vec::with_capacity(num_files as usize);

    for _ in 0..num_files {
        // rel_path
        let (rp_len, o) = decode_varint(blob, off)?; off = o;
        let rel_path = std::str::from_utf8(
            blob.get(off..off + rp_len as usize)
                .ok_or_else(|| AxmzipError::InvalidData("Path truncated".into()))?
        ).map_err(|_| AxmzipError::InvalidData("Path not UTF-8".into()))?.to_string();
        off += rp_len as usize;

        // orig_size and blob_len — this is ALL we need
        let (orig_size, o)  = decode_varint(blob, off)?; off = o;
        let (blob_len,  o)  = decode_varint(blob, off)?; off = o;

        infos.push(ArchiveFileInfo {
            rel_path,
            orig_size:   orig_size as usize,
            packed_size: blob_len  as usize,
        });

        // Skip the actual compressed blob entirely
        off += blob_len as usize;
        if off > blob.len() {
            return Err(AxmzipError::InvalidData("Entry blob overruns file".into()));
        }
    }

    Ok((root, infos))
}
