//! Axmzip Core — Axiom-Based Binary Compression v5
//!
//! Full pipeline:
//!   1. Auto-select best pre-filter (delta1, delta2, RGB delta, stride-field delta)
//!   2. DFA axiom discovery (constant, arithmetic, alternating, periodic, raw)
//!   3. Greedy encode → sequence of axiom IDs
//!   4. Compound passes — find frequent adjacent pairs, collapse into new axioms
//!   5. Prune unused library entries, reindex IDs 0..N
//!   6. Serialise with LEB128 varints
//!
//!   Lossy mode: quantise bytes before compression (quality 0–99).
//!   Stride mode: inter-packet field delta for structured binary (auto-detected).

use std::collections::HashMap;
use md5;

// ─────────────────────────────────────────────────────────────────
// PUBLIC TYPES
// ─────────────────────────────────────────────────────────────────

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
            AxmzipError::BadMagic          => write!(f, "Bad magic bytes — not an axmzip file"),
            AxmzipError::BadVersion        => write!(f, "Unsupported format version"),
            AxmzipError::ChecksumMismatch  => write!(f, "Checksum mismatch — data corrupted"),
            AxmzipError::InvalidData(s)    => write!(f, "Invalid data: {s}"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// CONSTANTS
// ─────────────────────────────────────────────────────────────────

const MAGIC:        &[u8] = b"AXM5";
const VERSION:      u8    = 0x01;
const INNER_MAGIC:  &[u8] = b"AXi\x05";

const MODE_PLAIN:  u8 = 0x00;
const MODE_STRIDE: u8 = 0x01;
const MODE_LOSSY:  u8 = 0x02;

const MAX_ATOMICS: usize = 256;
const MAX_PASSES:  usize = 12;
const MAX_PAT_LEN: usize = 64;

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
    let mut result: u64 = 0;
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
    }
    Ok((result, off))
}

// ─────────────────────────────────────────────────────────────────
// ENTROPY ESTIMATE
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
    let mut out = vec![0u8; data.len()];
    if data.is_empty() { return out; }
    out[0] = data[0];
    for i in 1..data.len() { out[i] = data[i].wrapping_sub(data[i-1]); }
    out
}

fn undelta1(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; data.len()];
    if data.is_empty() { return out; }
    out[0] = data[0];
    for i in 1..data.len() { out[i] = data[i].wrapping_add(out[i-1]); }
    out
}

fn delta2(data: &[u8]) -> Vec<u8> {
    if data.len() % 2 != 0 { return data.to_vec(); }
    let mut out = Vec::with_capacity(data.len());
    let words: Vec<u16> = (0..data.len()).step_by(2)
        .map(|i| u16::from_be_bytes([data[i], data[i+1]])).collect();
    out.extend_from_slice(&words[0].to_be_bytes());
    for i in 1..words.len() {
        out.extend_from_slice(&words[i].wrapping_sub(words[i-1]).to_be_bytes());
    }
    out
}

fn undelta2(data: &[u8]) -> Vec<u8> {
    if data.len() % 2 != 0 { return data.to_vec(); }
    let mut out = Vec::with_capacity(data.len());
    let words: Vec<u16> = (0..data.len()).step_by(2)
        .map(|i| u16::from_be_bytes([data[i], data[i+1]])).collect();
    let mut prev = words[0];
    out.extend_from_slice(&prev.to_be_bytes());
    for i in 1..words.len() {
        prev = prev.wrapping_add(words[i]);
        out.extend_from_slice(&prev.to_be_bytes());
    }
    out
}

fn delta_channels(data: &[u8], ch: usize) -> Vec<u8> {
    if data.len() % ch != 0 { return data.to_vec(); }
    let mut out = data.to_vec();
    for c in 0..ch {
        let mut prev = 0u8;
        let mut i = c;
        while i < data.len() {
            out[i] = data[i].wrapping_sub(prev);
            prev    = data[i];
            i      += ch;
        }
    }
    out
}

