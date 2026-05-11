use std::path::PathBuf;

fn main() {
    let mountpoint = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: sqlite-fs /Volumes/minfs-poc");

    minfuse::mount(
        minfs::MinFs::new(),
        mountpoint,
        minfuse::MountOptions::default(),
    )
    .unwrap();
}
