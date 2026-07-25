// SPDX-License-Identifier: LGPL-2.1-or-later
//! PSF2 virtual filesystem integration and malformed-input tests.

use std::io::Write;

use flate2::{Compression, write::ZlibEncoder};
use upse_psf::{
    DependencyLimits, LoadPlan, MemoryResolver, ParseLimits, Psf2LoadPlan, PsfBuilder, PsfVersion,
    load_plan,
};
use upse_psf2_vfs::{NodeKind, Psf2Vfs, VfsErrorKind, VfsLimits};

enum Entry<'a> {
    File {
        name: &'a str,
        data: &'a [u8],
        block_size: usize,
    },
    Empty(&'a str),
    Directory(&'a str, Vec<Entry<'a>>),
}

fn filesystem(entries: &[Entry<'_>]) -> Vec<u8> {
    let mut output = Vec::new();
    assert_eq!(append_directory(&mut output, entries), 0);
    output
}

fn append_directory(output: &mut Vec<u8>, entries: &[Entry<'_>]) -> usize {
    let offset = output.len();
    output.resize(offset + 4 + entries.len() * 48, 0);
    put_u32(output, offset, entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let entry_offset = offset + 4 + index * 48;
        let name = match entry {
            Entry::File { name, .. } | Entry::Empty(name) | Entry::Directory(name, _) => name,
        };
        assert!(!name.is_empty() && name.len() <= 36);
        output[entry_offset..entry_offset + name.len()].copy_from_slice(name.as_bytes());
        match entry {
            Entry::Empty(_) => {}
            Entry::Directory(_, children) => {
                let child_offset = append_directory(output, children);
                put_u32(output, entry_offset + 36, child_offset);
            }
            Entry::File {
                data, block_size, ..
            } => {
                assert!(!data.is_empty() && *block_size != 0);
                let blocks: Vec<_> = data
                    .chunks(*block_size)
                    .map(|block| {
                        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
                        encoder.write_all(block).unwrap();
                        encoder.finish().unwrap()
                    })
                    .collect();
                let data_offset = output.len();
                output.resize(data_offset + blocks.len() * 4, 0);
                for (block_index, block) in blocks.iter().enumerate() {
                    put_u32(output, data_offset + block_index * 4, block.len());
                }
                for block in blocks {
                    output.extend_from_slice(&block);
                }
                put_u32(output, entry_offset + 36, data_offset);
                put_u32(output, entry_offset + 40, data.len());
                put_u32(output, entry_offset + 44, *block_size);
            }
        }
    }
    offset
}

fn put_u32(output: &mut [u8], offset: usize, value: usize) {
    output[offset..offset + 4].copy_from_slice(&u32::try_from(value).unwrap().to_le_bytes());
}

fn psf2(reserved: Vec<u8>, libraries: &[(&str, &str)]) -> Vec<u8> {
    let mut builder = PsfBuilder::new(PsfVersion::Psf2).reserved(reserved);
    for (key, value) in libraries {
        builder = builder.tag(*key, *value);
    }
    builder.build()
}

fn plan(reserved: Vec<u8>) -> Psf2LoadPlan {
    let root = psf2(reserved, &[]);
    let LoadPlan::Psf2(plan) = load_plan(
        "root.psf2",
        &root,
        &mut MemoryResolver::new(),
        ParseLimits::default(),
        DependencyLimits::default(),
    )
    .unwrap() else {
        panic!("wrong plan")
    };
    plan
}

fn parse(reserved: Vec<u8>) -> Result<Psf2Vfs, VfsErrorKind> {
    Psf2Vfs::from_load_plan(&plan(reserved), VfsLimits::default()).map_err(|error| error.kind)
}

#[test]
fn generated_tree_has_exact_normalized_map_and_bounded_reads() {
    let exact_name = "abcdefghijklmnopqrstuvwxyz0123456789";
    assert_eq!(exact_name.len(), 36);
    let data: Vec<_> = (0_u8..23).collect();
    let image = filesystem(&[
        Entry::Directory(
            "Music",
            vec![
                Entry::File {
                    name: exact_name,
                    data: &data,
                    block_size: 7,
                },
                Entry::Empty("Silence"),
            ],
        ),
        Entry::Directory("EmptyDir", vec![]),
    ]);
    let vfs = parse(image).unwrap();

    assert_eq!(vfs.len(), 2);
    assert_eq!(
        vfs.files().map(|(path, _)| path).collect::<Vec<_>>(),
        [
            "music/abcdefghijklmnopqrstuvwxyz0123456789",
            "music/silence"
        ]
    );
    assert_eq!(
        vfs.node_kind("/MUSIC\\ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789")
            .unwrap(),
        Some(NodeKind::File)
    );
    assert_eq!(
        vfs.node_kind("emptydir").unwrap(),
        Some(NodeKind::Directory)
    );
    assert_eq!(vfs.file("music/silence").unwrap(), []);
    let mut output = [0_u8; 8];
    assert_eq!(
        vfs.read("music/abcdefghijklmnopqrstuvwxyz0123456789", 5, &mut output)
            .unwrap(),
        8
    );
    assert_eq!(output, data[5..13]);
    assert_eq!(
        vfs.read(
            "music/abcdefghijklmnopqrstuvwxyz0123456789",
            100,
            &mut output
        )
        .unwrap(),
        0
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn recursive_libraries_and_case_collisions_have_deterministic_overlays() {
    let base = psf2(
        filesystem(&[
            Entry::Directory(
                "Data",
                vec![
                    Entry::File {
                        name: "A.BIN",
                        data: b"base-a",
                        block_size: 16,
                    },
                    Entry::File {
                        name: "base.bin",
                        data: b"base-only",
                        block_size: 16,
                    },
                ],
            ),
            Entry::Directory(
                "Gone",
                vec![Entry::File {
                    name: "child",
                    data: b"removed",
                    block_size: 16,
                }],
            ),
            Entry::File {
                name: "becomes-dir",
                data: b"old-file",
                block_size: 16,
            },
        ]),
        &[("_lib", "common.psf2lib")],
    );
    let common = psf2(
        filesystem(&[Entry::File {
            name: "common.bin",
            data: b"common",
            block_size: 16,
        }]),
        &[],
    );
    let root = psf2(
        filesystem(&[
            Entry::Directory(
                "data",
                vec![
                    Entry::File {
                        name: "a.bin",
                        data: b"root-a",
                        block_size: 16,
                    },
                    Entry::File {
                        name: "new.bin",
                        data: b"first",
                        block_size: 16,
                    },
                    Entry::File {
                        name: "NEW.BIN",
                        data: b"last",
                        block_size: 16,
                    },
                ],
            ),
            Entry::File {
                name: "GONE",
                data: b"replacement",
                block_size: 16,
            },
            Entry::Directory(
                "BECOMES-DIR",
                vec![Entry::File {
                    name: "child",
                    data: b"new-child",
                    block_size: 16,
                }],
            ),
        ]),
        &[("_lib", "base.psf2lib")],
    );
    let mut resolver = MemoryResolver::new();
    resolver.insert("set/base.psf2lib", base).unwrap();
    resolver.insert("set/common.psf2lib", common).unwrap();
    let LoadPlan::Psf2(plan) = load_plan(
        "set/root.minipsf2",
        &root,
        &mut resolver,
        ParseLimits::default(),
        DependencyLimits::default(),
    )
    .unwrap() else {
        panic!("wrong plan")
    };
    let vfs = Psf2Vfs::from_load_plan(&plan, VfsLimits::default()).unwrap();

    assert_eq!(vfs.file("common.bin").unwrap(), b"common");
    assert_eq!(vfs.file("data/a.bin").unwrap(), b"root-a");
    assert_eq!(vfs.file("data/base.bin").unwrap(), b"base-only");
    assert_eq!(vfs.file("data/new.bin").unwrap(), b"last");
    assert_eq!(vfs.file("gone").unwrap(), b"replacement");
    assert_eq!(vfs.node_kind("gone/child").unwrap(), None);
    assert_eq!(
        vfs.node_kind("becomes-dir").unwrap(),
        Some(NodeKind::Directory)
    );
    assert_eq!(vfs.file("becomes-dir/child").unwrap(), b"new-child");
}

#[test]
fn malformed_names_offsets_tables_blocks_and_bombs_fail_during_construction() {
    assert_eq!(parse(vec![]).unwrap_err(), VfsErrorKind::Truncated);

    let mut invalid_name = filesystem(&[Entry::Empty("valid")]);
    invalid_name[4] = b'/';
    assert_eq!(parse(invalid_name).unwrap_err(), VfsErrorKind::InvalidName);

    let mut bad_padding = filesystem(&[Entry::Empty("a")]);
    bad_padding[6] = b'x';
    assert_eq!(parse(bad_padding).unwrap_err(), VfsErrorKind::InvalidName);

    let mut backward = filesystem(&[Entry::Directory("dir", vec![])]);
    put_u32(&mut backward, 40, 4);
    assert_eq!(parse(backward).unwrap_err(), VfsErrorKind::OffsetOrder);

    let mut bad_tuple = filesystem(&[Entry::Empty("empty")]);
    put_u32(&mut bad_tuple, 48, 1);
    assert_eq!(parse(bad_tuple).unwrap_err(), VfsErrorKind::InvalidEntry);

    let mut short = filesystem(&[Entry::File {
        name: "file",
        data: b"some file bytes",
        block_size: 4,
    }]);
    short.pop();
    assert_eq!(parse(short).unwrap_err(), VfsErrorKind::Truncated);

    let mut bomb = filesystem(&[Entry::File {
        name: "bomb",
        data: &[0; 128],
        block_size: 128,
    }]);
    put_u32(&mut bomb, 44, 1);
    put_u32(&mut bomb, 48, 1);
    assert!(matches!(
        parse(bomb),
        Err(VfsErrorKind::BlockSizeMismatch { expected: 1, .. })
    ));
}

#[test]
fn every_construction_resource_limit_is_enforced() {
    let nested = filesystem(&[Entry::Directory(
        "a",
        vec![Entry::Directory("b", vec![Entry::Empty("c")])],
    )]);
    let error = Psf2Vfs::from_load_plan(
        &plan(nested),
        VfsLimits {
            max_depth: 1,
            ..VfsLimits::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.kind, VfsErrorKind::LimitExceeded("directory depth"));

    let two_entries = filesystem(&[Entry::Empty("a"), Entry::Empty("b")]);
    let error = Psf2Vfs::from_load_plan(
        &plan(two_entries),
        VfsLimits {
            max_entries: 1,
            ..VfsLimits::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.kind, VfsErrorKind::LimitExceeded("entry count"));

    let long_path = filesystem(&[Entry::Directory(
        "1234567890",
        vec![Entry::Empty("1234567890")],
    )]);
    let error = Psf2Vfs::from_load_plan(
        &plan(long_path),
        VfsLimits {
            max_path_bytes: 16,
            ..VfsLimits::default()
        },
    )
    .unwrap_err();
    assert_eq!(error.kind, VfsErrorKind::LimitExceeded("path bytes"));

    let file = filesystem(&[Entry::File {
        name: "file",
        data: &[1; 17],
        block_size: 8,
    }]);
    for (limits, expected) in [
        (
            VfsLimits {
                max_file_bytes: 16,
                ..VfsLimits::default()
            },
            "file bytes",
        ),
        (
            VfsLimits {
                max_block_bytes: 7,
                ..VfsLimits::default()
            },
            "block bytes",
        ),
        (
            VfsLimits {
                max_blocks: 2,
                ..VfsLimits::default()
            },
            "block count",
        ),
        (
            VfsLimits {
                max_aggregate_bytes: 16,
                ..VfsLimits::default()
            },
            "aggregate file bytes",
        ),
    ] {
        let error = Psf2Vfs::from_load_plan(&plan(file.clone()), limits).unwrap_err();
        assert_eq!(error.kind, VfsErrorKind::LimitExceeded(expected));
    }
}

#[test]
fn invalid_queries_are_contained() {
    let vfs = parse(filesystem(&[Entry::Empty("file")])).unwrap();
    assert_eq!(
        vfs.file("missing").unwrap_err().kind,
        VfsErrorKind::NotFound
    );
    assert_eq!(vfs.file("/").unwrap_err().kind, VfsErrorKind::IsDirectory);
    for path in ["../file", "./file", "device:file", "caf\u{e9}"] {
        assert_eq!(
            vfs.node_kind(path).unwrap_err().kind,
            VfsErrorKind::InvalidPath
        );
    }
}

#[test]
fn bounded_arbitrary_reserved_sections_never_panic() {
    let limits = VfsLimits {
        max_depth: 4,
        max_entries: 64,
        max_path_bytes: 64,
        max_blocks: 64,
        max_file_bytes: 4096,
        max_block_bytes: 1024,
        max_aggregate_bytes: 8192,
    };
    let mut state = 0x6a09_e667_f3bc_c909_u64;
    for length in 0..2048 {
        let mut reserved = vec![0_u8; length];
        for byte in &mut reserved {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state.to_le_bytes()[0];
        }
        let _ = Psf2Vfs::from_load_plan(&plan(reserved), limits);
    }
}
