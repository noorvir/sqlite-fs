use sqlite_fs::SqliteFs;
use std::path::PathBuf;

fn main() {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let mut db = None;
    let mut backing = None;
    let mut mountpoint = None;

    let mut index = 0;
    while index < args.len() {
        if args[index] == "--db" {
            index += 1;
            if index >= args.len() {
                usage();
            }
            db = Some(PathBuf::from(&args[index]));
        } else if args[index] == "--backing" {
            index += 1;
            if index >= args.len() {
                usage();
            }
            backing = Some(PathBuf::from(&args[index]));
        } else if mountpoint.is_none() {
            mountpoint = Some(PathBuf::from(&args[index]));
        } else {
            usage();
        }
        index += 1;
    }

    let mountpoint = mountpoint.unwrap_or_else(|| usage());
    let fs: Box<dyn minfuse::FileSystem> = if let Some(db) = db {
        if let Some(backing) = backing {
            Box::new(SqliteFs::open_with_backing(db, backing).unwrap())
        } else {
            Box::new(SqliteFs::open(db).unwrap())
        }
    } else {
        if backing.is_some() {
            usage();
        }
        Box::new(minfs::MinFs::new())
    };

    let mut options = minfuse::MountOptions::default();
    options.debug = std::env::var_os("SLFS_DEBUG").is_some();
    minfuse::mount_boxed(fs, mountpoint, options).unwrap();
}

fn usage() -> ! {
    eprintln!(
        "usage: sqlite-fs [--db path/to/db.sqlite [--backing path/to/files]] /Volumes/sqlite-fs"
    );
    std::process::exit(2);
}
