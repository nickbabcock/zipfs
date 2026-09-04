//! Decoders, the pool that positions them, and the checksum they carry.

mod common;

use common::{Entry, TempArchive, archive, compressible, pseudo_random};
use rawzip::CompressionMethod;
use std::sync::Arc;
use zipfs::config::SKIP_BUFFER;
use zipfs::decode::{CoreParts, EntryLocation, PositionedDecoder};
use zipfs::index::{Index, Method, ROOT_INO, VERIFY_BAD, VERIFY_OK};
use zipfs::pool::{DecoderBudget, EntryPool};
use zipfs::{Archive, Config};

const STORE: CompressionMethod = CompressionMethod::STORE;
const DEFLATE: CompressionMethod = CompressionMethod::DEFLATE;
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
        let (index, _) = zipfs::index::build(&archive).unwrap();
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
        vec![("stored", STORE), ("deflated", DEFLATE), ("zstd", ZSTD)]
    }
    #[cfg(not(feature = "zstd"))]
    {
        vec![("stored", STORE), ("deflated", DEFLATE)]
    }
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
    let (index, _) = zipfs::index::build(&archive).unwrap();
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
    let reader = Arc::new(rawzip::FileReader::from(
        std::fs::File::open(&temp.path).unwrap(),
    ));
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
