"""
Axmzip v1: Axiom-Based Binary Compression
============================================
Key improvements over v2:
  - Library pruning: only keep equations actually used in final sequence
  - Variable-length ID encoding (fewer bytes for small libraries)
  - Smarter atomic selection: limit candidates, avoid overfitting small data
  - Re-ID after pruning for minimal encoding size

Serialization (binary):
  [magic 4B][version 1B][checksum 16B]
  [lib_count varint][seq_count varint]
  For each lib entry: [type 1B][id varint][payloadlen varint][payload]
  Sequence: [id varint] * seq_count
"""

import os, time, hashlib, struct, random
from collections import defaultdict


MAGIC   = b"AXM1"
VERSION = b"\x03"


# ─────────────────────────────────────────────
# VARIABLE-LENGTH INTEGER (LEB128)
# ─────────────────────────────────────────────

def encode_varint(n: int) -> bytes:
    parts = []
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            parts.append(b | 0x80)
        else:
            parts.append(b)
            break
    return bytes(parts)


def decode_varint(data: bytes, offset: int):
    result, shift = 0, 0
    while True:
        b = data[offset]; offset += 1
        result |= (b & 0x7F) << shift
        if not (b & 0x80):
            break
        shift += 7
    return result, offset


# ─────────────────────────────────────────────
# SERIALIZATION
# ─────────────────────────────────────────────

def serialize(library: dict, sequence: list, checksum: bytes) -> bytes:
    parts = [MAGIC, VERSION, checksum]
    parts.append(encode_varint(len(library)))
    parts.append(encode_varint(len(sequence)))

    for eq_id in sorted(library):
        e = library[eq_id]
        parts.append(encode_varint(eq_id))
        if e["type"] == "atomic":
            d = e["data"]
            parts.append(b"\x01")
            parts.append(encode_varint(len(d)))
            parts.append(d)
        else:
            refs = e["refs"]
            parts.append(b"\x02")
            parts.append(encode_varint(len(refs)))
            for r in refs:
                parts.append(encode_varint(r))

    for eq_id in sequence:
        parts.append(encode_varint(eq_id))

    return b"".join(parts)


def deserialize(blob: bytes):
    assert blob[:4] == MAGIC, "Bad magic"
    assert blob[4:5] == VERSION, "Bad version"
    checksum = blob[5:21]
    offset   = 21

    lib_count, offset = decode_varint(blob, offset)
    seq_count, offset = decode_varint(blob, offset)

    library = {}
    for _ in range(lib_count):
        eq_id, offset  = decode_varint(blob, offset)
        typ            = blob[offset]; offset += 1
        if typ == 0x01:
            length, offset = decode_varint(blob, offset)
            data           = blob[offset:offset+length]; offset += length
            library[eq_id] = {"type": "atomic", "data": data}
        else:
            ref_count, offset = decode_varint(blob, offset)
            refs = []
            for _ in range(ref_count):
                r, offset = decode_varint(blob, offset)
                refs.append(r)
            library[eq_id] = {"type": "compound", "refs": refs}

    sequence = []
    for _ in range(seq_count):
        eid, offset = decode_varint(blob, offset)
        sequence.append(eid)

    return library, sequence, checksum


# ─────────────────────────────────────────────
# EQUATION LIBRARY
# ─────────────────────────────────────────────

class EquationLibrary:
    def __init__(self):
        self.lib     = {}    # id → {"type","data"|"refs"}
        self.reverse = {}    # bytes → id
        self.next_id = 0

    def add_atomic(self, data: bytes) -> int:
        if data in self.reverse:
            return self.reverse[data]
        eid = self.next_id
        self.lib[eid]      = {"type": "atomic", "data": data}
        self.reverse[data] = eid
        self.next_id += 1
        return eid

    def add_compound(self, refs: list, resolved: bytes) -> int:
        if resolved in self.reverse:
            return self.reverse[resolved]
        eid = self.next_id
        self.lib[eid]          = {"type": "compound", "refs": refs}
        self.reverse[resolved] = eid
        self.next_id += 1
        return eid

    def resolve(self, eid: int) -> bytes:
        e = self.lib[eid]
        if e["type"] == "atomic":
            return e["data"]
        return b"".join(self.resolve(r) for r in e["refs"])

    def reindex(self, sequence: list) -> (dict, list):
        """Prune unused entries, remap IDs to 0-N consecutively."""
        # Collect all used IDs (transitively)
        used = set()
        def collect(eid):
            if eid in used:
                return
            used.add(eid)
            e = self.lib[eid]
            if e["type"] == "compound":
                for r in e["refs"]:
                    collect(r)

        for eid in sequence:
            collect(eid)

        # Remap IDs
        old_to_new = {old: new for new, old in enumerate(sorted(used))}
        new_lib = {}
        for old, new in old_to_new.items():
            e = self.lib[old]
            if e["type"] == "atomic":
                new_lib[new] = {"type": "atomic", "data": e["data"]}
            else:
                new_lib[new] = {"type": "compound", "refs": [old_to_new[r] for r in e["refs"]]}

        new_seq = [old_to_new[eid] for eid in sequence]
        return new_lib, new_seq


