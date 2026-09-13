//! Decoders, the pool that positions them, and the checksum they carry.

mod common;

use common::{
    Entry, TempArchive, archive, archive_with_stream, compressible, deflate, pseudo_random,
    stored_blocks,
};
use rawzip::CompressionMethod;
use std::sync::Arc;
use zipfs::config::{SKIP_BUFFER, Symlinks};
use zipfs::decode::{CoreParts, EntryLocation, PositionedDecoder};
use zipfs::index::{Index, Method, ROOT_INO, VERIFY_BAD, VERIFY_OK};
use zipfs::pool::{DecoderBudget, EntryPool};
use zipfs::{Archive, Config};

const STORE: CompressionMethod = CompressionMethod::STORE;
const DEFLATE: CompressionMethod = CompressionMethod::DEFLATE;
const DEFLATE64: CompressionMethod = CompressionMethod::DEFLATE64;
#[cfg(feature = "zstd")]
const ZSTD: CompressionMethod = CompressionMethod::ZSTD;

struct Fixture {
    _temp: TempArchive,
    archive: Archive,
    index: Index,
}

impl Fixture {
    fn new(name: &str, entries: &[Entry]) -> Fixture {
        let temp = TempArchive::new(name, &archive(entries));
        let archive = Archive::open(&temp.path).unwrap();
        let (index, _) = zipfs::index::build(&archive, Symlinks::default()).unwrap();
        Fixture {
            _temp: temp,
            archive,
            index,
        }
    }

    fn locate(&self, name: &str) -> (EntryLocation, u32) {
        let ino = self.index.lookup(ROOT_INO, name.as_bytes()).unwrap();
        let node = self.index.node(ino).unwrap();
        let meta = self.index.meta(node).unwrap();
        let loc = EntryLocation {
            data_start: self.archive.data_start(meta).unwrap(),
            compressed_size: meta.compressed_size,
            method: meta.method().unwrap(),
        };
        (loc, node.meta)
    }

    fn meta(&self, name: &str) -> &zipfs::index::EntryMeta {
        let ino = self.index.lookup(ROOT_INO, name.as_bytes()).unwrap();
        self.index.meta(self.index.node(ino).unwrap()).unwrap()
    }

    fn pool(&self, name: &str, config: &Config, per_file: usize) -> EntryPool {
        let (loc, _) = self.locate(name);
        EntryPool::new(
            DecoderBudget::new(config),
            self.archive.reader(),
            loc,
            per_file,
        )
    }
}

fn parts(method: Method) -> CoreParts {
    CoreParts {
        codec: zipfs::codec::Codec::new(method),
        buf: vec![0u8; 64 * 1024].into_boxed_slice(),
        skip: vec![0u8; SKIP_BUFFER].into_boxed_slice(),
    }
}

/// Reads a whole entry through one decoder in fixed sized steps.
fn read_all(
    decoder: &mut PositionedDecoder,
    meta: &zipfs::index::EntryMeta,
    step: usize,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chunk = vec![0u8; step];
    while (out.len() as u64) < meta.uncompressed_size {
        let n = decoder.read_at(out.len() as u64, &mut chunk, meta).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
    }
    out
}

fn methods() -> Vec<(&'static str, CompressionMethod)> {
    #[cfg(feature = "zstd")]
    {
        vec![
            ("stored", STORE),
            ("deflated", DEFLATE),
            ("deflated64", DEFLATE64),
            ("zstd", ZSTD),
        ]
    }
    #[cfg(not(feature = "zstd"))]
    {
        vec![
            ("stored", STORE),
            ("deflated", DEFLATE),
            ("deflated64", DEFLATE64),
        ]
    }
}