fn undelta_channels(data: &[u8], ch: usize) -> Vec<u8> {
    if data.len() % ch != 0 { return data.to_vec(); }
    let mut out = data.to_vec();
    for c in 0..ch {
        let mut prev = 0u8;
        let mut i = c;
        while i < data.len() {
            out[i] = data[i].wrapping_add(prev);
            prev    = out[i];
            i      += ch;
        }
    }
    out
}

fn apply_filter(data: &[u8], f: Filter) -> Vec<u8> {
    match f {
        Filter::None     => data.to_vec(),
        Filter::Delta1   => delta1(data),
        Filter::Delta2   => delta2(data),
        Filter::DeltaRgb => delta_channels(data, 3),
        Filter::DeltaRgba=> delta_channels(data, 4),
    }
}

fn reverse_filter(data: &[u8], f: Filter) -> Vec<u8> {
    match f {
        Filter::None     => data.to_vec(),
        Filter::Delta1   => undelta1(data),
        Filter::Delta2   => undelta2(data),
        Filter::DeltaRgb => undelta_channels(data, 3),
        Filter::DeltaRgba=> undelta_channels(data, 4),
    }
}

fn best_filter(data: &[u8], channels: u8) -> (Filter, Vec<u8>) {
    let candidates: Vec<Filter> = {
        let mut v = vec![Filter::None, Filter::Delta1];
        if data.len() % 2 == 0 { v.push(Filter::Delta2); }
        if channels == 3 && data.len() % 3 == 0 { v.push(Filter::DeltaRgb); }
        if channels == 4 && data.len() % 4 == 0 { v.push(Filter::DeltaRgba); }
        v
    };
    let mut best_f   = Filter::None;
    let mut best_ent = entropy(data);
    let mut best_d   = data.to_vec();
    for f in candidates {
        let filtered = apply_filter(data, f);
        let e = entropy(&filtered);
        if e < best_ent { best_ent = e; best_f = f; best_d = filtered; }
    }
    (best_f, best_d)
}

// ─────────────────────────────────────────────────────────────────
// STRIDE-FIELD DELTA
// ─────────────────────────────────────────────────────────────────

fn stride_delta(data: &[u8], stride: usize) -> Vec<u8> {
    if data.len() % stride != 0 || stride < 2 { return data.to_vec(); }
    let mut out = data.to_vec();
    for i in stride..data.len() {
        out[i] = data[i].wrapping_sub(data[i - stride]);
    }
    out
}

fn stride_undelta(data: &[u8], stride: usize) -> Vec<u8> {
    if data.len() % stride != 0 || stride < 2 { return data.to_vec(); }
    let mut out = data.to_vec();
    for i in stride..data.len() {
        out[i] = data[i].wrapping_add(out[i - stride]);
    }
    out
}

fn best_stride(data: &[u8]) -> (usize, Vec<u8>, f64) {
    let orig_ent = entropy(data);
    let mut best_s = 0usize;
    let mut best_d = data.to_vec();
    let mut best_e = orig_ent;
    for s in 2..=32usize {
        if data.len() % s != 0 { continue; }
        let transformed = stride_delta(data, s);
        let e = entropy(&transformed);
        if e < best_e - 0.05 { best_e = e; best_s = s; best_d = transformed.clone(); }
    }
    (best_s, best_d, best_e)
}

// ─────────────────────────────────────────────────────────────────
// DFA INFERENCE
// ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum DfaKind {
    Raw(Vec<u8>),
    Constant(u8, usize),               // value, count
    Arithmetic(u8, u8, usize),         // start, step, count
    Alternating(u8, u8, usize),        // a, b, count
    Periodic(Vec<u8>, usize),          // period, repeats
}

impl DfaKind {
    fn generate(&self) -> Vec<u8> {
        match self {
            DfaKind::Raw(d)             => d.clone(),
            DfaKind::Constant(v, n)     => vec![*v; *n],
            DfaKind::Arithmetic(s,st,n) => (0..*n).map(|i| s.wrapping_add((*st as usize * i) as u8)).collect(),
            DfaKind::Alternating(a,b,n) => (0..*n).map(|i| if i%2==0 {*a} else {*b}).collect(),
            DfaKind::Periodic(p, r)     => p.repeat(*r),
        }
    }