# ─────────────────────────────────────────────
# COMPRESSOR
# ─────────────────────────────────────────────

class Axmzip:
    def __init__(self,
                 min_len=2, max_len=48,
                 max_atomics=256,
                 max_passes=12,
                 verbose=True):
        self.min_len    = min_len
        self.max_len    = max_len
        self.max_atomics = max_atomics
        self.max_passes = max_passes
        self.verbose    = verbose

    def _log(self, *a):
        if self.verbose:
            print(*a)

    # ── Atomic discovery ──────────────────────

    def _find_atomics(self, data: bytes, lib: EquationLibrary):
        freq = defaultdict(int)
        n    = len(data)
        for length in range(self.min_len, min(self.max_len + 1, n + 1)):
            for i in range(n - length + 1):
                freq[data[i:i+length]] += 1

        # Savings = bytes_saved_in_raw_seq - library_entry_cost
        # Bytes saved = freq * (len - ID_varint_size)
        # Entry cost  = varint(id) + 1(type) + varint(len) + len
        # Estimate ID varint as 1 byte (small libraries)
        def savings(pat, f):
            entry = 1 + 1 + 1 + len(pat)    # id(1) + type(1) + len(1) + data
            ref_size = max(1, len(pat) - 2)  # bytes saved per reference ≈ len-2
            return f * ref_size - entry

        ranked = sorted(
            [(p, f) for p, f in freq.items() if f > 1 and savings(p, f) > 0],
            key=lambda x: -savings(x[0], x[1])
        )

        selected = ranked[:self.max_atomics]
        self._log(f"  [Pass 1] {len(freq):,} candidates → {len(selected)} selected atomic patterns")
        for pat, _ in selected:
            lib.add_atomic(pat)

    def _encode(self, data: bytes, lib: EquationLibrary) -> list:
        seq = []
        i   = 0
        while i < len(data):
            best_id, best_len = None, 0
            for length in range(min(self.max_len, len(data) - i), self.min_len - 1, -1):
                chunk = data[i:i+length]
                if chunk in lib.reverse:
                    best_id, best_len = lib.reverse[chunk], length
                    break
            if best_id is None:
                b = data[i:i+1]
                best_id   = lib.add_atomic(b)
                best_len  = 1
            seq.append(best_id)
            i += best_len
        return seq

    # ── Compound passes ───────────────────────

    def _compound_pass(self, seq: list, lib: EquationLibrary, pass_num: int) -> list:
        pair_freq = defaultdict(int)
        for i in range(len(seq) - 1):
            pair_freq[(seq[i], seq[i+1])] += 1

        def savings(pair, f):
            # Each occurrence currently costs 2 varints in sequence
            # After compounding: 1 varint + entry overhead
            # Entry overhead ≈ 1(id) + 1(type) + 1(len) + 2(refs) = 5 bytes
            return f - 5   # net references saved minus entry cost

        useful = {p: f for p, f in pair_freq.items() if savings(p, f) > 0}

        if not useful:
            return seq

        self._log(f"  [Pass {pass_num}] {len(useful)} compound pairs → seq {len(seq)}")

        pair_map = {}
        for pair in sorted(useful, key=lambda p: -useful[p]):
            resolved = lib.resolve(pair[0]) + lib.resolve(pair[1])
            cid      = lib.add_compound(list(pair), resolved)
            pair_map[pair] = cid

        new_seq, i = [], 0
        while i < len(seq):
            if i < len(seq) - 1:
                pair = (seq[i], seq[i+1])
                if pair in pair_map:
                    new_seq.append(pair_map[pair])
                    i += 2; continue
            new_seq.append(seq[i])
            i += 1

        return new_seq

    # ── Main ──────────────────────────────────

    def compress(self, data: bytes) -> tuple:
        t0  = time.time()
        lib = EquationLibrary()

        self._log(f"\n{'─'*52}")
        self._log(f"  Axmzip v1 — {len(data):,} bytes")
        self._log(f"{'─'*52}")

        self._find_atomics(data, lib)
        seq = self._encode(data, lib)
        self._log(f"  [Pass 1] Encoded → {len(seq):,} IDs  (lib: {len(lib.lib)})")

        for p in range(2, self.max_passes + 1):
            prev = len(seq)
            seq  = self._compound_pass(seq, lib, p)
            if len(seq) == prev:
                self._log(f"  [Pass {p}] Converged.")
                break

        # Prune & reindex
        new_lib, new_seq = lib.reindex(seq)
        self._log(f"  Pruned library: {len(lib.lib)} → {len(new_lib)} entries")
        self._log(f"  Final sequence: {len(new_seq):,} IDs")

        checksum = hashlib.md5(data).digest()
        blob     = serialize(new_lib, new_seq, checksum)

        orig    = len(data)
        comp    = len(blob)
        ratio   = (1 - comp / orig) * 100
        elapsed = time.time() - t0

        self._log(f"  Original   : {orig:,} B")
        self._log(f"  Compressed : {comp:,} B")
        self._log(f"  Ratio      : {ratio:+.2f}%")
        self._log(f"  Time       : {elapsed:.3f}s")

        return blob, {
            "original_bytes"  : orig,
            "compressed_bytes": comp,
            "ratio_pct"       : round(ratio, 2),
            "lib_entries"     : len(new_lib),
            "seq_len"         : len(new_seq),
            "elapsed_s"       : round(elapsed, 3)
        }

    def decompress(self, blob: bytes) -> bytes:
        library, sequence, checksum = deserialize(blob)
        lib = EquationLibrary()
        lib.lib = library
        data = b"".join(lib.resolve(eid) for eid in sequence)
        assert hashlib.md5(data).digest() == checksum, "Checksum mismatch!"
        return data


