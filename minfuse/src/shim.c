#define FUSE_USE_VERSION 31
#define FUSE_DARWIN_ENABLE_EXTENSIONS 0
#define _FILE_OFFSET_BITS 64

#include <fuse3/fuse.h>
#include <stdint.h>
#include <string.h>

struct minfuse_callbacks {
    int (*getattr)(void *, const char *, struct stat *);
    int (*readlink)(void *, const char *, char *, size_t);
    int (*mknod)(void *, const char *, mode_t, dev_t);
    int (*mkdir)(void *, const char *, mode_t);
    int (*unlink)(void *, const char *);
    int (*rmdir)(void *, const char *);
    int (*symlink)(void *, const char *, const char *);
    int (*rename)(void *, const char *, const char *, unsigned int);
    int (*link)(void *, const char *, const char *);
    int (*chmod)(void *, const char *, mode_t, int, uint64_t);
    int (*chown)(void *, const char *, uid_t, gid_t, int, uint64_t);
    int (*truncate)(void *, const char *, off_t, int, uint64_t);
    int (*open)(void *, const char *, int, uint64_t *);
    int (*create)(void *, const char *, mode_t, int, uint64_t *);
    int (*read)(void *, const char *, uint64_t, char *, size_t, off_t);
    int (*write)(void *, const char *, uint64_t, const char *, size_t, off_t);
    int (*statfs)(void *, const char *, struct statvfs *);
    int (*flush)(void *, const char *, uint64_t);
    int (*release)(void *, const char *, uint64_t);
    int (*fsync)(void *, const char *, uint64_t, int);
    int (*setxattr)(void *, const char *, const char *, const char *, size_t, int);
    int (*getxattr)(void *, const char *, const char *, char *, size_t);
    int (*listxattr)(void *, const char *, char *, size_t);
    int (*removexattr)(void *, const char *, const char *);
    int (*opendir)(void *, const char *, int, uint64_t *);
    int (*readdir)(void *, const char *, void *, fuse_fill_dir_t, off_t);
    int (*releasedir)(void *, const char *, uint64_t);
    int (*fsyncdir)(void *, const char *, uint64_t, int);
    int (*access)(void *, const char *, int);
    int (*utimens)(void *, const char *, const struct timespec *, int, uint64_t);
};

struct minfuse_state {
    struct minfuse_callbacks callbacks;
    void *user_data;
};

static struct minfuse_state *state(void) {
    return (struct minfuse_state *)fuse_get_context()->private_data;
}

static uint64_t fh(struct fuse_file_info *fi) {
    return fi ? fi->fh : 0;
}

static int has_fh(struct fuse_file_info *fi) {
    return fi ? 1 : 0;
}

static int wrap_getattr(const char *path, struct stat *st, struct fuse_file_info *fi) {
    (void)fi;
    struct minfuse_state *s = state();
    return s->callbacks.getattr(s->user_data, path, st);
}

static int wrap_readlink(const char *path, char *buf, size_t size) {
    struct minfuse_state *s = state();
    return s->callbacks.readlink(s->user_data, path, buf, size);
}

static int wrap_mknod(const char *path, mode_t mode, dev_t rdev) {
    struct minfuse_state *s = state();
    return s->callbacks.mknod(s->user_data, path, mode, rdev);
}

static int wrap_mkdir(const char *path, mode_t mode) {
    struct minfuse_state *s = state();
    return s->callbacks.mkdir(s->user_data, path, mode);
}

static int wrap_unlink(const char *path) {
    struct minfuse_state *s = state();
    return s->callbacks.unlink(s->user_data, path);
}

static int wrap_rmdir(const char *path) {
    struct minfuse_state *s = state();
    return s->callbacks.rmdir(s->user_data, path);
}

static int wrap_symlink(const char *linkname, const char *path) {
    struct minfuse_state *s = state();
    return s->callbacks.symlink(s->user_data, linkname, path);
}

static int wrap_rename(const char *from, const char *to, unsigned int flags) {
    struct minfuse_state *s = state();
    return s->callbacks.rename(s->user_data, from, to, flags);
}

static int wrap_link(const char *from, const char *to) {
    struct minfuse_state *s = state();
    return s->callbacks.link(s->user_data, from, to);
}

static int wrap_chmod(const char *path, mode_t mode, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    return s->callbacks.chmod(s->user_data, path, mode, has_fh(fi), fh(fi));
}

static int wrap_chown(const char *path, uid_t uid, gid_t gid, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    return s->callbacks.chown(s->user_data, path, uid, gid, has_fh(fi), fh(fi));
}

static int wrap_truncate(const char *path, off_t size, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    return s->callbacks.truncate(s->user_data, path, size, has_fh(fi), fh(fi));
}

static int wrap_open(const char *path, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    uint64_t handle = 0;
    int rc = s->callbacks.open(s->user_data, path, fi ? fi->flags : 0, &handle);
    if (rc == 0 && fi) {
        fi->fh = handle;
    }
    return rc;
}

