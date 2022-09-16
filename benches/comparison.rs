use async_filelike::Adaptor;
use async_io::block_on;
use blocking::Unblock;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use futures_lite::prelude::*;
use std::fs;

fn blocking_io(c: &mut Criterion) {
    let mut group = c.benchmark_group("blocking_io");

    group.bench_function("Read", |b| {
        let mut file = Unblock::new(fs::File::create("foo.txt").unwrap());
        let mut buf = vec![0u8; 10_000];
        block_on(file.write_all(&buf)).unwrap();

        b.iter(|| {
            block_on(async {
                black_box(file.read_exact(&mut buf).await.ok());
            })
        });

        fs::remove_file("foo.txt").unwrap();
    });

    group.bench_function("Write", |b| {
        let mut file = Unblock::new(fs::File::create("foo.txt").unwrap());
        let buf = vec![0u8; 10_000];

        b.iter(|| {
            block_on(async {
                black_box(file.write_all(&buf).await.ok());
            })
        });

        fs::remove_file("foo.txt").unwrap();
    });
}

fn filelike_io(c: &mut Criterion) {
    let mut group = c.benchmark_group("filelike_io");

    group.bench_function("Read", |b| {
        let mut file = Adaptor::new(fs::File::create("foo.txt").unwrap()).unwrap();
        let buf = vec![0u8; 10_000];
        block_on(file.write_all(&buf)).unwrap();
        let mut buf = Some(buf);

        b.iter(|| {
            block_on(async {
                let mut buf2 = buf.take().unwrap();
                black_box(file.read_exact(&mut buf2).await.ok());
                buf = Some(buf2);
            })
        });

        fs::remove_file("foo.txt").unwrap();
    });

    group.bench_function("Write", |b| {
        let mut file = Adaptor::new(fs::File::create("foo.txt").unwrap()).unwrap();
        let mut buf = Some(vec![0u8; 10_000]);

        b.iter(|| {
            block_on(async {
                let buf2 = buf.take().unwrap();
                black_box(file.write_all(&buf2).await.ok());
                buf = Some(buf2);
            })
        });

        fs::remove_file("foo.txt").unwrap();
    });
}

criterion_group! {
    comparison,
    blocking_io,
    filelike_io
}

criterion_main!(comparison);