# ─────────────────────────────────────────────
# TEST SUITE
# ─────────────────────────────────────────────

def make_datasets():
    random.seed(42)
    ds = {}

    # 1. Perfect repetition
    ds["repetitive_binary"]  = b"\xAB\xCD\xEF\x01" * 2000          # 8KB

    # 2. Repetitive with noise
    base = b"\xAB\xCD\xEF\x01"
    varied = b""
    for i in range(2000):
        chunk = bytearray(base)
        chunk[i % 4] ^= (i % 8)
        varied += bytes(chunk)
    ds["varied_repetitive"]  = varied                                # 8KB

    # 3. ASCII log file
    line = b"[INFO]  2024-03-01 user_id=42 action=login latency=12ms status=200\n"
    ds["log_text"]           = line * 300                           # ~20KB

    # 4. Source code
    code = b"for i in range(n):\n    result += compute(data[i], threshold)\n    if result > limit: break\n"
    ds["source_code"]        = code * 200                           # ~18KB

    # 5. Structured binary (sensor packets)
    sensor = b""
    for i in range(800):
        sensor += struct.pack(">BBHH", 0xFF, i % 64, i * 3 % 1024, 1024)
    ds["sensor_packets"]     = sensor                               # ~5KB

    # 6. DNA sequence (4-letter alphabet)
    bases = b"ATGC"
    ds["dna_sequence"]       = bytes(random.choice(bases) for _ in range(8000))  # 8KB

    # 7. Random (worst case)
    ds["random_data"]        = bytes(random.randint(0, 255) for _ in range(5000)) # 5KB

    # 8. Sparse zeros
    sparse = bytearray(8000)
    for i in range(0, 8000, 40):
        sparse[i] = random.randint(1, 255)
    ds["sparse_zeros"]       = bytes(sparse)                        # 8KB

    return ds


def run_tests():
    print("\n" + "="*68)
    print("  Axmzip v1 — Compression Test Suite")
    print("="*68)

    datasets = make_datasets()
    results  = []

    for name, data in datasets.items():
        print(f"\n>>> {name}  ({len(data):,} bytes)")
        ez = Axmzip(verbose=True, max_atomics=256, max_passes=12)
        blob, stats = ez.compress(data)

        # Verify decompression
        recovered = ez.decompress(blob)
        ok = "✓" if recovered == data else "✗ FAILED"
        print(f"  Decompression: {ok}")

        # Also compare with zlib for reference
        import zlib
        zlib_size = len(zlib.compress(data, level=9))
        print(f"  zlib (lvl9)  : {zlib_size:,} bytes  ({(1-zlib_size/len(data))*100:.1f}% reduction)")

        results.append({"name": name, "zlib_bytes": zlib_size, **stats})

    # Summary table
    print("\n\n" + "="*85)
    print(f"  {'Dataset':<22} {'Original':>9} {'Axmzip':>9} {'Axmzip%':>8} {'zlib':>9} {'zlib%':>7} {'Lib':>6}")
    print(f"  {'-'*22} {'-'*9} {'-'*9} {'-'*8} {'-'*9} {'-'*7} {'-'*6}")
    for r in results:
        eq_sym  = "▼" if r["ratio_pct"] > 0 else "▲"
        zl_pct  = (1 - r["zlib_bytes"]/r["original_bytes"])*100
        print(f"  {r['name']:<22} {r['original_bytes']:>9,} {r['compressed_bytes']:>9,} "
              f"{eq_sym}{abs(r['ratio_pct']):>6.1f}% {r['zlib_bytes']:>9,} "
              f"  {zl_pct:>5.1f}% {r['lib_entries']:>6}")

    print("\n  ▼ smaller than original   ▲ larger than original")
    print("  zlib shown as reference baseline\n")
    return results


if __name__ == "__main__":
    import struct
    results = run_tests()
