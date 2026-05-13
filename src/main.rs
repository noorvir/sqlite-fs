use sqlite_fs::SqliteFs;
use std::path::PathBuf;

fn main() {
    if let Err(err) = run() {
        eprintln!("sqlite-fs: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
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
    let fs: Box<dyn minfuse::FileSystem> = match (db, backing) {
        (Some(db), Some(backing)) => Box::new(
            SqliteFs::open_with_backing(db, backing)
                .map_err(|err| format!("failed to open SQLite filesystem: {err:?}"))?,
        ),
        (Some(db), None) => Box::new(
            SqliteFs::open(db)
                .map_err(|err| format!("failed to open SQLite filesystem: {err:?}"))?,
        ),
        (None, Some(_)) => usage(),
        (None, None) => Box::new(minfs::MinFs::new()),
    };

    let options = minfuse::MountOptions {
        debug: std::env::var_os("SLFS_DEBUG").is_some(),
        ..Default::default()
    };
    minfuse::mount_boxed(fs, mountpoint, options).map_err(|err| format!("mount failed: {err}"))
}

fn usage() -> ! {
    eprintln!(
        "usage: sqlite-fs [--db path/to/db.sqlite [--backing path/to/files]] /Volumes/sqlite-fs"
    );
    std::process::exit(2);
}