    fn serialised_cost(&self) -> usize {
        match self {
            DfaKind::Raw(d)           => d.len(),
            DfaKind::Constant(_,n)    => 1 + varint_len(*n as u64),
            DfaKind::Arithmetic(_,_,n)=> 2 + varint_len(*n as u64),
            DfaKind::Alternating(_,_,n)=>2 + varint_len(*n as u64),
            DfaKind::Periodic(p,r)    => varint_len(p.len() as u64) + p.len() + varint_len(*r as u64),
        }
    }
}

fn varint_len(mut n: u64) -> usize {
    let mut len = 1;
    while n >= 0x80 { n >>= 7; len += 1; }
    len
}

fn infer_dfa(data: &[u8]) -> DfaKind {
    let n = data.len();
    let mut best = DfaKind::Raw(data.to_vec());
    let mut best_cost = n;

    // Constant
    if data.iter().all(|&b| b == data[0]) {
        let c = DfaKind::Constant(data[0], n);
        let cost = c.serialised_cost();
        if cost < best_cost { best_cost = cost; best = c; }
    }

    // Arithmetic
    if n >= 3 {
        let step = data[1].wrapping_sub(data[0]);
        if data.windows(2).all(|w| w[1].wrapping_sub(w[0]) == step) {
            let c = DfaKind::Arithmetic(data[0], step, n);
            let cost = c.serialised_cost();
            if cost < best_cost { best_cost = cost; best = c; }
        }
    }

    // Alternating
    if n >= 4 && n % 2 == 0 {
        let (a, b) = (data[0], data[1]);
        if a != b && data.iter().enumerate().all(|(i,&v)| v == if i%2==0 {a} else {b}) {
            let c = DfaKind::Alternating(a, b, n);
            let cost = c.serialised_cost();
            if cost < best_cost { best_cost = cost; best = c; }
        }
    }

    // Periodic
    'outer: for pl in 2..=(n/2).min(32) {
        if n % pl != 0 { continue; }
        let period  = &data[..pl];
        let repeats = n / pl;
        for i in 0..n {
            if data[i] != period[i % pl] { continue 'outer; }
        }
        let c = DfaKind::Periodic(period.to_vec(), repeats);
        let cost = c.serialised_cost();
        if cost < best_cost { best = c; }
        break;
    }

    best
}

// ─────────────────────────────────────────────────────────────────
// AXIOM LIBRARY
// ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum AxiomEntry {
    Dfa(DfaKind),
    Compound(Vec<u32>),
}

struct AxiomLib {
    entries: Vec<AxiomEntry>,      // indexed by axiom ID
    reverse: HashMap<Vec<u8>, u32>, // resolved bytes → ID
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
        let resolved = kind.generate();
        self.register(AxiomEntry::Dfa(kind), resolved)
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

    /// Prune unused entries and reindex IDs 0..N.
    fn reindex(&self, sequence: &[u32]) -> (Vec<AxiomEntry>, Vec<u32>) {
        let mut used = std::collections::HashSet::new();
        fn collect(lib: &AxiomLib, id: u32, used: &mut std::collections::HashSet<u32>) {
            if used.contains(&id) { return; }
            used.insert(id);
            if let AxiomEntry::Compound(refs) = &lib.entries[id as usize] {
                for &r in refs { collect(lib, r, used); }
            }
        }
        for &id in sequence { collect(self, id, &mut used); }

        let mut sorted: Vec<u32> = used.into_iter().collect();
        sorted.sort();
        let old_to_new: HashMap<u32,u32> = sorted.iter().enumerate()
            .map(|(new, &old)| (old, new as u32)).collect();

        let new_entries: Vec<AxiomEntry> = sorted.iter().map(|&old| {
            match &self.entries[old as usize] {
                AxiomEntry::Dfa(k)       => AxiomEntry::Dfa(k.clone()),
                AxiomEntry::Compound(rs) => AxiomEntry::Compound(rs.iter().map(|r| old_to_new[r]).collect()),
            }
        }).collect();

        let new_seq: Vec<u32> = sequence.iter().map(|id| old_to_new[id]).collect();
        (new_entries, new_seq)
    }
}

