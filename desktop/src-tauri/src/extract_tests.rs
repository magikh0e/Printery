//! Tests for pv_extract, the one place this app writes files that come out of
//! an archive somebody downloaded.
//!
//! Added with the move from zip 2 to zip 9 in 0.3.3. That upgrade also changed
//! every decompressor underneath: deflate moved to the pure Rust zlib-rs
//! backend, bzip2 swapped its C library for a Rust port, and zstd went up a
//! major version. So the first test round-trips all four methods the app
//! claims to read, and the rest build hostile archives on purpose: a name that
//! climbs out of the destination, one rooted at / , Windows stream and device
//! names, a name that is not UTF-8, a local header that disagrees with the
//! central directory, and a symlink that lies about its size. The last two are
//! the shapes of GHSA-m665-v7pm-7grj and GHSA-q7v9-x93c-xqv3.
//!
//! Two of these assert zip 9's behaviour rather than an ideal: a rooted name
//! is stripped and lands inside rather than being refused, and a name that is
//! not UTF-8 is read as the DOS code page rather than refused. Both are safe.
//! If a future zip changes either, these fail and say so, which is the point.
//!
//! Each test works in its own folder under the system temp directory and
//! removes it afterwards. cargo test runs them; check.yml does not.

use super::*;
use std::io::{Cursor, Write};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "printery-extract-test-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
    fn root(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn opts(m: CompressionMethod) -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(m)
}

/// Build an archive in memory from (name, contents) pairs.
fn build(entries: &[(&str, &[u8])], method: CompressionMethod) -> Vec<u8> {
    let mut w = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, data) in entries {
        w.start_file(*name, opts(method)).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// Replace every occurrence of `from` with `to` (same length) in the raw
/// bytes, or only the first `limit` of them. Used to forge headers.
fn patch(bytes: &mut [u8], from: &[u8], to: &[u8], limit: usize) -> usize {
    assert_eq!(from.len(), to.len(), "patches must keep the length");
    let mut n = 0;
    let mut i = 0;
    while i + from.len() <= bytes.len() && n < limit {
        if &bytes[i..i + from.len()] == from {
            bytes[i..i + from.len()].copy_from_slice(to);
            n += 1;
            i += from.len();
        } else {
            i += 1;
        }
    }
    n
}

fn extract(s: &Scratch, archive: &[u8]) -> ExtractResult {
    fs::write(s.0.join("in.zip"), archive).unwrap();
    tauri::async_runtime::block_on(pv_extract(s.root(), "in.zip".into(), "out".into())).unwrap()
}

#[test]
fn every_method_the_app_reads_round_trips() {
    let big: Vec<u8> = (0..200_000u32).flat_map(|i| (i % 251).to_le_bytes()).collect();
    for method in [
        CompressionMethod::Stored,
        CompressionMethod::Deflated,
        CompressionMethod::Bzip2,
        CompressionMethod::Zstd,
    ] {
        let s = Scratch::new("methods");
        let zip = build(
            &[
                ("model.stl", b"solid cube\nendsolid cube\n"),
                ("parts/inner/plate.3mf", &big),
                ("readme.txt", b"printed at 0.2 mm"),
            ],
            method,
        );
        let r = extract(&s, &zip);
        assert_eq!((r.written, r.failed), (3, 0), "{method:?}: {:?}", r.problems);
        let out = s.0.join("out");
        assert_eq!(fs::read(out.join("model.stl")).unwrap(), b"solid cube\nendsolid cube\n");
        assert_eq!(fs::read(out.join("parts/inner/plate.3mf")).unwrap(), big, "{method:?}");
        assert_eq!(fs::read(out.join("readme.txt")).unwrap(), b"printed at 0.2 mm");
    }
}

/// Everything under `dir`, as paths relative to it.
fn files_under(dir: &Path) -> Vec<String> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/"))
        .collect()
}

#[test]
fn names_that_climb_out_are_refused_and_rooted_ones_stay_inside() {
    let s = Scratch::new("traversal");
    // Same length placeholders, rewritten into hostile names after writing,
    // since a well behaved writer will not produce them on its own.
    let mut zip = build(
        &[("aa/escape.txt", b"x"), ("abs-name.txt", b"y"), ("fine.txt", b"z")],
        CompressionMethod::Stored,
    );
    assert_eq!(patch(&mut zip, b"aa/escape.txt", b"../escape.txt", usize::MAX), 2);
    assert_eq!(patch(&mut zip, b"abs-name.txt", b"/bs-name.txt", usize::MAX), 2);
    let r = extract(&s, &zip);
    // .. above the destination is refused outright.
    assert!(!s.0.join("escape.txt").exists(), "a file climbed out of the destination");
    assert!(r.problems.iter().any(|p| p.contains("../escape.txt") && p.contains("refused")));
    // A leading / is stripped by zip 9 rather than refused, so it lands
    // inside the destination. What matters is that it never leaves it.
    assert_eq!((r.written, r.failed), (2, 1), "{:?}", r.problems);
    let mut inside = files_under(&s.0.join("out"));
    inside.sort();
    assert_eq!(inside, ["bs-name.txt", "fine.txt"]);
    assert_eq!(files_under(&s.0).len(), 3, "only in.zip and the two extracted files may exist");
}

