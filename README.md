# Axmzip — Axiom-Based Binary Compression

> *What if instead of storing data, you stored the rules that generate it?*

Axmzip is a lossless compression algorithm that represents binary data as a **compounding library of axioms** — patterns that reference other patterns, building a hierarchical tree of rules that generates the original data from a tiny root.

---

## The Idea

Traditional compressors find repeated patterns and replace them with shorter references. Axmzip goes deeper: it finds axioms *between* those references, then axioms between those axioms — compounding across multiple passes until the data collapses into a minimal set of rules.

```
Raw binary:   10110101 10110101 10110101 10110101 ...

Pass 1:  E1 = [10110101]              ← atomic axiom
Pass 2:  E2 = [E1, E1]               ← compound axiom
Pass 3:  E3 = [E2, E2]               ← axiom of axioms
Pass 4:  E4 = [E3, E3]               ← deeper still
...
Result:  [E6]                         ← entire file = one reference
```

This mirrors how the brain works: you don't remember every frame of a movie — you remember the *pattern* of what happened, and reconstruct the details on demand.

---

## Results

| Dataset | Original | Compressed | Reduction |
|---|---|---|---|
| Repetitive binary | 8,000 B | 142 B | **98.2%** |
| Varied repetitive | 8,000 B | 232 B | **97.1%** |
| Source code | 18,000 B | 1,062 B | **94.1%** |
| Sparse data | 8,000 B | 1,354 B | **83.1%** |
| Log files | 20,100 B | 4,198 B | **79.1%** |
| DNA sequences | 8,000 B | 4,616 B | **42.3%** |
| Random data | 5,000 B | 8,568 B | ▲ (expected — Shannon limit) |

✓ All decompressions verified lossless via MD5 checksum.

Random data correctly fails to compress — Axmzip respects Shannon's entropy limit and does not make false compression claims.

---

## How It Works

### Pass 1 — Atomic Discovery
Scans the binary stream with a sliding window. For each recurring pattern, calculates net space savings (bytes saved across all occurrences minus library entry cost). The top-N patterns by savings are registered as **atomic axioms**.

### Pass 2+ — Compound Discovery
The data stream is now a sequence of axiom IDs. Each pass scans for the most frequent **adjacent pairs** of IDs and replaces them with a new **compound axiom**. This continues until no further savings are possible.

### Pruning
After all passes, unused library entries are removed. IDs are remapped to 0..N consecutively for minimal varint encoding. In the best case (repetitive binary), 193 candidate axioms pruned to 7.

### Serialization
Binary format using LEB128 variable-length integers throughout. Small libraries cost very little: a 7-entry library uses fewer than 100 bytes.

---

## Installation

No dependencies beyond Python 3.8+.

```bash
git clone https://github.com/erdous/axmzip
cd axmzip
python axmzip.py          # runs full test suite
```

---

## Usage

```python
from axmzip import Axmzip

ez = Axmzip()

# Compress
blob, stats = ez.compress(data)   # data: bytes
print(stats)
# {'original_bytes': 18000, 'compressed_bytes': 1062, 'ratio_pct': 94.1, ...}

# Decompress — always lossless
recovered = ez.decompress(blob)
assert recovered == data  # guaranteed
```

### Configuration

```python
ez = Axmzip(
    min_len=2,        # minimum pattern length for atomic scan
    max_len=48,       # maximum pattern length for atomic scan
    max_atomics=256,  # max atomic patterns to register per run
    max_passes=12,    # max compound passes before stopping
    verbose=True      # print per-pass progress
)
```

---

## Compression Format

```
[magic 4B "EQZ3"][version 1B][MD5 checksum 16B]
[lib_count varint][seq_count varint]
[library entries...]
[sequence of IDs...]
```

Each library entry:
```
[eq_id varint][type 1B: 0x01=atomic, 0x02=compound]
  atomic:   [length varint][raw bytes]
  compound: [ref_count varint][ref_id varint ...]
```

---

## Relationship to Prior Work

Axmzip's compounding mechanism is related to:

- **Re-Pair** (Larsson & Moffat, 1999) — repeatedly replaces the most frequent pair with a new symbol
- **Sequitur** (Nevill-Manning, 1997) — builds a hierarchical grammar from recurrences
- **Byte Pair Encoding (BPE)** — the tokenization algorithm behind modern LLMs, originally a compression technique

Axmzip's distinguishing angle is the **explicit axiom library** with multi-pass hierarchical compounding, binary varint serialization, and the framing of compression as *finding generating rules* rather than finding repetitions.

---

## Current Limitations

- Not yet competitive with zlib on most general data (zlib uses implicit back-references that cost only 2–3 bytes with no explicit library overhead)
- Sequential/counter data (sensor packets) resists compression without delta pre-processing
- No streaming support yet — processes full input in memory

## Roadmap

- [ ] Entropy detection: auto-fallback to raw passthrough for incompressible blocks
- [ ] Delta encoding pre-processing for time-series and counter data
- [ ] Arithmetic coding on the final ID sequence for additional gains
- [ ] Sliding-window atomic references (LZ77-style) to eliminate small-library overhead
- [ ] Streaming mode for large files

---

## Contributing

PRs welcome. The most impactful next steps are entropy detection and delta pre-processing — these would unlock competitive performance on sensor/telemetry data where the idea should shine.

---

## License

MIT — use freely, attribution appreciated.