// ─────────────────────────────────────────────────────────────────
// INNER STREAM SERIALISATION
// ─────────────────────────────────────────────────────────────────

const TC_RAW:        u8 = 0x01;
const TC_COMPOUND:   u8 = 0x02;
const TC_CONSTANT:   u8 = 0x03;
const TC_ARITHMETIC: u8 = 0x04;
const TC_ALTERNATING:u8 = 0x05;
const TC_PERIODIC:   u8 = 0x06;

fn serialise_inner(entries: &[AxiomEntry], sequence: &[u32], filter: Filter) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(INNER_MAGIC);
    out.push(filter as u8);
    out.extend(encode_varint(entries.len() as u64));
    out.extend(encode_varint(sequence.len() as u64));

    for (id, entry) in entries.iter().enumerate() {
        out.extend(encode_varint(id as u64));
        match entry {
            AxiomEntry::Dfa(DfaKind::Raw(d)) => {
                out.push(TC_RAW);
                out.extend(encode_varint(d.len() as u64));
                out.extend_from_slice(d);
            }
            AxiomEntry::Compound(refs) => {
                out.push(TC_COMPOUND);
                out.extend(encode_varint(refs.len() as u64));
                for &r in refs { out.extend(encode_varint(r as u64)); }
            }
            AxiomEntry::Dfa(DfaKind::Constant(v, n)) => {
                out.push(TC_CONSTANT);
                out.push(*v);
                out.extend(encode_varint(*n as u64));
            }
            AxiomEntry::Dfa(DfaKind::Arithmetic(s, st, n)) => {
                out.push(TC_ARITHMETIC);
                out.push(*s); out.push(*st);
                out.extend(encode_varint(*n as u64));
            }
            AxiomEntry::Dfa(DfaKind::Alternating(a, b, n)) => {
                out.push(TC_ALTERNATING);
                out.push(*a); out.push(*b);
                out.extend(encode_varint(*n as u64));
            }
            AxiomEntry::Dfa(DfaKind::Periodic(p, r)) => {
                out.push(TC_PERIODIC);
                out.extend(encode_varint(p.len() as u64));
                out.extend_from_slice(p);
                out.extend(encode_varint(*r as u64));
            }
        }
    }
    for &id in sequence { out.extend(encode_varint(id as u64)); }
    out
}

