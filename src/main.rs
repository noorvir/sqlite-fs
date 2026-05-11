use sqlite_fs::SqliteFs;
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let fs: Box<dyn minfuse::FileSystem> = if args.first().is_some_and(|arg| arg == "--db") {
        if args.len() != 3 {
            usage();
        }
        let db = PathBuf::from(args.remove(1));
        args.remove(0);
        Box::new(SqliteFs::open(db).unwrap())
    } else {
        if args.len() != 1 {
            usage();
        }
        Box::new(minfs::MinFs::new())
    };

    let mountpoint = PathBuf::from(args.remove(0));
    let mut options = minfuse::MountOptions::default();
    options.debug = std::env::var_os("SLFS_DEBUG").is_some();
    minfuse::mount_boxed(fs, mountpoint, options).unwrap();
}

fn usage() -> ! {
    eprintln!("usage: sqlite-fs [--db path/to/db.sqlite] /Volumes/sqlite-fs");
    std::process::exit(2);
}
