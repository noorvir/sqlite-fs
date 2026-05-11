fn main() {
    println!("cargo:rerun-if-changed=src/shim.c");

    let header = std::path::Path::new("/usr/local/include/fuse3/fuse.h");
    let lib = std::path::Path::new("/usr/local/lib/libfuse3.dylib");
    if !header.exists() || !lib.exists() {
        panic!("macFUSE libfuse3 not found. Install macFUSE first.");
    }

    cc::Build::new()
        .file("src/shim.c")
        .include("/usr/local/include")
        .define("FUSE_USE_VERSION", "31")
        .define("FUSE_DARWIN_ENABLE_EXTENSIONS", "0")
        .define("_FILE_OFFSET_BITS", "64")
        .compile("minfuse_shim");

    println!("cargo:rustc-link-search=native=/usr/local/lib");
    println!("cargo:rustc-link-lib=dylib=fuse3");
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/local/lib");
}