fn deserialise_inner(blob: &[u8]) -> Result<(Vec<AxiomEntry>, Vec<u32>, Filter), AxmzipError> {
    if blob.len() < 6 || &blob[..4] != INNER_MAGIC {
        return Err(AxmzipError::InvalidData("Bad inner magic".into()));
    }
    let filter = Filter::from_u8(blob[4]);
    let mut off = 5usize;

    let (lib_count, o) = decode_varint(blob, off)?; off = o;
    let (seq_count, o) = decode_varint(blob, off)?; off = o;

    let mut entries: Vec<(u32, AxiomEntry)> = Vec::new();

    for _ in 0..lib_count {
        let (eid, o) = decode_varint(blob, off)?; off = o;
        let tc = blob[off]; off += 1;
        let entry = match tc {
            TC_RAW => {
                let (len, o) = decode_varint(blob, off)?; off = o;
                let d = blob[off..off+len as usize].to_vec(); off += len as usize;
                AxiomEntry::Dfa(DfaKind::Raw(d))
            }
            TC_COMPOUND => {
                let (rc, o) = decode_varint(blob, off)?; off = o;
                let mut refs = Vec::new();
                for _ in 0..rc {
                    let (r, o) = decode_varint(blob, off)?; off = o;
                    refs.push(r as u32);
                }
                AxiomEntry::Compound(refs)
            }
            TC_CONSTANT => {
                let v = blob[off]; off += 1;
                let (n, o) = decode_varint(blob, off)?; off = o;
                AxiomEntry::Dfa(DfaKind::Constant(v, n as usize))
            }
            TC_ARITHMETIC => {
                let s = blob[off]; off += 1;
                let st = blob[off]; off += 1;
                let (n, o) = decode_varint(blob, off)?; off = o;
                AxiomEntry::Dfa(DfaKind::Arithmetic(s, st, n as usize))
            }
            TC_ALTERNATING => {
                let a = blob[off]; off += 1;
                let b = blob[off]; off += 1;
                let (n, o) = decode_varint(blob, off)?; off = o;
                AxiomEntry::Dfa(DfaKind::Alternating(a, b, n as usize))
            }
            TC_PERIODIC => {
                let (pl, o) = decode_varint(blob, off)?; off = o;
                let p = blob[off..off+pl as usize].to_vec(); off += pl as usize;
                let (r, o) = decode_varint(blob, off)?; off = o;
                AxiomEntry::Dfa(DfaKind::Periodic(p, r as usize))
            }
            _ => return Err(AxmzipError::InvalidData(format!("Unknown type code {tc}")))
        };
        entries.push((eid as u32, entry));
    }

    entries.sort_by_key(|(id, _)| *id);
    let sorted_entries: Vec<AxiomEntry> = entries.into_iter().map(|(_, e)| e).collect();

    let mut sequence = Vec::with_capacity(seq_count as usize);
    for _ in 0..seq_count {
        let (id, o) = decode_varint(blob, off)?; off = o;
        sequence.push(id as u32);
    }

    Ok((sorted_entries, sequence, filter))
}

// ─────────────────────────────────────────────────────────────────
// CORE COMPRESS STREAM
// ─────────────────────────────────────────────────────────────────

fn compress_stream(data: &[u8], channels: u8) -> Vec<u8> {
    let (filter, filtered) = best_filter(data, channels);
    let n = filtered.len();
    let mut lib = AxiomLib::new();

    // Pattern frequency scan
    let mut freq: HashMap<Vec<u8>, usize> = HashMap::new();
    for length in 2..=(MAX_PAT_LEN.min(n)) {
        for i in 0..=(n.saturating_sub(length)) {
            *freq.entry(filtered[i..i+length].to_vec()).or_insert(0) += 1;
        }
    }

    // Score patterns, select top MAX_ATOMICS
    let mut candidates: Vec<(i64, Vec<u8>, DfaKind)> = freq.into_iter()
        .filter(|(_, f)| *f >= 2)
        .filter_map(|(pat, f)| {
            let dfa  = infer_dfa(&pat);
            let dc   = dfa.serialised_cost();
            let entry_cost = varint_len(MAX_ATOMICS as u64) + 1 + dc;
            let ref_size   = varint_len(MAX_ATOMICS as u64);
            let savings    = f as i64 * (pat.len() as i64 - ref_size as i64) - entry_cost as i64;
            if savings > 0 { Some((savings, pat, dfa)) } else { None }
        })
        .collect();

    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, _, dfa) in candidates.into_iter().take(MAX_ATOMICS) {
        lib.add_dfa(dfa);
    }

    // Greedy encode
    let mut sequence = Vec::new();
    let mut i = 0usize;
    while i < n {
        let mut best_id  = None;
        let mut best_len = 0usize;
        for length in (2..=MAX_PAT_LEN.min(n-i)).rev() {
            let chunk = &filtered[i..i+length];
            if let Some(&id) = lib.reverse.get(chunk) {
                best_id  = Some(id);
                best_len = length;
                break;
            }
        }
        if let Some(id) = best_id {
            sequence.push(id);
            i += best_len;
        } else {
            let id = lib.add_dfa(DfaKind::Raw(vec![filtered[i]]));
            sequence.push(id);
            i += 1;
        }
    }

    // Compound passes
    for _ in 0..MAX_PASSES {
        let mut pair_freq: HashMap<(u32,u32), usize> = HashMap::new();
        for w in sequence.windows(2) { *pair_freq.entry((w[0],w[1])).or_insert(0) += 1; }
        let useful: Vec<((u32,u32), usize)> = pair_freq.into_iter().filter(|(_,f)| *f > 5).collect();
        if useful.is_empty() { break; }

        let mut pair_map: HashMap<(u32,u32), u32> = HashMap::new();
        let mut sorted_pairs = useful;
        sorted_pairs.sort_by(|a,b| b.1.cmp(&a.1));
        for (pair, _) in sorted_pairs {
            let resolved: Vec<u8> = lib.resolve(pair.0).into_iter()
                .chain(lib.resolve(pair.1)).collect();
            let id = lib.add_compound(vec![pair.0, pair.1], resolved);
            pair_map.insert(pair, id);
        }

        let mut new_seq = Vec::new();
        let mut j = 0;
        while j < sequence.len() {
            if j + 1 < sequence.len() {
                let pair = (sequence[j], sequence[j+1]);
                if let Some(&id) = pair_map.get(&pair) {
                    new_seq.push(id); j += 2; continue;
                }
            }
            new_seq.push(sequence[j]); j += 1;
        }
        if new_seq.len() == sequence.len() { break; }
        sequence = new_seq;
    }

    let (final_entries, final_seq) = lib.reindex(&sequence);
    serialise_inner(&final_entries, &final_seq, filter)
}