static int wrap_create(const char *path, mode_t mode, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    uint64_t handle = 0;
    int rc = s->callbacks.create(s->user_data, path, mode, fi ? fi->flags : 0, &handle);
    if (rc == 0 && fi) {
        fi->fh = handle;
    }
    return rc;
}

static int wrap_read(const char *path, char *buf, size_t size, off_t off, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    return s->callbacks.read(s->user_data, path, fh(fi), buf, size, off);
}

static int wrap_write(const char *path, const char *buf, size_t size, off_t off, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    return s->callbacks.write(s->user_data, path, fh(fi), buf, size, off);
}

static int wrap_statfs(const char *path, struct statvfs *st) {
    struct minfuse_state *s = state();
    return s->callbacks.statfs(s->user_data, path, st);
}

static int wrap_flush(const char *path, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    return s->callbacks.flush(s->user_data, path, fh(fi));
}

static int wrap_release(const char *path, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    return s->callbacks.release(s->user_data, path, fh(fi));
}

static int wrap_fsync(const char *path, int datasync, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    return s->callbacks.fsync(s->user_data, path, fh(fi), datasync);
}

static int wrap_setxattr(const char *path, const char *name, const char *value, size_t size, int flags) {
    struct minfuse_state *s = state();
    return s->callbacks.setxattr(s->user_data, path, name, value, size, flags);
}

static int wrap_getxattr(const char *path, const char *name, char *value, size_t size) {
    struct minfuse_state *s = state();
    return s->callbacks.getxattr(s->user_data, path, name, value, size);
}

static int wrap_listxattr(const char *path, char *list, size_t size) {
    struct minfuse_state *s = state();
    return s->callbacks.listxattr(s->user_data, path, list, size);
}

static int wrap_removexattr(const char *path, const char *name) {
    struct minfuse_state *s = state();
    return s->callbacks.removexattr(s->user_data, path, name);
}

static int wrap_opendir(const char *path, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    uint64_t handle = 0;
    int rc = s->callbacks.opendir(s->user_data, path, fi ? fi->flags : 0, &handle);
    if (rc == 0 && fi) {
        fi->fh = handle;
    }
    return rc;
}

static int wrap_readdir(const char *path, void *buf, fuse_fill_dir_t filler,
                        off_t off, struct fuse_file_info *fi,
                        enum fuse_readdir_flags flags) {
    (void)fi;
    (void)flags;
    struct minfuse_state *s = state();
    return s->callbacks.readdir(s->user_data, path, buf, filler, off);
}

static int wrap_releasedir(const char *path, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    return s->callbacks.releasedir(s->user_data, path, fh(fi));
}

static int wrap_fsyncdir(const char *path, int datasync, struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    return s->callbacks.fsyncdir(s->user_data, path, fh(fi), datasync);
}

static int wrap_access(const char *path, int mask) {
    struct minfuse_state *s = state();
    return s->callbacks.access(s->user_data, path, mask);
}

static int wrap_utimens(const char *path, const struct timespec tv[2], struct fuse_file_info *fi) {
    struct minfuse_state *s = state();
    return s->callbacks.utimens(s->user_data, path, tv, has_fh(fi), fh(fi));
}

int minfuse_mount(int argc, char **argv, struct minfuse_callbacks *callbacks, void *user_data) {
    struct minfuse_state state = { *callbacks, user_data };

    struct fuse_operations ops;
    memset(&ops, 0, sizeof(ops));
    ops.getattr = wrap_getattr;
    ops.readlink = wrap_readlink;
    ops.mknod = wrap_mknod;
    ops.mkdir = wrap_mkdir;
    ops.unlink = wrap_unlink;
    ops.rmdir = wrap_rmdir;
    ops.symlink = wrap_symlink;
    ops.rename = wrap_rename;
    ops.link = wrap_link;
    ops.chmod = wrap_chmod;
    ops.chown = wrap_chown;
    ops.truncate = wrap_truncate;
    ops.open = wrap_open;
    ops.create = wrap_create;
    ops.read = wrap_read;
    ops.write = wrap_write;
    ops.statfs = wrap_statfs;
    ops.flush = wrap_flush;
    ops.release = wrap_release;
    ops.fsync = wrap_fsync;
    ops.setxattr = wrap_setxattr;
    ops.getxattr = wrap_getxattr;
    ops.listxattr = wrap_listxattr;
    ops.removexattr = wrap_removexattr;
    ops.opendir = wrap_opendir;
    ops.readdir = wrap_readdir;
    ops.releasedir = wrap_releasedir;
    ops.fsyncdir = wrap_fsyncdir;
    ops.access = wrap_access;
    ops.utimens = wrap_utimens;

    return fuse_main(argc, argv, &ops, &state);
}
