use super::{decode_mounts_field, mounts_list_staticfs_at};

#[test]
fn decode_mounts_field_unescapes_kernel_octal_sequences() {
    assert_eq!(
        decode_mounts_field("/home/u/Music/static\\040fs"),
        "/home/u/Music/static fs"
    );
    assert_eq!(decode_mounts_field("a\\011b\\012c\\134d"), "a\tb\nc\\d");
    // Fields without escapes pass through unchanged.
    assert_eq!(
        decode_mounts_field("/home/u/Music/staticfs"),
        "/home/u/Music/staticfs"
    );
    // Truncated or non-octal sequences are left untouched.
    assert_eq!(decode_mounts_field("trailing\\04"), "trailing\\04");
    assert_eq!(decode_mounts_field("not\\089octal"), "not\\089octal");
}

#[test]
fn live_mount_detection_decodes_escaped_targets() {
    let mounts = "staticfs /home/u/Music/static\\040fs fuse.staticfs rw,nosuid,nodev 0 0\n\
                  staticfs /home/u/mnt\\134dir fuse.staticfs rw 0 0\n\
                  staticfs /home/u/Music/staticfs fuse.staticfs rw 0 0\n\
                  tmpfs /tmp tmpfs rw 0 0\n";
    // The double-mount guard must match a mountpoint containing a space,
    // which /proc/self/mounts renders as \040.
    assert!(mounts_list_staticfs_at(mounts, "/home/u/Music/static fs"));
    assert!(mounts_list_staticfs_at(mounts, "/home/u/mnt\\dir"));
    assert!(mounts_list_staticfs_at(mounts, "/home/u/Music/staticfs"));
    assert!(!mounts_list_staticfs_at(mounts, "/home/u/Music/static"));
    // Non-staticfs sources never count as live staticfs mounts.
    assert!(!mounts_list_staticfs_at(mounts, "/tmp"));
}