fn decompress_stream(blob: &[u8]) -> Result<Vec<u8>, AxmzipError> {
    let (entries, sequence, filter) = deserialise_inner(blob)?;

    // Build resolution map
    fn resolve(entries: &[AxiomEntry], id: u32) -> Vec<u8> {
        match &entries[id as usize] {
            AxiomEntry::Dfa(k)       => k.generate(),
            AxiomEntry::Compound(rs) => rs.iter().flat_map(|&r| resolve(entries, r)).collect(),
        }
    }

    let filtered: Vec<u8> = sequence.iter().flat_map(|&id| resolve(&entries, id)).collect();
    Ok(reverse_filter(&filtered, filter))
}

// ─────────────────────────────────────────────────────────────────
// QUANTISATION (lossy mode)
// ─────────────────────────────────────────────────────────────────

fn quality_to_step(quality: u8) -> u8 {
    ((128.0 * (1.0 - quality as f64 / 100.0)).round() as u8).max(1)
}

fn quantize(data: &[u8], quality: u8) -> Vec<u8> {
    let step = quality_to_step(quality) as u16;
    data.iter().map(|&b| {
        let q = ((b as u16 + step/2) / step * step).min(255) as u8;
        q
    }).collect()
}

fn psnr(original: &[u8], reconstructed: &[u8]) -> f64 {
    let mse: f64 = original.iter().zip(reconstructed.iter())
        .map(|(&a, &b)| { let d = a as f64 - b as f64; d*d })
        .sum::<f64>() / original.len() as f64;
    if mse == 0.0 { return f64::INFINITY; }
    10.0 * (255.0f64.powi(2) / mse).log10()
}

fn max_error(original: &[u8], reconstructed: &[u8]) -> u8 {
    original.iter().zip(reconstructed.iter())
        .map(|(&a, &b)| (a as i16 - b as i16).unsigned_abs() as u8)
        .max().unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────
// OUTER SERIALISATION
// ─────────────────────────────────────────────────────────────────

fn serialise_plain(inner: &[u8], checksum: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(checksum);
    out.push(MODE_PLAIN);
    out.extend(encode_varint(inner.len() as u64));
    out.extend_from_slice(inner);
    out
}

fn serialise_stride(inner: &[u8], checksum: &[u8; 16], stride: u8) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(checksum);
    out.push(MODE_STRIDE);
    out.push(stride);
    out.extend(encode_varint(inner.len() as u64));
    out.extend_from_slice(inner);
    out
}

