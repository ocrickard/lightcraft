# Import scan reads

An import scan describes every candidate (format, dimensions, capture time, whether only its
preview shows) and used to read every byte of every file to do it: the probe hashed the whole
file for its content hash. For a card of 1501 Hasselblad 3FR files (212 MB each, ~318 GB) over
a ~45 MB/s card reader, the review grid took about two hours to appear.

The scan probe (`fs_scan_probe`) reads headers, not samples. It loads only the byte ranges the
sparse header plan (`lightcraft_raw::sparse::header_plan`) names — the tables that describe the
file — and probes that buffer. (The buffer is the file's full length, the unnamed bytes zero,
because the tables' offsets are absolute and the probe takes a whole-file `&[u8]`; only the
named ranges are read into it, and its memory permit counts those, as the preview loader's
does. A whole-file fallback counts the file, its permit taken before that read.) Anything the
plan cannot describe from part of a file is read whole, exactly as before:

- small files (up to the 512 KB first prefix),
- formats whose header probe reads samples (Nikon NEF/NRW, Olympus ORF, Pentax PEF, Sony ARW6),
- an unclear structure (the plan never guesses).

Measured on the author's card above: 593,408 of 212,680,704 bytes per file (0.3 %), three cold
files in 52 ms instead of ~15 s — about 300× faster.

## Content hashes

The scan probe carries no content hash: hashing needs the bytes it skips. Duplicate detection
still matches by content, but size alone is a weak filter — on the card above, 1396 of the
1501 files share their size with another file, so hashing every size-sharer still read
~295 GB whole, sequentially, behind a frozen counter.

The hash fill therefore compares a 64 KB head sample first and groups files by (size,
sample): only a group that could hold duplicates — two candidates, or a candidate and a
library file — is hashed whole to confirm it. Anything else certainly differs. The library's
own files of those sizes are sampled the same way (local reads); sizes that cannot be
sampled (no file, or an unreadable one) fall back to whole hashes, compared against the
stored ones. Small files hash from their sample read. The samples and the confirmations run
several at once, counted on the scan's progress, and stop when it is cancelled.

End to end — probe, samples, and (on the card above: zero) confirmations — the 1501-file
card scans in ~41 s.

The rest are hashed while the import reads them anyway:

- *copy*: the source is hashed as it streams into the copy, and the copy is hashed back and
  compared (`copy_streaming_verified`) — one read of the source instead of two;
- *add* (in place): the imported files are hashed once, not the whole card;
- *move*: the verified destination is hashed (local, fast); if that read fails the photo
  keeps no hash rather than failing a move already verified byte for byte.

Every other imported photo keeps a content hash, as before.

## Correctness

Probing the partly loaded buffer must give exactly what probing the whole file gives. That is
what `crates/raw/tests/sparse_corpus.rs` checks on every CC0 sample in the corpus (with the
skipped bytes zero and again 0xFF, so a reader that looked at skipped data would be caught);
it also checks `bounded_preview`'s header answer against the whole-file preview on any sample
that exercises it. The downloadable corpus (`cargo xtask corpus --download`) has no Hasselblad
sample yet: the card numbers above and the 3FR checks rest on the author's own 907X CFV 100C
files, run through the same tests from a private corpus during development. The one hole the
corpus test found this way — Sony ARW6 probes as ARW but validates its whole strip in header
mode — is read whole (see `tiff_plan`).
