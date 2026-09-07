use super::test_backend::TestCartridges;
use super::*;
use crate::staticfs::{format::FsMode, tree::Node};

fn context() -> (Ctx, TestCartridges) {
    let store = TestCartridges::default();
    let ctx = Ctx::with_test_cartridges("/nonexistent/static-root".into(), store.clone());
    (ctx, store)
}

#[test]
fn daemon_cartridges_appear_live_and_delete_frees_them() {
    let (ctx, store) = context();
    assert!(names(&ctx).unwrap().is_empty());
    store.insert("b.png", b"png and embedded audio");
    assert_eq!(names(&ctx).unwrap(), vec!["b.png"]);
    assert!(matches!(
        Node::CartridgesDir.lookup_child(&ctx, "a.png"),
        Err(FsError::NotFound)
    ));
    store.insert("a.png", b"complete");
    assert_eq!(names(&ctx).unwrap(), vec!["a.png", "b.png"]);
    let node = Node::CartridgesDir.lookup_child(&ctx, "a.png").unwrap();
    assert_eq!(node.display_path(), "cartridges/a.png");
    assert_eq!(node.perm(), 0o444);
    assert_eq!(Node::CartridgesDir.perm(), 0o755);
    assert!(!node.writable());
    remove(&ctx, "a.png").unwrap();
    assert!(store.info("a.png").is_none());
    assert!(matches!(
        node.read_range(&ctx, 0, 4),
        Err(FsError::NotFound)
    ));
    assert!(matches!(remove(&ctx, "a.png"), Err(FsError::NotFound)));
    assert_eq!(names(&ctx).unwrap(), vec!["b.png"]);
}

#[test]
fn binary_reads_preserve_audio_metadata_and_ignore_text_format_modes() {
    let (ctx, store) = context();
    let bytes = b"\x89PNG\r\n\x1a\n\0\xff---AUDIO-START---\x80\0\x01---AUDIO-END------METADATA-START---{}---METADATA-END---";
    store.insert("song.png", bytes);
    let node = Node::Cartridge("song.png".into());
    assert_eq!(metadata(&ctx, "song.png").unwrap().size, bytes.len() as u64);
    for mode in [FsMode::Awk, FsMode::Jq] {
        ctx.set_fsmod(mode);
        let mut actual = Vec::new();
        while actual.len() < bytes.len() {
            actual.extend(node.read_range(&ctx, actual.len() as u64, 7).unwrap());
        }
        assert_eq!(actual, bytes);
    }
    assert!(
        node.read_range(&ctx, bytes.len() as u64, 20)
            .unwrap()
            .is_empty()
    );
    assert!(
        node.read_range(&ctx, u64::MAX, u32::MAX)
            .unwrap()
            .is_empty()
    );
    assert!(node.read_range(&ctx, 0, 0).unwrap().is_empty());
}

#[test]
fn cartridge_names_cannot_escape_or_address_hidden_payloads() {
    let (ctx, store) = context();
    store.insert("private.png", b"private");
    for name in [
        "../private.png",
        "/private.png",
        "nested/song.png",
        ".partial.png",
        "bad\0.png",
        "song.txt",
    ] {
        assert!(
            matches!(metadata(&ctx, name), Err(FsError::NotFound)),
            "{name:?}"
        );
        assert!(
            matches!(read_range(&ctx, name, 0, 32), Err(FsError::NotFound)),
            "{name:?}"
        );
        assert!(
            matches!(remove(&ctx, name), Err(FsError::NotFound)),
            "{name:?}"
        );
    }
    assert!(store.info("private.png").is_some());
}

#[test]
fn cartridge_reads_are_bounded_and_fetch_fresh_daemon_bytes() {
    let (ctx, store) = context();
    let bytes = vec![0x80; CHUNK_BYTES * 3 + 4];
    store.insert("large.png", &bytes);
    assert_eq!(
        metadata(&ctx, "large.png").unwrap().size,
        bytes.len() as u64
    );
    assert_eq!(
        read_range(&ctx, "large.png", 0, u32::MAX).unwrap().len(),
        CHUNK_BYTES
    );
    assert_eq!(
        read_range(&ctx, "large.png", bytes.len() as u64 - 4, 4096).unwrap(),
        [0x80; 4]
    );
    store.insert("large.png", b"changed at the daemon");
    assert_eq!(
        read_range(&ctx, "large.png", 0, u32::MAX).unwrap(),
        b"changed at the daemon"
    );
    assert_eq!(metadata(&ctx, "large.png").unwrap().size, 21);
}
