use rustdm::writer::PartFile;
use std::fs;
use std::io::{ErrorKind, Read};
use tempfile::tempdir;

#[test]
fn create_preallocates_to_total() {
    let dir = tempdir().unwrap();
    let part_path = dir.path().join("test.bin.part");
    let part = PartFile::create(&part_path, Some(1024 * 1024)).unwrap();
    let meta = part.file.metadata().unwrap();
    assert_eq!(meta.len(), 1024 * 1024);
}

#[test]
fn writes_out_of_order_read_back_correctly() {
    let dir = tempdir().unwrap();
    let part_path = dir.path().join("test.bin.part");
    let part = PartFile::create(&part_path, Some(1000)).unwrap();

    let half1 = vec![1u8; 500];
    let half2 = vec![2u8; 500];

    part.write_at(500, &half2).unwrap();
    part.write_at(0, &half1).unwrap();
    part.sync().unwrap();

    let mut read_back = Vec::new();
    let mut f = fs::File::open(&part_path).unwrap();
    f.read_to_end(&mut read_back).unwrap();

    assert_eq!(&read_back[..500], &half1[..]);
    assert_eq!(&read_back[500..], &half2[..]);
}

#[test]
fn second_handle_sees_writes() {
    let dir = tempdir().unwrap();
    let part_path = dir.path().join("test.bin.part");
    let part = PartFile::create(&part_path, Some(100)).unwrap();

    let handle2 = PartFile::open(&part_path).unwrap();
    part.write_at(10, b"hello").unwrap();
    part.sync().unwrap();

    let mut buf = vec![0u8; 5];
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        handle2.file.read_exact_at(&mut buf, 10).unwrap();
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        handle2.file.seek_read(&mut buf, 10).unwrap();
    }
    assert_eq!(&buf, b"hello");
}

#[test]
fn finish_renames_and_removes_part() {
    let dir = tempdir().unwrap();
    let part_path = dir.path().join("file.bin.part");
    let final_path = dir.path().join("file.bin");

    let part = PartFile::create(&part_path, Some(10)).unwrap();
    part.write_at(0, b"0123456789").unwrap();
    part.finish(&final_path).unwrap();

    assert!(!part_path.exists());
    assert!(final_path.exists());
    assert_eq!(fs::read(&final_path).unwrap(), b"0123456789");
}

#[test]
fn finish_refuses_to_overwrite_existing_target() {
    let dir = tempdir().unwrap();
    let part_path = dir.path().join("file.bin.part");
    let final_path = dir.path().join("file.bin");

    fs::write(&final_path, b"already here").unwrap();

    let part = PartFile::create(&part_path, Some(10)).unwrap();
    part.write_at(0, b"new bytes!").unwrap();

    let err = part.finish(&final_path).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&final_path).unwrap(), b"already here");
}