/// A codec can consume all of its input before it delivers all of its output.
/// Thus an empty input must not end the read.
#[test]
fn a_read_that_stops_inside_the_last_match_does_not_lose_the_tail() {
    // One fixed block: the literal 0xff, a match of 258 at distance 1, and the
    // end of the block. The last byte holds the last bit of the distance and
    // all of the end code, so the decoder takes that byte before the match.
    const STREAM: [u8; 4] = [0xfb, 0x3f, 0x0a, 0x00];
    let body = [0xffu8; 259];
    let temp = TempArchive::new(
        "last-match",
        &archive_with_stream("f", DEFLATE, &STREAM, &body),
    );
    let archive = Archive::open(&temp.path).unwrap();
    let (index, _) = zipfs::index::build(&archive, Symlinks::default()).unwrap();
    let ino = index.lookup(ROOT_INO, b"f").unwrap();
    let meta = index.meta(index.node(ino).unwrap()).unwrap();
    let loc = EntryLocation {
        data_start: archive.data_start(meta).unwrap(),
        compressed_size: meta.compressed_size,
        method: meta.method().unwrap(),
    };

    for tail in 1..body.len() {
        let mut decoder = PositionedDecoder::new(archive.reader(), loc, parts(loc.method), true);
        let mut got = [0u8; 259];
        let (head, rest) = got.split_at_mut(body.len() - tail);
        assert_eq!(decoder.read_at(0, head, meta).unwrap(), head.len());
        let offset = head.len() as u64;
        assert_eq!(decoder.read_at(offset, rest, meta).unwrap(), tail);
        assert_eq!(got, body);
        assert_eq!(meta.verify_state(), VERIFY_OK);
    }
}

/// A stream that is valid only as Deflate64, and the bytes that it decodes to.
fn far_and_long_matches() -> (Vec<u8>, Vec<u8>) {
    // One final fixed block that holds two matches:
    // - Length code 285 and distance code 31 give length 65,538 at distance 65,536.
    // - Length code 257 and distance code 30 give length 3 at distance 32,769.
    // Deflate has no distance codes 30 and 31, and no extra bits for length code 285.
    const LAST_BLOCK: [u8; 10] = [0x1b, 0xfd, 0xff, 0xff, 0xff, 0x3f, 0xf0, 0x01, 0x00, 0x00];

    // A prefix larger than the 32 KiB that deflate can reach.
    let mut body = pseudo_random(70_000, 17);
    let mut stream = stored_blocks(&body);
    // Clear the final bit of the second stored block, because another block follows.
    stream[5 + usize::from(u16::MAX)] = 0;
    stream.extend_from_slice(&LAST_BLOCK);
    copy_match(&mut body, 65_538, 65_536);
    copy_match(&mut body, 3, 32_769);
    (stream, body)
}

fn copy_match(body: &mut Vec<u8>, length: usize, distance: usize) {
    for _ in 0..length {
        body.push(body[body.len() - distance]);
    }
}

fn single_entry(temp: &TempArchive) -> (Archive, Index) {
    let archive = Archive::open(&temp.path).unwrap();
    let (index, _) = zipfs::index::build(&archive, Symlinks::default()).unwrap();
    (archive, index)
}

#[test]
fn deflate64_reads_matches_that_deflate_cannot_express() {
    let (stream, body) = far_and_long_matches();
    let temp = TempArchive::new(
        "far-long",
        &archive_with_stream("f", DEFLATE64, &stream, &body),
    );
    let (archive, index) = single_entry(&temp);
    let ino = index.lookup(ROOT_INO, b"f").unwrap();
    let meta = index.meta(index.node(ino).unwrap()).unwrap();
    let loc = EntryLocation {
        data_start: archive.data_start(meta).unwrap(),
        compressed_size: meta.compressed_size,
        method: meta.method().unwrap(),
    };
    assert_eq!(loc.method, Method::Deflate64);

    // The output sizes put the end of a read inside the long match, and the
    // second pass finds any window or table that a rewind leaves behind.
    let mut decoder = PositionedDecoder::new(archive.reader(), loc, parts(loc.method), true);
    for step in [1, 4096, 131_072, body.len() + 1] {
        assert!(read_all(&mut decoder, meta, step) == body, "step {step}");
        assert_eq!(meta.verify_state(), VERIFY_OK);
        decoder.reinit(loc);
    }
}

