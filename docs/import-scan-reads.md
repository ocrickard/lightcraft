# Import scan reads

An import scan describes every candidate (format, dimensions, capture time, whether only its
preview shows) and used to read every byte of every file to do it: the probe hashed the whole
file for its content hash. For a card of 1501 Hasselblad 3FR files (212 MB each, ~318 GB) over
a ~45 MB/s card reader, the review grid took about two hours to appear.

The scan probe (`fs_scan_probe`) reads headers, not samples. It loads only the byte ranges the
sparse header plan (`lightcraft_raw::sparse::header_plan`) names — the tables that describe the
file — and probes that buffer. Anything the plan cannot describe from part of a file is read
whole, exactly as before:

- small files (up to the 512 KB first prefix),
- formats whose header probe reads samples (Nikon NEF/NRW, Olympus ORF, Pentax PEF, Sony ARW6),
- an unclear structure (the plan never guesses).

Measured on the card above: 593,408 of 212,680,704 bytes per file (0.3 %), three cold files in
52 ms instead of ~15 s — about 300× faster, the full card in under a minute.

## Content hashes

The scan probe carries no content hash: hashing needs the bytes it skips. Duplicate detection
still matches by content, because only files whose size another file shares are hashed during
the scan — different sizes cannot hold the same bytes. The rest are hashed while the import
reads them anyway:

- *copy*: the source is hashed as it streams into the copy, and the copy is hashed back and
  compared (`copy_streaming_verified`) — one read of the source instead of two;
- *add* (in place): the imported files are hashed once, not the whole card;
- *move*: the verified destination is hashed (local, fast).

Every imported photo keeps a content hash, as before.

## Correctness

Probing the partly loaded buffer must give exactly what probing the whole file gives. That is
what `crates/raw/tests/sparse_corpus.rs` checks on every CC0 sample (with the skipped bytes
zero and again 0xFF, so a reader that looked at skipped data would be caught): 111 files
across 10 formats, plus 2 Hasselblad 3FR files. The one hole it found this way — Sony ARW6
probes as ARW but validates its whole strip in header mode — is read whole (see
`tiff_plan`).