#[cfg(windows)]
#[test]
fn windows_alternate_data_streams_and_device_names_are_refused() {
    // "file:stream" writes a hidden stream onto another file, and CON or NUL
    // are devices, not files. zip 9 refuses both on Windows.
    let s = Scratch::new("ads");
    let mut zip = build(
        &[("ads-file.txt", b"x"), ("NUX.txt", b"y"), ("fine.txt", b"z")],
        CompressionMethod::Stored,
    );
    assert_eq!(patch(&mut zip, b"ads-file.txt", b"ads:file.txt", usize::MAX), 2);
    assert_eq!(patch(&mut zip, b"NUX.txt", b"NUL.txt", usize::MAX), 2);
    let r = extract(&s, &zip);
    assert_eq!((r.written, r.failed), (1, 2), "{:?}", r.problems);
    assert_eq!(files_under(&s.0.join("out")), ["fine.txt"]);
}

#[test]
fn a_name_that_is_not_utf8_is_read_as_dos_and_stays_inside() {
    // A non-ASCII name makes the writer set the UTF-8 flag, then the bytes are
    // made invalid UTF-8 in both headers. zip 9 falls back to the DOS code
    // page, which decodes any byte, so this extracts under an odd name
    // rather than failing. It has to land inside the destination either way.
    let s = Scratch::new("not-utf8");
    let mut zip = build(&[("caf\u{e9}.txt", b"x")], CompressionMethod::Stored);
    assert_eq!(patch(&mut zip, "caf\u{e9}".as_bytes(), b"caf\xff\xfe", usize::MAX), 2);
    let r = extract(&s, &zip);
    assert_eq!((r.written, r.failed), (1, 0), "{:?}", r.problems);
    let inside = files_under(&s.0.join("out"));
    assert_eq!(inside.len(), 1);
    assert!(inside[0].starts_with("caf") && inside[0].ends_with(".txt"), "{inside:?}");
    assert_eq!(files_under(&s.0).len(), 2);
}

#[test]
fn a_local_header_that_disagrees_never_names_the_output() {
    // The shape of GHSA-m665-v7pm-7grj: the central directory says one name,
    // the local header in front of the data says another. Only the central
    // directory may decide where anything is written.
    let s = Scratch::new("differential");
    let mut zip = build(&[("safe.txt", b"payload")], CompressionMethod::Stored);
    // The first occurrence is the local header, which comes before the data.
    assert_eq!(patch(&mut zip, b"safe.txt", b"evil.txt", 1), 1);
    let r = extract(&s, &zip);
    assert!(!s.0.join("out/evil.txt").exists(), "the local header named a file");
    assert!(!s.0.join("evil.txt").exists());
    // zip 9 may extract it under the central directory name or refuse the
    // entry outright; either is safe, and the count has to add up.
    assert_eq!(r.written + r.failed, 1, "{:?}", r.problems);
}

#[test]
fn a_symlink_that_lies_about_its_size_neither_aborts_nor_writes() {
    // The shape of GHSA-q7v9-x93c-xqv3: a symlink entry declaring a huge
    // uncompressed size. The crate's own extract() reserved that much memory
    // up front and aborted the process. pv_extract never calls it, and must
    // come back with an answer rather than taking the app down.
    let s = Scratch::new("symlink");
    let mut w = ZipWriter::new(Cursor::new(Vec::new()));
    w.add_symlink("link", "target", opts(CompressionMethod::Stored)).unwrap();
    let mut zip = w.finish().unwrap().into_inner();
    // Central directory header: signature PK\x01\x02, uncompressed size at +24.
    let cd = zip.windows(4).position(|x| x == b"PK\x01\x02").unwrap();
    zip[cd + 24..cd + 28].copy_from_slice(&0xFFFF_FFFEu32.to_le_bytes());
    let r = extract(&s, &zip);
    assert_eq!(r.written, 0, "{:?}", r.problems);
    assert_eq!(r.failed, 1);
    assert!(!s.0.join("out/link").exists(), "a partial write was left behind");
}