#[test]
fn deflate64_reports_an_invalid_stream() {
    // The first block has the reserved block type 3.
    let temp = TempArchive::new(
        "reserved",
        &archive_with_stream("f", DEFLATE64, &[0x07], b"body"),
    );
    let (archive, index) = single_entry(&temp);
    let ino = index.lookup(ROOT_INO, b"f").unwrap();
    let meta = index.meta(index.node(ino).unwrap()).unwrap();
    let loc = EntryLocation {
        data_start: archive.data_start(meta).unwrap(),
        compressed_size: meta.compressed_size,
        method: meta.method().unwrap(),
    };
    let mut decoder = PositionedDecoder::new(archive.reader(), loc, parts(loc.method), true);
    let mut got = [0u8; 4];
    let error = decoder.read_at(0, &mut got, meta).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn every_method_reads_back_what_was_written() {
    let body = compressible(300_000);
    let entries: Vec<_> = methods()
        .into_iter()
        .map(|(name, method)| Entry::file(leak(name), body.clone(), method))
        .collect();
    let fixture = Fixture::new("methods", &entries);
    let config = Config::default();

    for (name, _) in methods() {
        let (loc, _) = fixture.locate(name);
        let mut decoder = PositionedDecoder::new(
            fixture.archive.reader(),
            loc,
            parts(loc.method),
            config.verify,
        );
        let got = read_all(&mut decoder, fixture.meta(name), 8192);
        assert_eq!(got, body, "{name} did not round trip");
        assert_eq!(fixture.meta(name).verify_state(), VERIFY_OK);
    }
}

#[test]
fn a_decoder_that_is_rewound_produces_the_same_bytes() {
    let body = pseudo_random(200_000, 7);
    let fixture = Fixture::new("rewind", &[Entry::file("data", body.clone(), DEFLATE)]);
    let (loc, _) = fixture.locate("data");
    let meta = fixture.meta("data");
    let config = Config::default();

    let mut decoder = PositionedDecoder::new(
        fixture.archive.reader(),
        loc,
        parts(loc.method),
        config.verify,
    );
    let first = read_all(&mut decoder, meta, 4096);
    decoder.reinit(loc);
    let second = read_all(&mut decoder, meta, 4096);

    assert_eq!(first, body);
    assert_eq!(second, body);
}

#[test]
fn a_read_in_the_middle_of_an_entry_returns_the_right_bytes() {
    let body = pseudo_random(500_000, 11);
    let entries: Vec<_> = methods()
        .into_iter()
        .map(|(name, method)| Entry::file(leak(name), body.clone(), method))
        .collect();
    let fixture = Fixture::new("offsets", &entries);
    let config = Config::default();

    for (name, _) in methods() {
        let pool = fixture.pool(name, &config, 4);
        let meta = fixture.meta(name);
        for offset in [0u64, 1, 4095, 100_000, 499_999] {
            let want = (body.len() as u64 - offset).min(9_000) as usize;
            let mut got = vec![0u8; want];
            let mut decoder = pool.checkout(offset);
            let n = decoder.read_at(offset, &mut got, meta).unwrap();
            assert_eq!(n, want, "{name} at {offset}");
            let start = usize::try_from(offset).expect("test offset fits in usize");
            assert_eq!(got[..n], body[start..start + n], "{name} at {offset}");
        }
    }
}

#[test]
fn reading_backwards_rewinds_a_decoder_instead_of_failing() {
    let body = pseudo_random(300_000, 13);
    let fixture = Fixture::new("backwards", &[Entry::file("data", body.clone(), DEFLATE)]);
    let config = Config::default();
    let pool = fixture.pool("data", &config, 1);
    let meta = fixture.meta("data");

    let mut buf = vec![0u8; 1024];
    for offset in [250_000u64, 10_000, 200_000, 0, 299_000] {
        let n = {
            let mut decoder = pool.checkout(offset);
            decoder.read_at(offset, &mut buf, meta).unwrap()
        };
        let start = usize::try_from(offset).expect("test offset fits in usize");
        assert_eq!(buf[..n], body[start..start + n], "at offset {offset}");
    }
}

#[test]
fn a_pool_keeps_no_more_decoders_than_it_is_allowed() {
    let body = compressible(100_000);
    let fixture = Fixture::new("cap", &[Entry::file("data", body, DEFLATE)]);
    let config = Config {
        decoders_per_file: 2,
        ..Config::default()
    };
    let pool = fixture.pool("data", &config, config.decoders_per_file);
    let meta = fixture.meta("data");

    // Hold four decoders at once, then let them all go.
    {
        let mut held: Vec<_> = (0..4).map(|i| pool.checkout(i * 1000)).collect();
        let mut buf = [0u8; 16];
        for (i, decoder) in held.iter_mut().enumerate() {
            decoder.read_at(i as u64 * 1000, &mut buf, meta).unwrap();
        }
    }
    assert_eq!(pool.idle_len(), 2, "the extra decoders were not retained");
}

#[test]
fn the_mount_wide_budget_caps_retained_decoders() {
    let body = compressible(50_000);
    let fixture = Fixture::new("budget", &[Entry::file("data", body, DEFLATE)]);
    let mut config = Config {
        decoders_per_file: 8,
        max_retained_decoders: 3,
        ..Config::default()
    };
    config.clamp();

    let budget = DecoderBudget::new(&config);
    let (loc, _) = fixture.locate("data");
    let pool = EntryPool::new(
        Arc::clone(&budget),
        fixture.archive.reader(),
        loc,
        config.decoders_per_file,
    );
    let meta = fixture.meta("data");
    {
        let mut held: Vec<_> = (0..6).map(|i| pool.checkout(i * 100)).collect();
        let mut buf = [0u8; 8];
        for (i, decoder) in held.iter_mut().enumerate() {
            decoder.read_at(i as u64 * 100, &mut buf, meta).unwrap();
        }
    }
    assert_eq!(budget.retained(), 3);
    assert_eq!(pool.idle_len(), 3);
    drop(pool);
    assert_eq!(budget.retained(), 0, "closing the file gave the slots back");
}

#[test]
fn a_corrupt_entry_is_reported_when_it_is_read_to_the_end() {
    let body = compressible(64_000);
    let mut bytes = archive(&[Entry::file("data", body.clone(), DEFLATE)]);
    // Flip a bit inside the compressed data, past the local header.
    let target = bytes.len() / 2;
    bytes[target] ^= 0x40;

    let temp = TempArchive::new("corrupt", &bytes);
    let archive = Archive::open(&temp.path).unwrap();
    let (index, _) = zipfs::index::build(&archive, Symlinks::default()).unwrap();
    let ino = index.lookup(ROOT_INO, b"data").unwrap();
    let meta = index.meta(index.node(ino).unwrap()).unwrap();
    let loc = EntryLocation {
        data_start: archive.data_start(meta).unwrap(),
        compressed_size: meta.compressed_size,
        method: meta.method().unwrap(),
    };

    let mut decoder = PositionedDecoder::new(archive.reader(), loc, parts(loc.method), true);
    let mut out = vec![0u8; body.len()];
    let result = decoder.read_at(0, &mut out, meta);
    assert!(result.is_err(), "corruption went unnoticed");
    assert_eq!(meta.verify_state(), VERIFY_BAD);

    // Without verification the same read succeeds, whatever it returns.
    let mut decoder = PositionedDecoder::new(archive.reader(), loc, parts(loc.method), false);
    let mut out = vec![0u8; body.len()];
    decoder.read_at(0, &mut out, meta).unwrap();
}

#[test]
fn a_final_decompressor_error_marks_verification_bad() {
    let body = vec![b'A'];
    let fixture = Fixture::new(
        "final-validation",
        &[Entry::file("data", body.clone(), DEFLATE)],
    );
    let meta = fixture.meta("data");
    let compressed = [0x8a, 0x00, 0x07];
    let temp = TempArchive::new("final-validation-bytes", &compressed);
    let reader = Arc::new(zipfs::GuardedFile::open(&temp.path).unwrap());
    let loc = EntryLocation {
        data_start: 0,
        compressed_size: compressed.len() as u64,
        method: Method::Deflate,
    };
    let mut decoder = PositionedDecoder::new(reader, loc, parts(Method::Deflate), true);
    let mut out = vec![0u8; body.len()];
    let result = decoder.read_at(0, &mut out, meta);

    assert!(result.is_err(), "the final decompressor error was ignored");
    assert_eq!(meta.verify_state(), VERIFY_BAD);
}

/// The central directory promises `body`, but the stream gives only part of it.
/// A stream can be cut, or it can end correctly but too soon.
#[test]
fn a_stream_shorter_than_its_entry_fails_the_read() {
    let body = pseudo_random(200_000, 29);
    let whole = deflate(&body);
    let stored = stored_blocks(&body);
    let streams = [
        ("cut", DEFLATE, whole[..whole.len() / 2].to_vec()),
        ("early", DEFLATE, deflate(&body[..100_000])),
        ("cut64", DEFLATE64, stored[..stored.len() / 2].to_vec()),
        ("early64", DEFLATE64, stored_blocks(&body[..100_000])),
    ];
    for (name, method, stream) in streams {
        for verify in [true, false] {
            let temp = TempArchive::new(name, &archive_with_stream("f", method, &stream, &body));
            let archive = Archive::open(&temp.path).unwrap();
            let (index, _) = zipfs::index::build(&archive, Symlinks::default()).unwrap();
            let ino = index.lookup(ROOT_INO, b"f").unwrap();
            let meta = index.meta(index.node(ino).unwrap()).unwrap();
            let loc = EntryLocation {
                data_start: archive.data_start(meta).unwrap(),
                compressed_size: meta.compressed_size,
                method: meta.method().unwrap(),
            };
            let mut decoder =
                PositionedDecoder::new(archive.reader(), loc, parts(loc.method), verify);

            let mut chunk = [0u8; 4096];
            let mut offset = 0u64;
            let error = loop {
                match decoder.read_at(offset, &mut chunk, meta) {
                    Ok(n) => {
                        assert_eq!(n, chunk.len(), "{name} read short at {offset}");
                        offset += n as u64;
                    }
                    Err(error) => break error,
                }
            };
            assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof, "{name}");
            assert_eq!(meta.verify_state(), VERIFY_BAD, "{name}");

            // The same decoder must not change its answer on the next read.
            let again = decoder
                .read_at(decoder.pos(), &mut chunk, meta)
                .unwrap_err();
            assert_eq!(again.kind(), std::io::ErrorKind::UnexpectedEof, "{name}");
        }
    }
}

#[test]
fn many_threads_read_one_entry_at_once() {
    let body = pseudo_random(1_000_000, 17);
    let fixture = Fixture::new("threads", &[Entry::file("data", body.clone(), DEFLATE)]);
    let config = Config::default();
    let pool = Arc::new(fixture.pool("data", &config, 4));
    let meta = fixture.meta("data");
    let body = Arc::new(body);

    std::thread::scope(|scope| {
        for thread in 0..8u64 {
            let pool = Arc::clone(&pool);
            let body = Arc::clone(&body);
            scope.spawn(move || {
                let mut buf = vec![0u8; 7_777];
                for step in 0..25u64 {
                    // A spread of offsets, so decoders are reused and rewound.
                    let offset = (thread * 37 + step * 40_009) % 990_000;
                    let n = {
                        let mut decoder = pool.checkout(offset);
                        decoder.read_at(offset, &mut buf, meta).unwrap()
                    };
                    let start = usize::try_from(offset).expect("test offset fits in usize");
                    assert_eq!(buf[..n], body[start..start + n], "at offset {offset}");
                }
            });
        }
    });
}

/// Turns a borrowed name into one the fixture builder can hold.
fn leak(name: &str) -> &'static str {
    Box::leak(name.to_string().into_boxed_str())
}