fn serialise_lossy(inner: &[u8], checksum_of_quantized: &[u8; 16], quality: u8, channels: u8) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(checksum_of_quantized);
    out.push(MODE_LOSSY);
    out.push(quality);
    out.push(channels);
    out.extend(encode_varint(inner.len() as u64));
    out.extend_from_slice(inner);
    out
}

// ─────────────────────────────────────────────────────────────────
// PUBLIC API
// ─────────────────────────────────────────────────────────────────

/// Compress `data` and return (compressed_bytes, stats).
///
/// - `quality`: 100 = lossless, 0–99 = lossy (higher = better quality, less compression)
/// - `channels`: 1 = mono/gray, 3 = RGB, 4 = RGBA
pub fn compress(data: &[u8], quality: u8, channels: u8) -> (Vec<u8>, CompressStats) {
    let t0    = std::time::Instant::now();
    let orig  = data.len();

    // ── Lossy path ────────────────────────────────────────────────
    if quality < 100 {
        let quantized        = quantize(data, quality);
        let psnr_val         = psnr(data, &quantized);
        let max_err          = max_error(data, &quantized);
        let checksum: [u8;16]= *md5::compute(&quantized);
        let inner            = compress_stream(&quantized, channels);
        let blob             = serialise_lossy(&inner, &checksum, quality, channels);
        let comp             = blob.len();
        let ratio            = (1.0 - comp as f64 / orig as f64) * 100.0;
        let step             = quality_to_step(quality);
        return (blob, CompressStats {
            original_bytes: orig, compressed_bytes: comp,
            ratio_pct: ratio, mode: format!("lossy(q={quality},step={step})"),
            lossless: false, psnr_db: psnr_val, max_error: max_err,
            lib_entries: 0,
            elapsed_ms: t0.elapsed().as_millis() as u64,
        });
    }

    // ── Lossless path — try plain then stride ────────────────────
    let checksum: [u8;16] = *md5::compute(data);

    // Plain v3 (delta filter + DFA + compound)
    let plain_inner = compress_stream(data, channels);
    let plain_blob  = serialise_plain(&plain_inner, &checksum);

    // Stride-field delta
    let (best_s, stride_data, _stride_ent) = best_stride(data);
    let (blob, mode) = if best_s > 0 {
        let stride_inner = compress_stream(&stride_data, channels);
        let stride_blob  = serialise_stride(&stride_inner, &checksum, best_s as u8);
        if stride_blob.len() < plain_blob.len() {
            (stride_blob, format!("stride(s={best_s})"))
        } else {
            (plain_blob, "plain".into())
        }
    } else {
        (plain_blob, "plain".into())
    };

    let comp  = blob.len();
    let ratio = (1.0 - comp as f64 / orig as f64) * 100.0;

    (blob, CompressStats {
        original_bytes: orig, compressed_bytes: comp,
        ratio_pct: ratio, mode,
        lossless: true, psnr_db: f64::INFINITY, max_error: 0,
        lib_entries: 0,
        elapsed_ms: t0.elapsed().as_millis() as u64,
    })
}

/// Decompress an axmzip blob. Returns original bytes (or quantized bytes for lossy).
pub fn decompress(blob: &[u8]) -> Result<Vec<u8>, AxmzipError> {
    if blob.len() < 22 { return Err(AxmzipError::InvalidData("Too short".into())); }
    if &blob[..4] != MAGIC  { return Err(AxmzipError::BadMagic); }
    if blob[4] != VERSION   { return Err(AxmzipError::BadVersion); }

    let checksum = &blob[5..21];
    let mut off  = 21usize;
    let mode     = blob[off]; off += 1;

    let data = match mode {
        MODE_PLAIN => {
            let (blen, o) = decode_varint(blob, off)?; off = o;
            decompress_stream(&blob[off..off+blen as usize])?
        }
        MODE_STRIDE => {
            let stride = blob[off] as usize; off += 1;
            let (blen, o) = decode_varint(blob, off)?; off = o;
            let raw = decompress_stream(&blob[off..off+blen as usize])?;
            stride_undelta(&raw, stride)
        }
        MODE_LOSSY => {
            off += 2; // skip quality + channels (already encoded in inner filter)
            let (blen, o) = decode_varint(blob, off)?; off = o;
            decompress_stream(&blob[off..off+blen as usize])?
        }
        _ => return Err(AxmzipError::InvalidData(format!("Unknown mode {mode}")))
    };

    let actual: [u8;16] = *md5::compute(&data);
    if actual.as_ref() != checksum { return Err(AxmzipError::ChecksumMismatch); }
    Ok(data)
}

