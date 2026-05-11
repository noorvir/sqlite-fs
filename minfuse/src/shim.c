#define FUSE_USE_VERSION 31
#define FUSE_DARWIN_ENABLE_EXTENSIONS 0
#define _FILE_OFFSET_BITS 64

#include <fuse3/fuse.h>
#include <string.h>

struct minfuse_callbacks {
    int (*getattr)(void *, const char *, struct stat *);
    int (*readdir)(void *, const char *, void *, fuse_fill_dir_t, off_t);
    int (*open)(void *, const char *, struct fuse_file_info *);
    int (*read)(void *, const char *, char *, size_t, off_t, struct fuse_file_info *);
    int (*write)(void *, const char *, const char *, size_t, off_t, struct fuse_file_info *);
    int (*truncate)(void *, const char *, off_t);
};

struct minfuse_state {
    struct minfuse_callbacks callbacks;
    void *user_data;
};

static struct minfuse_state *state(void) {
    return (struct minfuse_state *)fuse_get_context()->private_data;
}

static int wrap_getattr(const char *path, struct stat *st, struct fuse_file_info *fi) {
    (void)fi;
    struct minfuse_state *s = state();
    return s->callbacks.getattr(s->user_data, path, st);
}

static int wrap_readdir(const char *path, void *buf, fuse_fill_dir_t filler,
                        off_t off, struct fuse_file_info *fi,
                        enum fuse_readdir_flags flags) {
    (void)fi;
    (void)flags;
    struct minfuse_state *s = state();
    return s->callbacks.readdir(s->user_data, path, buf, filler, off);
}

static int wrap_open(const char *path, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    return s->callbacks.open(s->user_data, path, fi);
}

static int wrap_read(const char *path, char *buf, size_t size, off_t off, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    return s->callbacks.read(s->user_data, path, buf, size, off, fi);
}

static int wrap_write(const char *path, const char *buf, size_t size, off_t off, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    return s->callbacks.write(s->user_data, path, buf, size, off, fi);
}

static int wrap_truncate(const char *path, off_t size, struct fuse_file_info *fi) {
    (void)fi;
    struct minfuse_state *s = state();
    return s->callbacks.truncate(s->user_data, path, size);
}

int minfuse_mount(int argc, char **argv, struct minfuse_callbacks *callbacks, void *user_data) {
    struct minfuse_state state = { *callbacks, user_data };

    struct fuse_operations ops;
    memset(&ops, 0, sizeof(ops));
    ops.getattr = wrap_getattr;
    ops.readdir = wrap_readdir;
    ops.open = wrap_open;
    ops.read = wrap_read;
    ops.write = wrap_write;
    ops.truncate = wrap_truncate;

    return fuse_main(argc, argv, &ops, &state);
}