/// Return true if the blob looks like a valid axmzip file.
pub fn is_axmzip(blob: &[u8]) -> bool {
    blob.len() >= 22 && &blob[..4] == MAGIC && blob[4] == VERSION
}

/// Return mode string from blob header without decompressing.
pub fn probe(blob: &[u8]) -> Option<String> {
    if !is_axmzip(blob) { return None; }
    match blob[21] {
        MODE_PLAIN  => Some("lossless/plain".into()),
        MODE_STRIDE => Some(format!("lossless/stride(s={})", blob[22])),
        MODE_LOSSY  => Some(format!("lossy(q={})", blob[22])),
        _           => None,
    }
}

// ─────────────────────────────────────────────────────────────────
// TESTS
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test] fn roundtrip_repetitive() {
        let data: Vec<u8> = b"\xAB\xCD\xEF\x01".repeat(2000);
        let (blob, stats) = compress(&data, 100, 1);
        assert!(stats.ratio_pct > 95.0);
        assert_eq!(decompress(&blob).unwrap(), data);
    }

    #[test] fn roundtrip_counter() {
        let data: Vec<u8> = (0..8000u32).map(|i| (i%256) as u8).collect();
        let (blob, stats) = compress(&data, 100, 1);
        assert!(stats.ratio_pct > 95.0);
        assert_eq!(decompress(&blob).unwrap(), data);
    }

    #[test] fn roundtrip_sensor() {
        let mut data = Vec::new();
        for i in 0u16..800 { data.extend_from_slice(&[0xFF, (i%64) as u8, (i*3%256) as u8, 0x04, 0x00]); }
        let (blob, stats) = compress(&data, 100, 1);
        assert!(stats.ratio_pct > 50.0, "Expected >50% got {:.1}%", stats.ratio_pct);
        assert_eq!(decompress(&blob).unwrap(), data);
    }

    #[test] fn lossy_roundtrip() {
        let data: Vec<u8> = (0..1000).map(|i| ((i as f64 * 0.1).sin() * 100.0 + 128.0) as u8).collect();
        let (blob, stats) = compress(&data, 75, 1);
        assert!(!stats.lossless);
        assert!(stats.psnr_db > 25.0);
        let recovered = decompress(&blob).unwrap();
        assert_eq!(recovered.len(), data.len());
    }

    #[test] fn varint_roundtrip() {
        for n in [0u64, 1, 127, 128, 16383, 16384, u32::MAX as u64] {
            let enc = encode_varint(n);
            let (dec, _) = decode_varint(&enc, 0).unwrap();
            assert_eq!(n, dec);
        }
    }

    #[test] fn dfa_constant()    { let d=vec![0xFFu8;100]; assert_eq!(infer_dfa(&d).generate(), d); }
    #[test] fn dfa_arithmetic()  { let d:Vec<u8>=(0u8..100).collect(); assert_eq!(infer_dfa(&d).generate(), d); }
    #[test] fn dfa_alternating() { let d:Vec<u8>=(0..100).map(|i| if i%2==0 {0xAA} else {0x55}).collect(); assert_eq!(infer_dfa(&d).generate(), d); }
    #[test] fn dfa_periodic()    { let d=b"ABC".repeat(30); assert_eq!(infer_dfa(&d).generate(), d); }
}
