/*
  FUSE: Filesystem in Userspace
  Copyright (C) 2001-2007  Miklos Szeredi <miklos@szeredi.hu>

  This program can be distributed under the terms of the GNU GPLv2.
  See the file COPYING.
*/

/** @file
 *
 * This file system mirrors the existing file system hierarchy of the
 * system, starting at the root file system. This is implemented by
 * just "passing through" all requests to the corresponding user-space
 * libc functions. In contrast to passthrough.c and passthrough_fh.c,
 * this implementation uses the low-level API. Its performance should
 * be the least bad among the three, but many operations are not
 * implemented. In particular, it is not possible to remove files (or
 * directories) because the code necessary to defer actual removal
 * until the file is not opened anymore would make the example much
 * more complicated.
 *
 * When writeback caching is enabled (-o writeback mount option), it
 * is only possible to write to files for which the mounting user has
 * read permissions. This is because the writeback cache requires the
 * kernel to be able to issue read requests for all files (which the
 * passthrough filesystem cannot satisfy if it can't read the file in
 * the underlying filesystem).
 *
 * Compile with:
 *
 *     gcc -Wall passthrough_ll.c `pkg-config fuse3 --cflags --libs` -o passthrough_ll
 *
 * ## Source code ##
 * \include passthrough_ll.c
 */

#define _GNU_SOURCE
#define FUSE_USE_VERSION FUSE_MAKE_VERSION(3, 12)

#include <fuse_lowlevel.h>
#include <unistd.h>
#include <stdlib.h>
#include <stdio.h>
#include <stddef.h>
#include <stdbool.h>
#include <string.h>
#include <limits.h>
#include <dirent.h>
#include <assert.h>
#include <errno.h>
#include <inttypes.h>
#include <pthread.h>
#include <sys/file.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/xattr.h>

#include "passthrough_helpers.h"

/* We are re-using pointers to our `struct lo_inode` and `struct
   lo_dirp` elements as inodes. This means that we must be able to
   store uintptr_t values in a fuse_ino_t variable. The following
   incantation checks this condition at compile time. */
#if defined(__GNUC__) && (__GNUC__ > 4 || __GNUC__ == 4 && __GNUC_MINOR__ >= 6) && !defined __cplusplus
_Static_assert(sizeof(fuse_ino_t) >= sizeof(uintptr_t),
	       "fuse_ino_t too small to hold uintptr_t values!");
#else
struct _uintptr_to_must_hold_fuse_ino_t_dummy_struct \
	{ unsigned _uintptr_to_must_hold_fuse_ino_t:
			((sizeof(fuse_ino_t) >= sizeof(uintptr_t)) ? 1 : -1); };
#endif

struct lo_inode {
	struct lo_inode *next; /* protected by lo->mutex */
	struct lo_inode *prev; /* protected by lo->mutex */
	int fd;
	ino_t ino;
	dev_t dev;
	uint64_t refcount; /* protected by lo->mutex */
};

enum {
	CACHE_NEVER,
	CACHE_NORMAL,
	CACHE_ALWAYS,
};

struct lo_data {
	pthread_mutex_t mutex;
	int debug;
	int writeback;
	int range_reads;
	int relaxed_create_durability;
	int defer_file_namespace;
	int flock;
	int xattr;
	char *source;
	char *policy_file;
	double timeout;
	int cache;
	int timeout_set;
	struct lo_inode root; /* protected by lo->mutex */
};

static const struct fuse_opt lo_opts[] = {
	{ "writeback",
	  offsetof(struct lo_data, writeback), 1 },
	{ "no_writeback",
	  offsetof(struct lo_data, writeback), 0 },
	{ "range_reads",
	  offsetof(struct lo_data, range_reads), 1 },
	{ "no_range_reads",
	  offsetof(struct lo_data, range_reads), 0 },
	{ "relaxed_create_durability",
	  offsetof(struct lo_data, relaxed_create_durability), 1 },
	{ "strict_create_durability",
	  offsetof(struct lo_data, relaxed_create_durability), 0 },
	{ "defer_file_namespace",
	  offsetof(struct lo_data, defer_file_namespace), 1 },
	{ "canonical_file_namespace",
	  offsetof(struct lo_data, defer_file_namespace), 0 },
	{ "source=%s",
	  offsetof(struct lo_data, source), 0 },
	{ "policy_file=%s",
	  offsetof(struct lo_data, policy_file), 0 },
	{ "flock",
	  offsetof(struct lo_data, flock), 1 },
	{ "no_flock",
	  offsetof(struct lo_data, flock), 0 },
	{ "xattr",
	  offsetof(struct lo_data, xattr), 1 },
	{ "no_xattr",
	  offsetof(struct lo_data, xattr), 0 },
	{ "timeout=%lf",
	  offsetof(struct lo_data, timeout), 0 },
	{ "timeout=",
	  offsetof(struct lo_data, timeout_set), 1 },
	{ "cache=never",
	  offsetof(struct lo_data, cache), CACHE_NEVER },
	{ "cache=auto",
	  offsetof(struct lo_data, cache), CACHE_NORMAL },
	{ "cache=always",
	  offsetof(struct lo_data, cache), CACHE_ALWAYS },

	FUSE_OPT_END
};

static void passthrough_ll_help(void)
{
	printf(
"    -o writeback           Enable writeback\n"
"    -o no_writeback        Disable write back\n"
"    -o range_reads         Route large reads through FUSE read callbacks\n"
"    -o no_range_reads      Use passthrough for large reads\n"
"    -o relaxed_create_durability  Do not fsync new overlay namespace before app durability requests\n"
"    -o strict_create_durability   Eagerly persist new overlay namespace (default)\n"
"    -o defer_file_namespace       Keep new regular-file dentries NVMe-only until checkpoint\n"
"    -o canonical_file_namespace   Create HDD regular-file dentries immediately (default)\n"
"    -o source=/home/dir    Source directory to be mounted\n"
"    -o policy_file=/path   Longest-prefix durable/volatile policy map\n"
"    -o flock               Enable flock\n"
"    -o no_flock            Disable flock\n"
"    -o xattr               Enable xattr\n"
"    -o no_xattr            Disable xattr\n"
"    -o timeout=1.0         Caching timeout\n"
"    -o timeout=0/1         Timeout is set\n"
"    -o cache=never         Disable cache\n"
"    -o cache=auto          Auto enable cache\n"
"    -o cache=always        Cache always\n");
}

static struct lo_data *lo_data(fuse_req_t req)
{
	return (struct lo_data *) fuse_req_userdata(req);
}

static struct lo_inode *lo_inode(fuse_req_t req, fuse_ino_t ino)
{
	if (ino == FUSE_ROOT_ID)
		return &lo_data(req)->root;
	else
		return (struct lo_inode *) (uintptr_t) ino;
}

static int lo_fd(fuse_req_t req, fuse_ino_t ino)
{
	return lo_inode(req, ino)->fd;
}

static bool lo_debug(fuse_req_t req)
{
	return lo_data(req)->debug != 0;
}

#define DURABLE_WRITEBACK_ROOT "/var/lib/morainefs/overlay"
#define DURABLE_CHECKPOINT_STATE_ROOT "/var/lib/morainefs/metadata/generations"
#define DURABLE_NAMESPACE_STATE_ROOT "/var/lib/morainefs/metadata/journal"
#define DURABLE_RENAME_STATE_ROOT "/var/lib/morainefs/metadata/journal"
#define DURABLE_CHECKPOINT_SOCKET "/run/morainefs/checkpoint.sock"

#define VOLATILE_WRITEBACK_ROOT "/run/morainefs/volatile/overlay"
#define VOLATILE_CHECKPOINT_STATE_ROOT "/run/morainefs/volatile/metadata/generations"
#define VOLATILE_NAMESPACE_STATE_ROOT "/run/morainefs/volatile/metadata/journal"
#define VOLATILE_RENAME_STATE_ROOT "/run/morainefs/volatile/metadata/journal"
#define VOLATILE_CHECKPOINT_SOCKET "/run/morainefs/volatile/checkpoint.sock"

enum path_policy_kind {
	PATH_POLICY_DURABLE = 0,
	PATH_POLICY_VOLATILE = 1,
};

#define MAX_POLICY_RULES 128
struct path_policy_rule {
	char prefix[PATH_MAX + 1];
	enum path_policy_kind kind;
};

static struct path_policy_rule g_policy_rules[MAX_POLICY_RULES];
static size_t g_policy_rule_count;
static enum path_policy_kind g_default_policy = PATH_POLICY_DURABLE;

static bool path_policy_prefix_matches(const char *path, const char *prefix)
{
	size_t n = strlen(prefix);

	if (n == 1 && prefix[0] == '/')
		return path[0] == '/';
	if (strncmp(path, prefix, n) != 0)
		return false;
	return path[n] == 0 || path[n] == '/';
}

static enum path_policy_kind policy_for_source(const char *source_path)
{
	enum path_policy_kind kind = g_default_policy;
	size_t best = 0;

	for (size_t i = 0; i < g_policy_rule_count; ++i) {
		size_t n = strlen(g_policy_rules[i].prefix);
		if (n >= best &&
		    path_policy_prefix_matches(source_path, g_policy_rules[i].prefix)) {
			best = n;
			kind = g_policy_rules[i].kind;
		}
	}
	return kind;
}

static bool source_is_volatile(const char *source_path)
{
	return policy_for_source(source_path) == PATH_POLICY_VOLATILE;
}

static const char *writeback_root_for_source(const char *source_path)
{
	return source_is_volatile(source_path) ?
	       VOLATILE_WRITEBACK_ROOT : DURABLE_WRITEBACK_ROOT;
}

static const char *checkpoint_state_root_for_source(const char *source_path)
{
	return source_is_volatile(source_path) ?
	       VOLATILE_CHECKPOINT_STATE_ROOT : DURABLE_CHECKPOINT_STATE_ROOT;
}

static const char *namespace_state_root_for_source(const char *source_path)
{
	return source_is_volatile(source_path) ?
	       VOLATILE_NAMESPACE_STATE_ROOT : DURABLE_NAMESPACE_STATE_ROOT;
}

static const char *rename_state_root_for_source(const char *source_path)
{
	return source_is_volatile(source_path) ?
	       VOLATILE_RENAME_STATE_ROOT : DURABLE_RENAME_STATE_ROOT;
}

static const char *checkpoint_socket_for_source(const char *source_path)
{
	return source_is_volatile(source_path) ?
	       VOLATILE_CHECKPOINT_SOCKET : DURABLE_CHECKPOINT_SOCKET;
}

static void apply_writeback_stat_for_fd(int canonical_fd, struct stat *base);
static int lo_open_existing_writeback(fuse_req_t req, fuse_ino_t ino, int flags);
static int source_path_for_inode(fuse_req_t req, fuse_ino_t ino,
                                 char *out, size_t out_size);
static int source_path_for_child(fuse_req_t req, fuse_ino_t parent,
                                 const char *name, char *out, size_t out_size);
static bool writeback_exists_for_source(const char *source_path);
static bool writeback_is_clean_for_source(const char *source_path);
static int writeback_path_for_source(const char *source_path,
                                     char *out, size_t out_size);
static int checkpoint_state_path_for_source(const char *source_path,
                                            char *out, size_t out_size);
static int mkdir_parents(const char *path);
static int fsync_parent_path(const char *path);
static void notify_checkpoint_path(const char *source_path);
static int is_dot_or_dotdot(const char *name);

static int rename_marker_path_for_source(const char *new_source,
                                         char *out, size_t out_size)
{
	const char *root = rename_state_root_for_source(new_source);
	int n = snprintf(out, out_size, "%s%s.rename", root, new_source);
	return n >= 0 && (size_t)n < out_size ? 0 : -1;
}

static bool rename_marker_exists(const char *new_source)
{
	char marker[PATH_MAX + 512];
	struct stat st;

	if (rename_marker_path_for_source(new_source, marker, sizeof(marker)) == -1)
		return false;
	return lstat(marker, &st) == 0 && S_ISREG(st.st_mode);
}

static int rename_ready_path_for_source(const char *new_source,
                                        char *out, size_t out_size)
{
	const char *root = rename_state_root_for_source(new_source);
	int n = snprintf(out, out_size, "%s%s.rename.ready", root, new_source);
	return n >= 0 && (size_t)n < out_size ? 0 : -1;
}

static int rename_dest_backup_path_for_source(const char *new_source,
                                               char *out, size_t out_size)
{
	const char *root = rename_state_root_for_source(new_source);
	int n = snprintf(out, out_size, "%s%s.rename.dst-overlay", root, new_source);
	return n >= 0 && (size_t)n < out_size ? 0 : -1;
}

static bool rename_ready_exists(const char *new_source)
{
	char ready[PATH_MAX + 512];
	struct stat st;

	if (rename_ready_path_for_source(new_source, ready, sizeof(ready)) == -1)
		return false;
	return lstat(ready, &st) == 0 && S_ISREG(st.st_mode);
}

static int read_rename_source(const char *new_source, char *out, size_t out_size)
{
	char marker[PATH_MAX + 512];
	struct stat st;
	ssize_t n;
	int fd;
	int saved;

	if (rename_marker_path_for_source(new_source, marker, sizeof(marker)) == -1) {
		errno = ENAMETOOLONG;
		return -1;
	}
	fd = open(marker, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
	if (fd == -1)
		return -1;
	if (fstat(fd, &st) == -1 || st.st_size <= 0 ||
	    (uint64_t)st.st_size >= out_size) {
		saved = errno ? errno : EINVAL;
		close(fd);
		errno = saved;
		return -1;
	}
	n = read(fd, out, (size_t)st.st_size);
	if (n != st.st_size || memchr(out, '\0', (size_t)n) != NULL ||
	    out[0] != '/') {
		saved = errno ? errno : EINVAL;
		close(fd);
		errno = saved;
		return -1;
	}
	out[n] = '\0';
	close(fd);
	return 0;
}

static int clear_rename_ready(const char *new_source)
{
	char ready[PATH_MAX + 512];

	if (rename_ready_path_for_source(new_source, ready, sizeof(ready)) == -1)
		return -1;
	if (unlink(ready) == -1) {
		if (errno == ENOENT)
			return 0;
		return -1;
	}
	return fsync_parent_path(ready);
}

static int clear_rename_dest_backup(const char *new_source)
{
	char backup[PATH_MAX + 512];

	if (rename_dest_backup_path_for_source(new_source, backup, sizeof(backup)) == -1)
		return -1;
	if (unlink(backup) == -1) {
		if (errno == ENOENT)
			return 0;
		return -1;
	}
	return fsync_parent_path(backup);
}

static int mark_rename_source(const char *old_source, const char *new_source)
{
	char marker[PATH_MAX + 512];
	char ready[PATH_MAX + 512];
	const char *p = old_source;
	size_t left = strlen(old_source);
	int fd;
	int saved;

	if (rename_marker_path_for_source(new_source, marker, sizeof(marker)) == -1 ||
	    rename_ready_path_for_source(new_source, ready, sizeof(ready)) == -1)
		return -1;
	if (mkdir_parents(marker) == -1)
		return -1;

	/*
	 * A ready file without its intent should be impossible, but remove one
	 * defensively before reusing this destination. The marker-parent fsync
	 * below also makes this unlink durable.
	 */
	if (unlink(ready) == -1 && errno != ENOENT)
		return -1;

	fd = open(marker, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC | O_NOFOLLOW,
	          0600);
	if (fd == -1)
		return -1;
	while (left) {
		ssize_t n = write(fd, p, left);
		if (n <= 0) {
			saved = errno ? errno : EIO;
			close(fd);
			errno = saved;
			return -1;
		}
		p += n;
		left -= (size_t)n;
	}
	if (fsync(fd) == -1) {
		saved = errno;
		close(fd);
		errno = saved;
		return -1;
	}
	close(fd);
	return fsync_parent_path(marker);
}

static int mark_rename_ready(const char *new_source)
{
	char ready[PATH_MAX + 512];
	static const char payload[] = "ready-v1\n";
	int fd;
	int saved;

	if (rename_ready_path_for_source(new_source, ready, sizeof(ready)) == -1)
		return -1;
	if (mkdir_parents(ready) == -1)
		return -1;
	fd = open(ready, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC | O_NOFOLLOW,
	          0600);
	if (fd == -1)
		return -1;
	if (write(fd, payload, sizeof(payload) - 1) !=
	        (ssize_t)(sizeof(payload) - 1) ||
	    fsync(fd) == -1) {
		saved = errno ? errno : EIO;
		close(fd);
		errno = saved;
		return -1;
	}
	close(fd);
	return fsync_parent_path(ready);
}

static int clear_rename_marker(const char *new_source)
{
	char marker[PATH_MAX + 512];

	if (rename_marker_path_for_source(new_source, marker, sizeof(marker)) == -1)
		return -1;
	if (unlink(marker) == -1) {
		if (errno == ENOENT)
			return 0;
		return -1;
	}
	return fsync_parent_path(marker);
}

static int create_writeback_for_source(const char *source_path, int flags,
                                       mode_t mode, int canonical_fd,
                                       bool relaxed_create_durability);
static bool created_marker_exists(const char *source_path);
static int mark_created_source(const char *source_path);
static int clear_created_marker(const char *source_path);
static bool rename_marker_exists(const char *new_source);
static int mark_rename_source(const char *old_source, const char *new_source);
static int clear_rename_marker(const char *new_source);
static void drop_writeback_for_source(const char *source_path);
static void drop_micro_for_source(const char *source_path);
static void notify_checkpoint_fd(int fd);
static bool track_backing_id(int fd, int backing_id);
static bool track_range_fd(int fd);
static bool take_range_fd(int fd);
static size_t range_block_size_for_read(int fd, off_t offset, size_t size);
static bool range_cache_reply(fuse_req_t req, int source_fd,
                              size_t size, off_t offset, size_t block_size);
static int mkdir_parents(const char *path);

static void lo_init(void *userdata,
		    struct fuse_conn_info *conn)
{
	struct lo_data *lo = (struct lo_data *)userdata;
	bool has_flag;

	if (lo->writeback) {
		has_flag = fuse_set_feature_flag(conn, FUSE_CAP_WRITEBACK_CACHE);
		if (lo->debug && has_flag)
			fuse_log(FUSE_LOG_DEBUG,
				 "lo_init: activating writeback\n");
	}
	if (lo->flock && conn->capable & FUSE_CAP_FLOCK_LOCKS) {
		has_flag = fuse_set_feature_flag(conn, FUSE_CAP_FLOCK_LOCKS);
		if (lo->debug && has_flag)
			fuse_log(FUSE_LOG_DEBUG,
				 "lo_init: activating flock locks\n");
	}

	has_flag = fuse_set_feature_flag(conn, FUSE_CAP_PASSTHROUGH);
	if (lo->debug)
		fuse_log(FUSE_LOG_DEBUG,
			 "lo_init: passthrough capable=%d enabled=%d max_backing_stack=%u\n",
			 !!(conn->capable_ext & FUSE_CAP_PASSTHROUGH), has_flag,
			 conn->max_backing_stack_depth);

	/* Disable the receiving and processing of FUSE_INTERRUPT requests */
	conn->no_interrupt = 1;
}

static void lo_destroy(void *userdata)
{
	struct lo_data *lo = (struct lo_data*) userdata;

	while (lo->root.next != &lo->root) {
		struct lo_inode* next = lo->root.next;
		lo->root.next = next->next;
		close(next->fd);
		free(next);
	}
}

static void lo_getattr(fuse_req_t req, fuse_ino_t ino,
			     struct fuse_file_info *fi)
{
	int res;
	struct stat buf;
	struct lo_data *lo = lo_data(req);
	int fd = lo_fd(req, ino);

	(void) fi;

	res = fstatat(fd, "", &buf, AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW);
	if (res == -1)
		return (void) fuse_reply_err(req, errno);
	apply_writeback_stat_for_fd(fd, &buf);

	fuse_reply_attr(req, &buf, lo->timeout);
}

static void lo_setattr(fuse_req_t req, fuse_ino_t ino, struct stat *attr,
		       int valid, struct fuse_file_info *fi)
{
	int saverr;
	char procname[64];
	struct lo_inode *inode = lo_inode(req, ino);
	int ifd = inode->fd;
	int overlay_fd = -1;
	int target_fd = -1;
	int res;

	/*
	 * An existing NVMe writeback copy is authoritative.  Path-based setattr
	 * requests therefore have to hit it rather than silently mutating the
	 * stale canonical HDD inode underneath it.
	 */
	if (fi)
		target_fd = (int)fi->fh;
	else {
		overlay_fd = lo_open_existing_writeback(req, ino, O_RDWR | O_CLOEXEC);
		if (overlay_fd != -1)
			target_fd = overlay_fd;
	}

	if (valid & FUSE_SET_ATTR_MODE) {
		if (target_fd != -1)
			res = fchmod(target_fd, attr->st_mode);
		else {
			sprintf(procname, "/proc/self/fd/%i", ifd);
			res = chmod(procname, attr->st_mode);
		}
		if (res == -1)
			goto out_err;
	}
	if (valid & (FUSE_SET_ATTR_UID | FUSE_SET_ATTR_GID)) {
		uid_t uid = (valid & FUSE_SET_ATTR_UID) ?
			attr->st_uid : (uid_t) -1;
		gid_t gid = (valid & FUSE_SET_ATTR_GID) ?
			attr->st_gid : (gid_t) -1;

		if (target_fd != -1)
			res = fchown(target_fd, uid, gid);
		else
			res = fchownat(ifd, "", uid, gid,
				       AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW);
		if (res == -1)
			goto out_err;
	}
	if (valid & FUSE_SET_ATTR_SIZE) {
		if (target_fd != -1)
			res = ftruncate(target_fd, attr->st_size);
		else {
			sprintf(procname, "/proc/self/fd/%i", ifd);
			res = truncate(procname, attr->st_size);
		}
		if (res == -1)
			goto out_err;
	}
	if (valid & (FUSE_SET_ATTR_ATIME | FUSE_SET_ATTR_MTIME)) {
		struct timespec tv[2];

		tv[0].tv_sec = 0;
		tv[1].tv_sec = 0;
		tv[0].tv_nsec = UTIME_OMIT;
		tv[1].tv_nsec = UTIME_OMIT;

		if (valid & FUSE_SET_ATTR_ATIME_NOW)
			tv[0].tv_nsec = UTIME_NOW;
		else if (valid & FUSE_SET_ATTR_ATIME)
			tv[0] = attr->st_atim;

		if (valid & FUSE_SET_ATTR_MTIME_NOW)
			tv[1].tv_nsec = UTIME_NOW;
		else if (valid & FUSE_SET_ATTR_MTIME)
			tv[1] = attr->st_mtim;

		if (target_fd != -1)
			res = futimens(target_fd, tv);
		else {
			sprintf(procname, "/proc/self/fd/%i", ifd);
			res = utimensat(AT_FDCWD, procname, tv, 0);
		}
		if (res == -1)
			goto out_err;
	}

	if (overlay_fd != -1) {
		notify_checkpoint_fd(overlay_fd);
		close(overlay_fd);
	}
	return lo_getattr(req, ino, fi);

out_err:
	saverr = errno;
	if (overlay_fd != -1)
		close(overlay_fd);
	fuse_reply_err(req, saverr);
}

static struct lo_inode *lo_find(struct lo_data *lo, struct stat *st)
{
	struct lo_inode *p;
	struct lo_inode *ret = NULL;

	pthread_mutex_lock(&lo->mutex);
	for (p = lo->root.next; p != &lo->root; p = p->next) {
		if (p->ino == st->st_ino && p->dev == st->st_dev) {
			assert(p->refcount > 0);
			ret = p;
			ret->refcount++;
			break;
		}
	}
	pthread_mutex_unlock(&lo->mutex);
	return ret;
}


static struct lo_inode *create_new_inode(int fd, struct fuse_entry_param *e, struct lo_data* lo)
{
	struct lo_inode *inode = NULL;
	struct lo_inode *prev, *next;
	
	inode = calloc(1, sizeof(struct lo_inode));
	if (!inode)
		return NULL;

	inode->refcount = 1;
	inode->fd = fd;
	inode->ino = e->attr.st_ino;
	inode->dev = e->attr.st_dev;

	pthread_mutex_lock(&lo->mutex);
	prev = &lo->root;
	next = prev->next;
	next->prev = inode;
	inode->next = next;
	inode->prev = prev;
	prev->next = inode;
	pthread_mutex_unlock(&lo->mutex);
	return inode;
}

static int fill_entry_param_new_inode(fuse_req_t req, fuse_ino_t parent, int fd, struct fuse_entry_param *e)
{
	int res;
	struct lo_data *lo = lo_data(req);

	memset(e, 0, sizeof(*e));
	e->attr_timeout = lo->timeout;
	e->entry_timeout = lo->timeout;

	res = fstatat(fd, "", &e->attr, AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW);
	if (res == -1)
		return errno;

	e->ino = (uintptr_t) create_new_inode(dup(fd), e, lo);

	if (lo_debug(req))
		fuse_log(FUSE_LOG_DEBUG, "  %" PRIu64 "/%d -> %" PRIu64 "\n",
			(uint64_t)parent, fd, (uint64_t)e->ino);

	return 0;

}

static int lo_do_lookup(fuse_req_t req, fuse_ino_t parent, const char *name,
			 struct fuse_entry_param *e)
{
	int newfd = -1;
	int res;
	int saverr;
	struct lo_data *lo = lo_data(req);
	struct lo_inode *inode;
	char source_path[PATH_MAX + 1];
	char wb_path[PATH_MAX + 512];
	bool upper_only = false;

	memset(e, 0, sizeof(*e));
	e->attr_timeout = lo->timeout;
	e->entry_timeout = lo->timeout;

	if (source_path_for_child(req, parent, name,
	                          source_path, sizeof(source_path)) == -1)
		return errno ? errno : EIO;

	newfd = openat(lo_fd(req, parent), name, O_PATH | O_NOFOLLOW);
	if (newfd == -1 && errno == ENOENT) {
		/*
		 * A deferred regular-file dentry has no HDD namespace entry yet.
		 * Expose it only when durable namespace metadata proves that the
		 * overlay belongs to the logical namespace.
		 */
		if (!created_marker_exists(source_path) &&
		    !(rename_marker_exists(source_path) &&
		      rename_ready_exists(source_path)))
			goto out_err;
		if (writeback_path_for_source(source_path, wb_path,
		                              sizeof(wb_path)) == -1) {
			errno = ENAMETOOLONG;
			goto out_err;
		}
		newfd = open(wb_path, O_PATH | O_NOFOLLOW | O_CLOEXEC);
		if (newfd == -1)
			goto out_err;
		upper_only = true;
	} else if (newfd == -1) {
		goto out_err;
	}

	res = fstatat(newfd, "", &e->attr, AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW);
	if (res == -1)
		goto out_err;
	if (!upper_only)
		apply_writeback_stat_for_fd(newfd, &e->attr);

	inode = lo_find(lo_data(req), &e->attr);
	if (inode) {
		close(newfd);
		newfd = -1;
	} else {
		inode = create_new_inode(newfd, e, lo);
		if (!inode)
			goto out_err;
	}
	e->ino = (uintptr_t) inode;

	if (lo_debug(req))
		fuse_log(FUSE_LOG_DEBUG, "  %" PRIu64 "/%s -> %" PRIu64 "%s\n",
			(uint64_t)parent, name, (uint64_t)e->ino,
			upper_only ? " upper-only" : "");

	return 0;

out_err:
	saverr = errno;
	if (newfd != -1)
		close(newfd);
	return saverr;
}

static void lo_lookup(fuse_req_t req, fuse_ino_t parent, const char *name)
{
	struct fuse_entry_param e;
	int err;

	if (lo_debug(req))
		fuse_log(FUSE_LOG_DEBUG, "lo_lookup(parent=%" PRIu64 ", name=%s)\n",
			parent, name);

	err = lo_do_lookup(req, parent, name, &e);
	if (err)
		fuse_reply_err(req, err);
	else
		fuse_reply_entry(req, &e);
}

static void lo_mknod_symlink(fuse_req_t req, fuse_ino_t parent,
			     const char *name, mode_t mode, dev_t rdev,
			     const char *link)
{
	int res;
	int saverr;
	struct lo_inode *dir = lo_inode(req, parent);
	struct fuse_entry_param e;

	res = mknod_wrapper(dir->fd, name, link, mode, rdev);

	saverr = errno;
	if (res == -1)
		goto out;

	saverr = lo_do_lookup(req, parent, name, &e);
	if (saverr)
		goto out;

	if (lo_debug(req))
		fuse_log(FUSE_LOG_DEBUG, "  %" PRIu64 "/%s -> %" PRIu64 "\n",
			(uint64_t)parent, name, (uint64_t)e.ino);

	fuse_reply_entry(req, &e);
	return;

out:
	fuse_reply_err(req, saverr);
}

static void lo_mknod(fuse_req_t req, fuse_ino_t parent,
		     const char *name, mode_t mode, dev_t rdev)
{
	lo_mknod_symlink(req, parent, name, mode, rdev, NULL);
}

static void lo_mkdir(fuse_req_t req, fuse_ino_t parent, const char *name,
		     mode_t mode)
{
	lo_mknod_symlink(req, parent, name, S_IFDIR | mode, 0, NULL);
}

static void lo_symlink(fuse_req_t req, const char *link,
		       fuse_ino_t parent, const char *name)
{
	lo_mknod_symlink(req, parent, name, S_IFLNK, 0, link);
}

static void lo_link(fuse_req_t req, fuse_ino_t ino, fuse_ino_t parent,
		    const char *name)
{
	int res;
	struct lo_data *lo = lo_data(req);
	struct lo_inode *inode = lo_inode(req, ino);
	struct fuse_entry_param e;
	char procname[64];
	int saverr;

	memset(&e, 0, sizeof(struct fuse_entry_param));
	e.attr_timeout = lo->timeout;
	e.entry_timeout = lo->timeout;

	char source_path[PATH_MAX + 1];
	if (source_path_for_inode(req, ino, source_path, sizeof(source_path)) == 0 &&
	    writeback_exists_for_source(source_path)) {
		if (!writeback_is_clean_for_source(source_path)) {
			fuse_reply_err(req, EBUSY);
			return;
		}
		/* Canonical is current, so dropping the clean overlay is safe. */
		drop_writeback_for_source(source_path);
	}

	sprintf(procname, "/proc/self/fd/%i", inode->fd);
	res = linkat(AT_FDCWD, procname, lo_fd(req, parent), name,
		     AT_SYMLINK_FOLLOW);
	if (res == -1)
		goto out_err;

	res = fstatat(inode->fd, "", &e.attr, AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW);
	if (res == -1)
		goto out_err;

	pthread_mutex_lock(&lo->mutex);
	inode->refcount++;
	pthread_mutex_unlock(&lo->mutex);
	e.ino = (uintptr_t) inode;

	if (lo_debug(req))
		fuse_log(FUSE_LOG_DEBUG, "  %" PRIu64 "/%s -> %" PRIu64 "\n",
			(uint64_t)parent, name,
			(uint64_t)e.ino);

	fuse_reply_entry(req, &e);
	return;

out_err:
	saverr = errno;
	fuse_reply_err(req, saverr);
}

static bool writeback_dir_has_visible_entries(const char *source_dir)
{
	char wb_dir[PATH_MAX + 512];
	char child_source[PATH_MAX + 1];
	DIR *dp;
	struct dirent *de;
	bool found = false;

	if (writeback_path_for_source(source_dir, wb_dir, sizeof(wb_dir)) == -1)
		return false;
	dp = opendir(wb_dir);
	if (!dp)
		return false;

	while ((de = readdir(dp)) != NULL) {
		int n;
		if (is_dot_or_dotdot(de->d_name))
			continue;
		n = snprintf(child_source, sizeof(child_source), "%s/%s",
		             source_dir, de->d_name);
		if (n < 0 || (size_t)n >= sizeof(child_source))
			continue;
		if (created_marker_exists(child_source) ||
		    (rename_marker_exists(child_source) &&
		     rename_ready_exists(child_source))) {
			found = true;
			break;
		}
	}
	closedir(dp);
	return found;
}

static bool writeback_dir_has_any_entries(const char *source_dir)
{
	char wb_dir[PATH_MAX + 512];
	DIR *dp;
	struct dirent *de;
	bool found = false;

	if (writeback_path_for_source(source_dir, wb_dir, sizeof(wb_dir)) == -1)
		return false;
	dp = opendir(wb_dir);
	if (!dp)
		return false;
	while ((de = readdir(dp)) != NULL) {
		if (!is_dot_or_dotdot(de->d_name)) {
			found = true;
			break;
		}
	}
	closedir(dp);
	return found;
}

static void lo_rmdir(fuse_req_t req, fuse_ino_t parent, const char *name)
{
	char source_path[PATH_MAX + 1];
	char wb_dir[PATH_MAX + 512];
	int res;

	if (source_path_for_child(req, parent, name,
	                          source_path, sizeof(source_path)) == -1) {
		fuse_reply_err(req, errno ? errno : EIO);
		return;
	}
	if (writeback_dir_has_visible_entries(source_path)) {
		fuse_reply_err(req, ENOTEMPTY);
		return;
	}

	res = unlinkat(lo_fd(req, parent), name, AT_REMOVEDIR);
	if (res == 0 &&
	    writeback_path_for_source(source_path, wb_dir, sizeof(wb_dir)) == 0)
		(void)rmdir(wb_dir);

	fuse_reply_err(req, res == -1 ? errno : 0);
}

static void lo_rename(fuse_req_t req, fuse_ino_t parent, const char *name,
		      fuse_ino_t newparent, const char *newname,
		      unsigned int flags)
{
	char old_source[PATH_MAX + 1];
	char new_source[PATH_MAX + 1];
	char old_wb[PATH_MAX + 512];
	char new_wb[PATH_MAX + 512];
	char old_state[PATH_MAX + 512];
	char new_state[PATH_MAX + 512];
	char dest_backup[PATH_MAX + 512];
	bool old_has_writeback;
	bool old_dirty = false;
	bool new_has_writeback;
	bool new_dirty = false;
	bool dest_backup_moved = false;
	bool old_canonical_exists = true;
	bool new_canonical_exists = false;
	struct stat canonical_st;
	int saved;
	int res;

	if (flags) {
		fuse_reply_err(req, EINVAL);
		return;
	}
	if (source_path_for_child(req, parent, name,
	                          old_source, sizeof(old_source)) == -1 ||
	    source_path_for_child(req, newparent, newname,
	                          new_source, sizeof(new_source)) == -1) {
		fuse_reply_err(req, EIO);
		return;
	}
	if (strcmp(old_source, new_source) == 0) {
		fuse_reply_err(req, 0);
		return;
	}
	if (policy_for_source(old_source) != policy_for_source(new_source)) {
		fuse_reply_err(req, EXDEV);
		return;
	}
	{
		struct stat old_st;
		if (fstatat(lo_fd(req, parent), name, &old_st,
		            AT_SYMLINK_NOFOLLOW) == 0 &&
		    S_ISDIR(old_st.st_mode) &&
		    writeback_dir_has_any_entries(old_source)) {
			fuse_reply_err(req, EBUSY);
			return;
		}
	}
	if (rename_marker_exists(old_source) ||
	    rename_marker_exists(new_source)) {
		/* Do not build path-keyed rename chains before the prior one commits. */
		fuse_reply_err(req, EBUSY);
		return;
	}

	old_has_writeback = writeback_exists_for_source(old_source);
	if (old_has_writeback) {
		old_dirty = !writeback_is_clean_for_source(old_source);
		if (!old_dirty) {
			/* A clean overlay is redundant with canonical. */
			drop_writeback_for_source(old_source);
			old_has_writeback = false;
		}
	}

	new_has_writeback = writeback_exists_for_source(new_source);
	if (new_has_writeback) {
		new_dirty = !writeback_is_clean_for_source(new_source);
		if (!new_dirty) {
			/* Clean destination cache is disposable if rename replaces it. */
			drop_writeback_for_source(new_source);
			new_has_writeback = false;
		}
	}

	if (old_has_writeback) {
		if (fstatat(lo_fd(req, parent), name, &canonical_st,
		            AT_SYMLINK_NOFOLLOW) == -1) {
			if (errno != ENOENT) {
				fuse_reply_err(req, errno);
				return;
			}
			old_canonical_exists = false;
		}
		if (fstatat(lo_fd(req, newparent), newname, &canonical_st,
		            AT_SYMLINK_NOFOLLOW) == 0) {
			new_canonical_exists = true;
		} else if (errno != ENOENT) {
			fuse_reply_err(req, errno);
			return;
		}

		/*
		 * A dirty upper-only source is valid only while its create intent
		 * exists. Replacing a canonical destination from an upper-only source
		 * needs a lower-inode replacement transaction we do not yet implement;
		 * keep that mixed case conservative.
		 */
		if (!old_canonical_exists && !created_marker_exists(old_source)) {
			fuse_reply_err(req, ENOENT);
			return;
		}
		if (!old_canonical_exists && new_canonical_exists) {
			fuse_reply_err(req, EBUSY);
			return;
		}
	}

	if (!old_has_writeback) {
		res = renameat(lo_fd(req, parent), name,
		               lo_fd(req, newparent), newname);
		if (res == 0) {
			drop_micro_for_source(old_source);
			drop_micro_for_source(new_source);
		}
		fuse_reply_err(req, res == -1 ? errno : 0);
		return;
	}

	/*
	 * Dirty-source rename transaction.
	 *
	 * The durable NVMe marker is written before either namespace is
	 * changed.  The overlay move and HDD rename cannot be atomic with
	 * each other, so a crash may expose either half; startup recovery
	 * uses this marker to roll the operation forward.  The marker is
	 * intentionally left in place after success until the checkpointer
	 * has syncfs()'d the HDD namespace + data.
	 */
	if (writeback_path_for_source(old_source, old_wb, sizeof(old_wb)) == -1 ||
	    writeback_path_for_source(new_source, new_wb, sizeof(new_wb)) == -1 ||
	    checkpoint_state_path_for_source(old_source, old_state,
	                                     sizeof(old_state)) == -1 ||
	    checkpoint_state_path_for_source(new_source, new_state,
	                                     sizeof(new_state)) == -1 ||
	    rename_dest_backup_path_for_source(new_source, dest_backup,
	                                       sizeof(dest_backup)) == -1 ||
	    mkdir_parents(new_wb) == -1 ||
	    mark_rename_source(old_source, new_source) == -1) {
		fuse_reply_err(req, errno ? errno : EIO);
		return;
	}

	/*
	 * Replacing an independently dirty destination needs one extra private
	 * name so a synchronous canonical-rename failure can restore both
	 * authoritative overlays.  The backup lives under rename-state, outside
	 * the user-visible source namespace and outside the checkpointer scan.
	 *
	 * Crash recovery always rolls an in-flight rename forward, so the backup
	 * is needed only for live rollback before the syscall returns an error.
	 * A stale backup while no marker is active is treated conservatively.
	 */
	if (new_dirty) {
		struct stat backup_st;
		if (lstat(dest_backup, &backup_st) == 0) {
			(void)clear_rename_marker(new_source);
			fuse_reply_err(req, EBUSY);
			return;
		}
		if (errno != ENOENT || mkdir_parents(dest_backup) == -1 ||
		    rename(new_wb, dest_backup) == -1) {
			saved = errno ? errno : EIO;
			(void)clear_rename_marker(new_source);
			fuse_reply_err(req, saved);
			return;
		}
		dest_backup_moved = true;
		(void)fsync_parent_path(new_wb);
		(void)fsync_parent_path(dest_backup);
	}

	/*
	 * Move the authoritative source NVMe inode into the destination.  If the
	 * HDD rename then fails synchronously, move it back and restore any dirty
	 * destination backup so the syscall still has normal failure semantics.
	 * If the machine crashes between namespace moves, the durable marker makes
	 * recovery deterministic and rolls the operation forward.
	 */
	if (rename(old_wb, new_wb) == -1) {
		saved = errno;
		if (dest_backup_moved) {
			(void)rename(dest_backup, new_wb);
			(void)fsync_parent_path(new_wb);
			(void)fsync_parent_path(dest_backup);
		}
		(void)clear_rename_marker(new_source);
		fuse_reply_err(req, saved);
		return;
	}

	if (old_canonical_exists) {
		res = renameat(lo_fd(req, parent), name,
		               lo_fd(req, newparent), newname);
	} else {
		/*
		 * Deferred regular-file namespace: there is no HDD dentry to move.
		 * The durable rename intent + moved overlay define the live namespace;
		 * the checkpointer later creates and fsyncs the destination dentry.
		 */
		res = 0;
	}
	if (res == -1) {
		saved = errno;
		(void)rename(new_wb, old_wb);
		if (dest_backup_moved)
			(void)rename(dest_backup, new_wb);
		(void)fsync_parent_path(old_wb);
		if (strcmp(old_wb, new_wb) != 0)
			(void)fsync_parent_path(new_wb);
		if (dest_backup_moved)
			(void)fsync_parent_path(dest_backup);
		(void)clear_rename_marker(new_source);
		fuse_reply_err(req, saved);
		return;
	}

	/*
	 * Any checkpoint generations attached to either pathname are now
	 * invalid.  The rename marker itself forces the destination dirty
	 * until a new post-syncfs generation is written.
	 */
	(void)unlink(old_state);
	(void)unlink(new_state);
	(void)fsync_parent_path(old_wb);
	if (strcmp(old_wb, new_wb) != 0)
		(void)fsync_parent_path(new_wb);

	/*
	 * The old destination is no longer part of the namespace after a
	 * successful canonical rename.  Unlink the private overlay backup before
	 * publishing ready.  A crash before this unlink is harmless: recovery
	 * recognizes the same deterministic backup pathname and discards it after
	 * rolling the transaction forward.
	 */
	if (dest_backup_moved) {
		if (unlink(dest_backup) == 0)
			(void)fsync_parent_path(dest_backup);
		else if (errno != ENOENT && lo_debug(req))
			fuse_log(FUSE_LOG_WARNING,
			         "rename: could not remove destination backup %s: %m\n",
			         dest_backup);
	}

	/*
	 * Publish a second durable NVMe phase only after both moves completed.
	 * The checkpointer must never copy the destination while the canonical
	 * rename is still in flight, because it could otherwise checkpoint into
	 * the soon-to-be-replaced destination inode.
	 *
	 * If publishing this phase fails, the rename itself has still succeeded.
	 * Leave the durable intent marker behind; reboot recovery can safely roll
	 * it forward rather than attempting an unsafe rollback after success.
	 */
	if (mark_rename_ready(new_source) == -1 && lo_debug(req))
		fuse_log(FUSE_LOG_WARNING,
		         "rename: could not publish ready marker for %s: %m\n",
		         new_source);

	drop_micro_for_source(old_source);
	drop_micro_for_source(new_source);
	notify_checkpoint_path(new_source);
	fuse_reply_err(req, 0);
}

static void lo_unlink(fuse_req_t req, fuse_ino_t parent, const char *name)
{
	char source_path[PATH_MAX + 1];
	char rename_old[PATH_MAX + 1];
	char wb_path[PATH_MAX + 512];
	struct stat st;
	bool has_writeback;
	bool has_rename;
	bool canonical_exists = true;
	int res;

	if (source_path_for_child(req, parent, name,
	                          source_path, sizeof(source_path)) == -1) {
		fuse_reply_err(req, EIO);
		return;
	}

	if (fstatat(lo_fd(req, parent), name, &st, AT_SYMLINK_NOFOLLOW) == -1) {
		if (errno != ENOENT) {
			fuse_reply_err(req, errno);
			return;
		}
		canonical_exists = false;
		has_writeback = writeback_exists_for_source(source_path);
		if (!has_writeback ||
		    (!created_marker_exists(source_path) &&
		     !(rename_marker_exists(source_path) &&
		       rename_ready_exists(source_path)))) {
			fuse_reply_err(req, ENOENT);
			return;
		}
		if (writeback_path_for_source(source_path, wb_path,
		                              sizeof(wb_path)) == -1 ||
		    lstat(wb_path, &st) == -1) {
			fuse_reply_err(req, errno ? errno : EIO);
			return;
		}
	}

	/*
	 * If this pathname is the destination of a completed dirty rename, unlink
	 * supersedes that pending checkpoint transaction. Forget durable intent
	 * first so crash recovery cannot resurrect the deleted destination.
	 */
	has_rename = rename_marker_exists(source_path);
	if (has_rename) {
		if (!rename_ready_exists(source_path)) {
			fuse_reply_err(req, EBUSY);
			return;
		}
		if (read_rename_source(source_path, rename_old, sizeof(rename_old)) == -1) {
			fuse_reply_err(req, errno ? errno : EIO);
			return;
		}
		if (created_marker_exists(rename_old) &&
		    clear_created_marker(rename_old) == -1) {
			fuse_reply_err(req, errno);
			return;
		}
		if (clear_rename_marker(source_path) == -1) {
			fuse_reply_err(req, errno);
			return;
		}
		if (clear_rename_ready(source_path) == -1) {
			fuse_reply_err(req, errno);
			return;
		}
		if (clear_rename_dest_backup(source_path) == -1) {
			fuse_reply_err(req, errno);
			return;
		}
	}

	has_writeback = writeback_exists_for_source(source_path);
	if (has_writeback && st.st_nlink > 1) {
		if (!writeback_is_clean_for_source(source_path)) {
			fuse_reply_err(req, EBUSY);
			return;
		}
		drop_writeback_for_source(source_path);
		has_writeback = false;
	}

	/*
	 * Clear create intent before removing the visible namespace entry.
	 * For an upper-only file this is the namespace deletion itself; for a
	 * canonical file it prevents post-crash resurrection before HDD unlink.
	 */
	if (has_writeback && created_marker_exists(source_path) &&
	    clear_created_marker(source_path) == -1) {
		fuse_reply_err(req, errno);
		return;
	}

	if (!canonical_exists) {
		if (has_writeback)
			drop_writeback_for_source(source_path);
		drop_micro_for_source(source_path);
		fuse_reply_err(req, 0);
		return;
	}

	res = unlinkat(lo_fd(req, parent), name, 0);
	if (res == 0) {
		if (has_writeback)
			drop_writeback_for_source(source_path);
		drop_micro_for_source(source_path);
	}

	fuse_reply_err(req, res == -1 ? errno : 0);
}

static void unref_inode(struct lo_data *lo, struct lo_inode *inode, uint64_t n)
{
	if (!inode)
		return;

	pthread_mutex_lock(&lo->mutex);
	assert(inode->refcount >= n);
	inode->refcount -= n;
	if (!inode->refcount) {
		struct lo_inode *prev, *next;

		prev = inode->prev;
		next = inode->next;
		next->prev = prev;
		prev->next = next;

		pthread_mutex_unlock(&lo->mutex);
		close(inode->fd);
		free(inode);

	} else {
		pthread_mutex_unlock(&lo->mutex);
	}
}

static void lo_forget_one(fuse_req_t req, fuse_ino_t ino, uint64_t nlookup)
{
	struct lo_data *lo = lo_data(req);
	struct lo_inode *inode = lo_inode(req, ino);

	if (lo_debug(req)) {
		fuse_log(FUSE_LOG_DEBUG,
			"  forget %" PRIu64 " %" PRIu64 " -%" PRIu64 "\n",
			(uint64_t)ino, inode->refcount,
			nlookup);
	}

	unref_inode(lo, inode, nlookup);
}

static void lo_forget(fuse_req_t req, fuse_ino_t ino, uint64_t nlookup)
{
	lo_forget_one(req, ino, nlookup);
	fuse_reply_none(req);
}

static void lo_forget_multi(fuse_req_t req, size_t count,
				struct fuse_forget_data *forgets)
{
	size_t i;

	for (i = 0; i < count; i++)
		lo_forget_one(req, forgets[i].ino, forgets[i].nlookup);
	fuse_reply_none(req);
}

static void lo_readlink(fuse_req_t req, fuse_ino_t ino)
{
	char buf[PATH_MAX + 1];
	ssize_t res;

	res = readlinkat(lo_fd(req, ino), "", buf, sizeof(buf));
	if (res == -1)
		return (void) fuse_reply_err(req, errno);

	if ((size_t)res == sizeof(buf))
		return (void) fuse_reply_err(req, ENAMETOOLONG);

	buf[(size_t)res] = '\0';

	fuse_reply_readlink(req, buf);
}

#define WRITEBACK_DIR_OFFSET_BASE ((off_t)1 << 60)

struct lo_dirp {
	DIR *dp;
	DIR *wb_dp;
	struct dirent *entry;
	off_t offset;
	bool upper_phase;
};

static struct lo_dirp *lo_dirp(struct fuse_file_info *fi)
{
	return (struct lo_dirp *) (uintptr_t) fi->fh;
}

static void lo_opendir(fuse_req_t req, fuse_ino_t ino, struct fuse_file_info *fi)
{
	int error = ENOMEM;
	struct lo_data *lo = lo_data(req);
	struct lo_dirp *d;
	char source_path[PATH_MAX + 1];
	char wb_path[PATH_MAX + 512];
	int fd = -1;
	int wb_fd = -1;

	d = calloc(1, sizeof(struct lo_dirp));
	if (d == NULL)
		goto out_err;

	fd = openat(lo_fd(req, ino), ".", O_RDONLY | O_DIRECTORY);
	if (fd == -1)
		goto out_errno;

	d->dp = fdopendir(fd);
	if (d->dp == NULL)
		goto out_errno;
	fd = -1;

	/*
	 * Parent directories remain canonical on HDD. If this directory has
	 * deferred regular-file children, their overlays live in the mirrored
	 * writeback directory and are merged into readdir after canonical names.
	 */
	if (source_path_for_inode(req, ino, source_path, sizeof(source_path)) == 0 &&
	    writeback_path_for_source(source_path, wb_path, sizeof(wb_path)) == 0) {
		wb_fd = open(wb_path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
		if (wb_fd != -1) {
			d->wb_dp = fdopendir(wb_fd);
			if (d->wb_dp == NULL)
				close(wb_fd);
			wb_fd = -1;
		}
	}

	d->offset = 0;
	d->entry = NULL;
	d->upper_phase = false;

	fi->fh = (uintptr_t) d;
	if (lo->cache != CACHE_NEVER)
		fi->cache_readdir = 1;
	if (lo->cache == CACHE_ALWAYS)
		fi->keep_cache = 1;
	fuse_reply_open(req, fi);
	return;

out_errno:
	error = errno;
out_err:
	if (fd != -1)
		close(fd);
	if (wb_fd != -1)
		close(wb_fd);
	if (d) {
		if (d->dp)
			closedir(d->dp);
		if (d->wb_dp)
			closedir(d->wb_dp);
		free(d);
	}
	fuse_reply_err(req, error);
}

static int is_dot_or_dotdot(const char *name)
{
	return name[0] == '.' && (name[1] == '\0' ||
				  (name[1] == '.' && name[2] == '\0'));
}

static bool upper_dir_entry_visible(fuse_req_t req, fuse_ino_t ino,
                                    struct lo_dirp *d, const char *name,
                                    struct stat *st)
{
	char source_path[PATH_MAX + 1];

	if (!d->wb_dp || is_dot_or_dotdot(name))
		return false;

	/* Canonical names were already emitted in the lower phase. */
	if (fstatat(dirfd(d->dp), name, st, AT_SYMLINK_NOFOLLOW) == 0)
		return false;
	if (errno != ENOENT)
		return false;

	if (fstatat(dirfd(d->wb_dp), name, st, AT_SYMLINK_NOFOLLOW) == -1 ||
	    !S_ISREG(st->st_mode))
		return false;

	if (source_path_for_child(req, ino, name,
	                          source_path, sizeof(source_path)) == -1)
		return false;

	return created_marker_exists(source_path) ||
	       (rename_marker_exists(source_path) &&
	        rename_ready_exists(source_path));
}

static void lo_do_readdir(fuse_req_t req, fuse_ino_t ino, size_t size,
			  off_t offset, struct fuse_file_info *fi, int plus)
{
	struct lo_dirp *d = lo_dirp(fi);
	char *buf;
	char *p;
	size_t rem = size;
	int err = 0;

	buf = calloc(1, size);
	if (!buf) {
		fuse_reply_err(req, ENOMEM);
		return;
	}
	p = buf;

	if (offset != d->offset) {
		d->entry = NULL;
		if (offset >= WRITEBACK_DIR_OFFSET_BASE) {
			d->upper_phase = true;
			if (d->wb_dp)
				seekdir(d->wb_dp, offset - WRITEBACK_DIR_OFFSET_BASE);
		} else {
			d->upper_phase = false;
			seekdir(d->dp, offset);
		}
		d->offset = offset;
	}

	while (1) {
		size_t entsize;
		off_t nextoff;
		const char *name;
		struct stat upper_st;
		fuse_ino_t entry_ino = 0;

		if (!d->entry) {
			DIR *active = d->upper_phase ? d->wb_dp : d->dp;

			if (!active) {
				if (!d->upper_phase && d->wb_dp) {
					d->upper_phase = true;
					rewinddir(d->wb_dp);
					d->offset = WRITEBACK_DIR_OFFSET_BASE;
					continue;
				}
				break;
			}

			errno = 0;
			d->entry = readdir(active);
			if (!d->entry) {
				if (errno) {
					err = errno;
					goto error;
				}
				if (!d->upper_phase && d->wb_dp) {
					d->upper_phase = true;
					rewinddir(d->wb_dp);
					d->offset = WRITEBACK_DIR_OFFSET_BASE;
					continue;
				}
				break;
			}
		}

		name = d->entry->d_name;
		if (d->upper_phase) {
			if (!upper_dir_entry_visible(req, ino, d, name, &upper_st)) {
				d->entry = NULL;
				continue;
			}
			nextoff = WRITEBACK_DIR_OFFSET_BASE + d->entry->d_off;
		} else {
			nextoff = d->entry->d_off;
		}

		if (plus) {
			struct fuse_entry_param e;
			if (!d->upper_phase && is_dot_or_dotdot(name)) {
				e = (struct fuse_entry_param) {
					.attr.st_ino = d->entry->d_ino,
					.attr.st_mode = d->entry->d_type << 12,
				};
			} else {
				err = lo_do_lookup(req, ino, name, &e);
				if (err)
					goto error;
				entry_ino = e.ino;
			}

			entsize = fuse_add_direntry_plus(req, p, rem, name,
							 &e, nextoff);
		} else {
			struct stat st;
			if (d->upper_phase) {
				st = upper_st;
			} else {
				st = (struct stat) {
					.st_ino = d->entry->d_ino,
					.st_mode = d->entry->d_type << 12,
				};
			}
			entsize = fuse_add_direntry(req, p, rem, name, &st, nextoff);
		}

		if (entsize > rem) {
			if (entry_ino != 0)
				lo_forget_one(req, entry_ino, 1);
			break;
		}

		p += entsize;
		rem -= entsize;
		d->entry = NULL;
		d->offset = nextoff;
	}

error:
	if (err && rem == size)
		fuse_reply_err(req, err);
	else
		fuse_reply_buf(req, buf, size - rem);
	free(buf);
}

static void lo_readdir(fuse_req_t req, fuse_ino_t ino, size_t size,
		       off_t offset, struct fuse_file_info *fi)
{
	lo_do_readdir(req, ino, size, offset, fi, 0);
}

static void lo_readdirplus(fuse_req_t req, fuse_ino_t ino, size_t size,
			   off_t offset, struct fuse_file_info *fi)
{
	lo_do_readdir(req, ino, size, offset, fi, 1);
}

static void lo_releasedir(fuse_req_t req, fuse_ino_t ino, struct fuse_file_info *fi)
{
	struct lo_dirp *d = lo_dirp(fi);
	(void) ino;
	closedir(d->dp);
	if (d->wb_dp)
		closedir(d->wb_dp);
	free(d);
	fuse_reply_err(req, 0);
}

static void lo_tmpfile(fuse_req_t req, fuse_ino_t parent,
		      mode_t mode, struct fuse_file_info *fi)
{
	int fd;
	struct lo_data *lo = lo_data(req);
	struct fuse_entry_param e;
	int err;

	if (lo_debug(req))
		fuse_log(FUSE_LOG_DEBUG, "lo_tmpfile(parent=%" PRIu64 ")\n",
			parent);

	fd = openat(lo_fd(req, parent), ".",
		    (fi->flags | O_TMPFILE) & ~O_NOFOLLOW, mode);
	if (fd == -1)
		return (void) fuse_reply_err(req, errno);

	fi->fh = (uint64_t)fd;
	if (lo->cache == CACHE_NEVER)
		fi->direct_io = 1;
	else if (lo->cache == CACHE_ALWAYS)
		fi->keep_cache = 1;

	/* parallel_direct_writes feature depends on direct_io features.
	   To make parallel_direct_writes valid, need set fi->direct_io
	   in current function. */
	fi->parallel_direct_writes = 1; 
	
	err = fill_entry_param_new_inode(req, parent, fd, &e); 
	if (err)
		fuse_reply_err(req, err);
	else
		fuse_reply_create(req, &e, fi);
}

static void lo_create(fuse_req_t req, fuse_ino_t parent, const char *name,
					      mode_t mode, struct fuse_file_info *fi)
{
	int canonical_fd = -1;
	int fd = -1;
	struct lo_data *lo = lo_data(req);
	struct fuse_entry_param e;
	char source_path[PATH_MAX + 1];
	const char *tier = "canonical";
	int err;

	if (lo_debug(req))
		fuse_log(FUSE_LOG_DEBUG, "lo_create(parent=%" PRIu64 ", name=%s)\n",
			parent, name);

	if (source_path_for_child(req, parent, name,
	                          source_path, sizeof(source_path)) == -1)
		return (void)fuse_reply_err(req, errno ? errno : EIO);

	/*
	 * Deferred-file namespace keeps only regular-file dentries off HDD.
	 * The durable overlay + create marker define the logical file until the
	 * checkpointer creates and fsyncs the canonical final component.
	 * Directories remain canonical, so parent fd/path identity is unchanged.
	 */
	if (lo->defer_file_namespace || source_is_volatile(source_path)) {
		if (fi->flags & O_EXCL) {
			struct stat existing;
			if (fstatat(lo_fd(req, parent), name, &existing,
			            AT_SYMLINK_NOFOLLOW) == 0)
				return (void)fuse_reply_err(req, EEXIST);
			if (errno != ENOENT)
				return (void)fuse_reply_err(req, errno);
			if (writeback_exists_for_source(source_path) &&
			    (created_marker_exists(source_path) ||
			     (rename_marker_exists(source_path) &&
			      rename_ready_exists(source_path))))
				return (void)fuse_reply_err(req, EEXIST);
		}
		fd = create_writeback_for_source(
		    source_path, fi->flags, mode, -1,
		    false /* upper-only dentry requires durable create intent */);
		if (fd == -1)
			return (void)fuse_reply_err(req, errno);
		tier = "nvme-writeback-new-deferred";
	} else {
		canonical_fd = openat(lo_fd(req, parent), name,
			    (fi->flags | O_CREAT) & ~O_NOFOLLOW, mode);
		if (canonical_fd == -1)
			return (void)fuse_reply_err(req, errno);

		fd = canonical_fd;
		{
			int wb_fd = create_writeback_for_source(
			    source_path, fi->flags, mode, canonical_fd,
			    lo->relaxed_create_durability != 0);
			if (wb_fd != -1) {
				fd = wb_fd;
				tier = "nvme-writeback-new";
				close(canonical_fd);
				canonical_fd = -1;
			}
		}
	}

	fi->fh = (uint64_t)fd;
	if (lo->cache == CACHE_NEVER)
		fi->direct_io = 1;
	else if (lo->cache == CACHE_ALWAYS)
		fi->keep_cache = 1;
	fi->parallel_direct_writes = 1;

	err = lo_do_lookup(req, parent, name, &e);
	if (err) {
		close(fd);
		return (void)fuse_reply_err(req, err);
	}

	fi->backing_id = fuse_passthrough_open(req, fd);
	if (fi->backing_id > 0 && !track_backing_id(fd, fi->backing_id)) {
		(void)fuse_passthrough_close(req, fi->backing_id);
		fi->backing_id = 0;
	}

	if (lo_debug(req))
		fuse_log(FUSE_LOG_DEBUG,
		         "lo_create: tier=%s fd=%d backing_id=%d passthrough=%s\n",
		         tier, fd, fi->backing_id,
		         fi->backing_id > 0 ? "yes" : "no");

	fuse_reply_create(req, &e, fi);
}

static void lo_fsyncdir(fuse_req_t req, fuse_ino_t ino, int datasync,
			struct fuse_file_info *fi)
{
	int res;
	int fd = dirfd(lo_dirp(fi)->dp);
	(void) ino;
	if (datasync)
		res = fdatasync(fd);
	else
		res = fsync(fd);
	fuse_reply_err(req, res == -1 ? errno : 0);
}


#define MICRO_MAX_SIZE (2 * 1024 * 1024)
#define WRITEBACK_COPY_MAX_SIZE (4 * 1024 * 1024)

#define MICRO_ROOT "/mnt/morainefs-hot"

#define RANGE_CACHE_ROOT "/var/cache/morainefs/ranges"
#define RANGE_BLOCK_MIN_SIZE (64ULL * 1024ULL)
#define RANGE_BLOCK_MID_SIZE (256ULL * 1024ULL)
#define RANGE_BLOCK_SEQ_SIZE (1024ULL * 1024ULL)
#define RANGE_BLOCK_MAX_SIZE (4ULL * 1024ULL * 1024ULL)
#define RANGE_SEQ_GAP_BYTES (64ULL * 1024ULL)
#define RANGE_CACHE_CAP_BYTES (512ULL * 1024ULL * 1024ULL)

struct range_read_handle {
	int fd;
	off_t last_end;
	unsigned int sequential_streak;
	size_t block_size;
	struct range_read_handle *next;
};

static pthread_mutex_t range_handles_mutex = PTHREAD_MUTEX_INITIALIZER;
static struct range_read_handle *range_handles;

static bool track_range_fd(int fd)
{
	struct range_read_handle *h = malloc(sizeof(*h));
	if (!h)
		return false;
	h->fd = fd;
	h->last_end = -1;
	h->sequential_streak = 0;
	h->block_size = RANGE_BLOCK_MIN_SIZE;
	pthread_mutex_lock(&range_handles_mutex);
	h->next = range_handles;
	range_handles = h;
	pthread_mutex_unlock(&range_handles_mutex);
	return true;
}

static bool take_range_fd(int fd)
{
	struct range_read_handle **pp;
	struct range_read_handle *h;
	bool found = false;

	pthread_mutex_lock(&range_handles_mutex);
	for (pp = &range_handles; (h = *pp) != NULL; pp = &h->next) {
		if (h->fd != fd)
			continue;
		*pp = h->next;
		free(h);
		found = true;
		break;
	}
	pthread_mutex_unlock(&range_handles_mutex);
	return found;
}

static size_t range_block_size_for_read(int fd, off_t offset, size_t size)
{
	struct range_read_handle *h;
	size_t block_size = 0;

	if (offset < 0)
		return 0;

	pthread_mutex_lock(&range_handles_mutex);
	for (h = range_handles; h; h = h->next) {
		if (h->fd != fd)
			continue;

		if (h->last_end >= 0 && offset >= h->last_end &&
		    (uint64_t)(offset - h->last_end) <= RANGE_SEQ_GAP_BYTES)
			h->sequential_streak++;
		else
			h->sequential_streak = 0;

		if (h->sequential_streak >= 4)
			h->block_size = RANGE_BLOCK_MAX_SIZE;
		else if (h->sequential_streak >= 2 &&
		         h->block_size < RANGE_BLOCK_SEQ_SIZE)
			h->block_size = RANGE_BLOCK_SEQ_SIZE;
		else if (h->sequential_streak >= 1 &&
		         h->block_size < RANGE_BLOCK_MID_SIZE)
			h->block_size = RANGE_BLOCK_MID_SIZE;

		h->last_end = offset + (off_t)size;
		block_size = h->block_size;
		break;
	}
	pthread_mutex_unlock(&range_handles_mutex);
	return block_size;
}

struct range_cache_entry {
	char *path;
	size_t size;
	uint64_t last_use;
	struct range_cache_entry *next;
};

static pthread_mutex_t range_cache_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t range_fill_mutex = PTHREAD_MUTEX_INITIALIZER;
static struct range_cache_entry *range_cache_entries;
static size_t range_cache_bytes;
static uint64_t range_cache_clock;

static void range_cache_forget_locked(const char *path)
{
	struct range_cache_entry **pp;
	struct range_cache_entry *e;

	for (pp = &range_cache_entries; (e = *pp) != NULL; pp = &e->next) {
		if (strcmp(e->path, path) != 0)
			continue;
		*pp = e->next;
		if (range_cache_bytes >= e->size)
			range_cache_bytes -= e->size;
		else
			range_cache_bytes = 0;
		free(e->path);
		free(e);
		return;
	}
}

static void range_cache_touch(const char *path, size_t size)
{
	struct range_cache_entry *e;
	struct range_cache_entry *victim;
	struct range_cache_entry **victim_pp;
	struct range_cache_entry **pp;

	pthread_mutex_lock(&range_cache_mutex);
	for (e = range_cache_entries; e; e = e->next) {
		if (strcmp(e->path, path) == 0) {
			e->last_use = ++range_cache_clock;
			pthread_mutex_unlock(&range_cache_mutex);
			return;
		}
	}

	e = calloc(1, sizeof(*e));
	if (!e) {
		pthread_mutex_unlock(&range_cache_mutex);
		return;
	}
	e->path = strdup(path);
	if (!e->path) {
		free(e);
		pthread_mutex_unlock(&range_cache_mutex);
		return;
	}
	e->size = size;
	e->last_use = ++range_cache_clock;
	e->next = range_cache_entries;
	range_cache_entries = e;
	range_cache_bytes += size;

	while (range_cache_bytes > RANGE_CACHE_CAP_BYTES && range_cache_entries) {
		victim = range_cache_entries;
		victim_pp = &range_cache_entries;
		for (pp = &range_cache_entries; *pp; pp = &(*pp)->next) {
			if ((*pp)->last_use < victim->last_use) {
				victim = *pp;
				victim_pp = pp;
			}
		}
		*victim_pp = victim->next;
		(void)unlink(victim->path);
		if (range_cache_bytes >= victim->size)
			range_cache_bytes -= victim->size;
		else
			range_cache_bytes = 0;
		free(victim->path);
		free(victim);
	}
	pthread_mutex_unlock(&range_cache_mutex);
}

static bool same_file_generation(const struct stat *a, const struct stat *b)
{
	return a->st_dev == b->st_dev &&
	       a->st_ino == b->st_ino &&
	       a->st_size == b->st_size &&
	       a->st_mtim.tv_sec == b->st_mtim.tv_sec &&
	       a->st_mtim.tv_nsec == b->st_mtim.tv_nsec &&
	       a->st_ctim.tv_sec == b->st_ctim.tv_sec &&
	       a->st_ctim.tv_nsec == b->st_ctim.tv_nsec;
}

static int range_cache_block_path(const struct stat *st, off_t block_start,
                                  char *out, size_t out_size)
{
	long long mtime_ns =
		(long long)st->st_mtim.tv_sec * 1000000000LL + st->st_mtim.tv_nsec;
	long long ctime_ns =
		(long long)st->st_ctim.tv_sec * 1000000000LL + st->st_ctim.tv_nsec;
	int n = snprintf(
		out, out_size,
		"%s/pid-%ld/%llx-%llx-%llx-%llx-%llx/%016llx.blk",
		RANGE_CACHE_ROOT, (long)getpid(),
		(unsigned long long)st->st_dev,
		(unsigned long long)st->st_ino,
		(unsigned long long)st->st_size,
		(unsigned long long)mtime_ns,
		(unsigned long long)ctime_ns,
		(unsigned long long)block_start);
	return n >= 0 && (size_t)n < out_size ? 0 : -1;
}

static int range_cache_open_block(int source_fd, const struct stat *before,
                                  off_t block_start, size_t block_len,
                                  bool *was_hit)
{
	char path[PATH_MAX + 512];
	char tmp[PATH_MAX + 640];
	struct stat cached_st;
	struct stat after;
	unsigned char *buf = NULL;
	off_t done = 0;
	int fd = -1;
	int out_fd = -1;
	int saved = 0;
	bool locked = false;

	tmp[0] = '\0';
	*was_hit = false;
	if (range_cache_block_path(before, block_start, path, sizeof(path)) == -1)
		return -1;

	/* Fast hit path: never serialize cache reads. */
	fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
	if (fd != -1) {
		if (fstat(fd, &cached_st) == 0 &&
		    S_ISREG(cached_st.st_mode) &&
		    (size_t)cached_st.st_size == block_len) {
			*was_hit = true;
			range_cache_touch(path, block_len);
			return fd;
		}
		close(fd);
		fd = -1;
	}

	/*
	 * Serialize only fills.  The second check is essential because another
	 * FUSE worker may have completed this exact block while we waited.
	 * A single fill stream also avoids turning one sequential HDD access into
	 * competing reads from several worker threads.
	 */
	pthread_mutex_lock(&range_fill_mutex);
	locked = true;

	fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
	if (fd != -1) {
		if (fstat(fd, &cached_st) == 0 &&
		    S_ISREG(cached_st.st_mode) &&
		    (size_t)cached_st.st_size == block_len) {
			*was_hit = true;
			range_cache_touch(path, block_len);
			pthread_mutex_unlock(&range_fill_mutex);
			return fd;
		}
		close(fd);
		fd = -1;
		pthread_mutex_lock(&range_cache_mutex);
		range_cache_forget_locked(path);
		pthread_mutex_unlock(&range_cache_mutex);
		(void)unlink(path);
	}

	if (mkdir_parents(path) == -1)
		goto fail;
	if (snprintf(tmp, sizeof(tmp), "%s.tmp.%ld.%lu",
	             path, (long)getpid(), (unsigned long)pthread_self()) >=
	    (int)sizeof(tmp)) {
		errno = ENAMETOOLONG;
		goto fail;
	}
	out_fd = open(tmp, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW,
	              0600);
	if (out_fd == -1)
		goto fail;

	buf = malloc(256 * 1024);
	if (!buf)
		goto fail;

	while ((size_t)done < block_len) {
		size_t want = block_len - (size_t)done;
		if (want > 256 * 1024)
			want = 256 * 1024;
		ssize_t n = pread(source_fd, buf, want, block_start + done);
		if (n <= 0)
			goto fail;
		ssize_t written = 0;
		while (written < n) {
			ssize_t m = write(out_fd, buf + written, (size_t)(n - written));
			if (m <= 0)
				goto fail;
			written += m;
		}
		done += n;
	}

	free(buf);
	buf = NULL;
	if (fstat(source_fd, &after) == -1 ||
	    !same_file_generation(before, &after)) {
		errno = ESTALE;
		goto fail;
	}

	close(out_fd);
	out_fd = -1;
	if (rename(tmp, path) == -1)
		goto fail;

	fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
	if (fd == -1)
		goto fail_after_publish;
	range_cache_touch(path, block_len);
	pthread_mutex_unlock(&range_fill_mutex);
	return fd;

fail:
	saved = errno ? errno : EIO;
	free(buf);
	if (out_fd != -1)
		close(out_fd);
	if (tmp[0] != '\0')
		(void)unlink(tmp);
	if (locked)
		pthread_mutex_unlock(&range_fill_mutex);
	errno = saved;
	return -1;

fail_after_publish:
	saved = errno ? errno : EIO;
	if (locked)
		pthread_mutex_unlock(&range_fill_mutex);
	errno = saved;
	return -1;
}

static bool range_cache_reply(fuse_req_t req, int source_fd,
                              size_t size, off_t offset, size_t block_size)
{
	struct stat st;
	struct fuse_bufvec out;
	off_t block_start;
	off_t block_end;
	size_t block_len;
	size_t reply_size;
	bool hit;
	int cache_fd;

	if (offset < 0 || size == 0)
		return false;
	if (fstat(source_fd, &st) == -1 || !S_ISREG(st.st_mode) ||
	    st.st_size <= 0)
		return false;
	if (offset >= st.st_size) {
		fuse_reply_buf(req, NULL, 0);
		return true;
	}

	if (block_size < RANGE_BLOCK_MIN_SIZE ||
	    block_size > RANGE_BLOCK_MAX_SIZE)
		return false;

	block_start = (offset / (off_t)block_size) * (off_t)block_size;
	block_end = block_start + (off_t)block_size;
	if (offset + (off_t)size > block_end)
		return false;

	block_len = (size_t)((st.st_size - block_start) < (off_t)block_size ?
	                     (st.st_size - block_start) : (off_t)block_size);
	reply_size = size;
	if (offset + (off_t)reply_size > st.st_size)
		reply_size = (size_t)(st.st_size - offset);

	cache_fd = range_cache_open_block(source_fd, &st, block_start,
	                                  block_len, &hit);
	if (cache_fd == -1)
		return false;

	if (lo_debug(req))
		fuse_log(FUSE_LOG_DEBUG,
		         "range_cache: %s block=%lld block_len=%zu read_off=%lld read_len=%zu\n",
		         hit ? "hit" : "miss",
		         (long long)block_start, block_len,
		         (long long)offset, reply_size);

	out = FUSE_BUFVEC_INIT(reply_size);
	out.buf[0].flags = FUSE_BUF_IS_FD | FUSE_BUF_FD_SEEK;
	out.buf[0].fd = cache_fd;
	out.buf[0].pos = offset - block_start;
	fuse_reply_data(req, &out, FUSE_BUF_SPLICE_MOVE);
	close(cache_fd);
	return true;
}


static int source_path_for_inode(fuse_req_t req, fuse_ino_t ino,
                                 char *out, size_t out_size)
{
	char procname[64];
	ssize_t n;

	if (out_size < 2)
		return -1;
	snprintf(procname, sizeof(procname), "/proc/self/fd/%d", lo_fd(req, ino));
	n = readlink(procname, out, out_size - 1);
	if (n <= 0 || (size_t)n >= out_size - 1)
		return -1;
	out[n] = 0;
	if (out[0] != '/' || strstr(out, " (deleted)") != NULL)
		return -1;

	{
		const char *roots[] = {
			DURABLE_WRITEBACK_ROOT,
			VOLATILE_WRITEBACK_ROOT,
		};
		for (size_t i = 0; i < sizeof(roots) / sizeof(roots[0]); ++i) {
			size_t root_len = strlen(roots[i]);
			if (strncmp(out, roots[i], root_len) == 0 &&
			    out[root_len] == '/') {
				size_t logical_len = strlen(out + root_len);
				memmove(out, out + root_len, logical_len + 1);
				break;
			}
		}
	}
	return 0;
}

static int writeback_path_for_source(const char *source_path,
                                     char *out, size_t out_size)
{
	const char *root = writeback_root_for_source(source_path);
	int n = snprintf(out, out_size, "%s%s", root, source_path);
	return n >= 0 && (size_t)n < out_size ? 0 : -1;
}




static int source_path_for_child(fuse_req_t req, fuse_ino_t parent,
                                 const char *name, char *out, size_t out_size)
{
	char procname[64];
	char parent_path[PATH_MAX + 1];
	ssize_t n;
	int written;

	snprintf(procname, sizeof(procname), "/proc/self/fd/%d",
	         lo_fd(req, parent));
	n = readlink(procname, parent_path, PATH_MAX);
	if (n <= 0 || n >= PATH_MAX)
		return -1;
	parent_path[n] = '\0';
	if (parent_path[0] != '/' || strstr(parent_path, " (deleted)") != NULL)
		return -1;

	written = snprintf(out, out_size, "%s%s%s", parent_path,
	                   strcmp(parent_path, "/") == 0 ? "" : "/", name);
	if (written < 0 || (size_t)written >= out_size) {
		errno = ENAMETOOLONG;
		return -1;
	}
	return 0;
}

static int checkpoint_state_path_for_source(const char *source_path,
                                            char *out, size_t out_size)
{
	const char *root = checkpoint_state_root_for_source(source_path);
	int n = snprintf(out, out_size, "%s%s.state", root, source_path);
	return n >= 0 && (size_t)n < out_size ? 0 : -1;
}

static int micro_path_for_source(const char *source_path,
                                 char *out, size_t out_size)
{
	int n = snprintf(out, out_size, "%s%s", MICRO_ROOT, source_path);
	return n >= 0 && (size_t)n < out_size ? 0 : -1;
}

static bool writeback_exists_for_source(const char *source_path)
{
	char wb_path[PATH_MAX + 512];
	struct stat st;

	if (writeback_path_for_source(source_path, wb_path, sizeof(wb_path)) == -1)
		return false;
	return lstat(wb_path, &st) == 0 && S_ISREG(st.st_mode);
}

static bool writeback_is_clean_for_source(const char *source_path)
{
	char wb_path[PATH_MAX + 512];
	char state_path[PATH_MAX + 512];
	struct stat st;

	/* A pending namespace transaction always makes the destination dirty. */
	if (rename_marker_exists(source_path))
		return false;
	unsigned long long size;
	long long mtime_ns, ctime_ns;
	long long actual_mtime_ns, actual_ctime_ns;
	FILE *f;

	if (writeback_path_for_source(source_path, wb_path, sizeof(wb_path)) == -1 ||
	    checkpoint_state_path_for_source(source_path, state_path,
	                                     sizeof(state_path)) == -1)
		return false;
	if (lstat(wb_path, &st) == -1 || !S_ISREG(st.st_mode))
		return false;

	f = fopen(state_path, "re");
	if (!f)
		return false;
	if (fscanf(f, "%llu %lld %lld", &size, &mtime_ns, &ctime_ns) != 3) {
		fclose(f);
		return false;
	}
	fclose(f);

	actual_mtime_ns = (long long)st.st_mtim.tv_sec * 1000000000LL +
	                  (long long)st.st_mtim.tv_nsec;
	actual_ctime_ns = (long long)st.st_ctim.tv_sec * 1000000000LL +
	                  (long long)st.st_ctim.tv_nsec;
	return size == (unsigned long long)st.st_size &&
	       mtime_ns == actual_mtime_ns &&
	       ctime_ns == actual_ctime_ns;
}

static void drop_writeback_for_source(const char *source_path)
{
	char wb_path[PATH_MAX + 512];
	char state_path[PATH_MAX + 512];

	if (writeback_path_for_source(source_path, wb_path, sizeof(wb_path)) == 0)
		(void)unlink(wb_path);
	if (checkpoint_state_path_for_source(source_path, state_path,
	                                     sizeof(state_path)) == 0)
		(void)unlink(state_path);
}

static void drop_micro_for_source(const char *source_path)
{
	char micro_path[PATH_MAX + sizeof(MICRO_ROOT) + 2];

	if (micro_path_for_source(source_path, micro_path, sizeof(micro_path)) == 0)
		(void)unlink(micro_path);
}

static int lo_open_existing_writeback(fuse_req_t req, fuse_ino_t ino, int flags)
{
	char source_path[PATH_MAX + 1];
	char wb_path[PATH_MAX + 512];

	if (source_path_for_inode(req, ino, source_path, sizeof(source_path)) == -1)
		return -1;
	if (writeback_path_for_source(source_path, wb_path, sizeof(wb_path)) == -1)
		return -1;
	return open(wb_path, flags | O_NOFOLLOW);
}

static void merge_writeback_stat(struct stat *base, const struct stat *wb)
{
	base->st_mode = (base->st_mode & (mode_t)S_IFMT) |
		(wb->st_mode & ~(mode_t)S_IFMT);
	base->st_uid = wb->st_uid;
	base->st_gid = wb->st_gid;
	base->st_size = wb->st_size;
	base->st_blocks = wb->st_blocks;
	base->st_blksize = wb->st_blksize;
	base->st_atim = wb->st_atim;
	base->st_mtim = wb->st_mtim;
	base->st_ctim = wb->st_ctim;
}

static void apply_writeback_stat_for_fd(int canonical_fd, struct stat *base)
{
	char procname[64];
	char source_path[PATH_MAX + 1];
	char wb_path[PATH_MAX + 512];
	struct stat wb;
	ssize_t n;
	int fd;

	if (!S_ISREG(base->st_mode))
		return;
	snprintf(procname, sizeof(procname), "/proc/self/fd/%d", canonical_fd);
	n = readlink(procname, source_path, PATH_MAX);
	if (n <= 0 || n >= PATH_MAX)
		return;
	source_path[n] = '\0';
	if (source_path[0] != '/' || strstr(source_path, " (deleted)") != NULL)
		return;
	if (writeback_path_for_source(source_path, wb_path, sizeof(wb_path)) == -1)
		return;

	fd = open(wb_path, O_PATH | O_NOFOLLOW | O_CLOEXEC);
	if (fd == -1)
		return;
	if (fstat(fd, &wb) == 0 && S_ISREG(wb.st_mode))
		merge_writeback_stat(base, &wb);
	close(fd);
}

static int mkdir_parents(const char *path)
{
	char tmp[PATH_MAX + 512];
	char *p;

	if (strlen(path) >= sizeof(tmp)) {
		errno = ENAMETOOLONG;
		return -1;
	}
	strcpy(tmp, path);
	for (p = tmp + 1; *p; ++p) {
		if (*p != '/')
			continue;
		*p = '\0';
		if (mkdir(tmp, 0755) == -1 && errno != EEXIST) {
			*p = '/';
			return -1;
		}
		*p = '/';
	}
	return 0;
}

static int fsync_parent_path(const char *path)
{
	char parent[PATH_MAX + 512];
	char *slash;
	int fd, ret, saved;

	if (strlen(path) >= sizeof(parent)) {
		errno = ENAMETOOLONG;
		return -1;
	}
	strcpy(parent, path);
	slash = strrchr(parent, '/');
	if (!slash || slash == parent) {
		errno = EINVAL;
		return -1;
	}
	*slash = '\0';
	fd = open(parent, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
	if (fd == -1)
		return -1;
	ret = fsync(fd);
	saved = errno;
	close(fd);
	errno = saved;
	return ret;
}


static int created_marker_path_for_source(const char *source_path,
                                          char *out, size_t out_size)
{
	const char *root = namespace_state_root_for_source(source_path);
	int n = snprintf(out, out_size, "%s%s.created", root, source_path);
	return n >= 0 && (size_t)n < out_size ? 0 : -1;
}

static bool created_marker_exists(const char *source_path)
{
	char marker[PATH_MAX + 512];
	struct stat st;

	if (created_marker_path_for_source(source_path, marker, sizeof(marker)) == -1)
		return false;
	return lstat(marker, &st) == 0 && S_ISREG(st.st_mode);
}

static int mark_created_source(const char *source_path)
{
	char marker[PATH_MAX + 512];
	static const char payload[] = "created-v1\n";
	int fd;
	int saved;

	if (created_marker_path_for_source(source_path, marker, sizeof(marker)) == -1)
		return -1;
	if (mkdir_parents(marker) == -1)
		return -1;

	fd = open(marker, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC | O_NOFOLLOW,
	          0600);
	if (fd == -1)
		return -1;
	if (write(fd, payload, sizeof(payload) - 1) != (ssize_t)(sizeof(payload) - 1) ||
	    fsync(fd) == -1) {
		saved = errno;
		close(fd);
		errno = saved;
		return -1;
	}
	close(fd);
	return fsync_parent_path(marker);
}

static int clear_created_marker(const char *source_path)
{
	char marker[PATH_MAX + 512];

	if (created_marker_path_for_source(source_path, marker, sizeof(marker)) == -1)
		return -1;
	if (unlink(marker) == -1) {
		if (errno == ENOENT)
			return 0;
		return -1;
	}
	return fsync_parent_path(marker);
}

static int create_writeback_for_source(const char *source_path, int flags,
                                       mode_t mode, int canonical_fd,
                                       bool relaxed_create_durability)
{
	char wb_path[PATH_MAX + 512];
	struct stat st;
	int fd;
	int saved;

	if (writeback_path_for_source(source_path, wb_path, sizeof(wb_path)) == -1)
		return -1;

	/*
	 * lo_create() has just established a new canonical namespace entry.
	 * Any path-keyed auxiliary state left here is stale by definition.
	 */
	drop_writeback_for_source(source_path);
	drop_micro_for_source(source_path);

	if (mkdir_parents(wb_path) == -1)
		return -1;

	fd = open(wb_path,
	          (flags | O_CREAT | O_TRUNC | O_CLOEXEC) & ~O_NOFOLLOW,
	          mode);
	if (fd == -1)
		return -1;

	if (fstat(canonical_fd, &st) == 0) {
		if (fchmod(fd, st.st_mode & 07777) == -1 ||
		    fchown(fd, st.st_uid, st.st_gid) == -1) {
			saved = errno;
			close(fd);
			(void)unlink(wb_path);
			errno = saved;
			return -1;
		}
	}

	/*
	 * Strict mode eagerly persists enough private namespace state to recover
	 * a newly-created file even if the application never requests directory
	 * durability.  Artifact mode deliberately does not strengthen those
	 * semantics: the canonical placeholder still exists for the live mount,
	 * but a crash may lose an un-fsynced new artifact just as it may on a
	 * normal filesystem.
	 */
	if (!relaxed_create_durability &&
	    (fsync_parent_path(wb_path) == -1 ||
	     mark_created_source(source_path) == -1)) {
		saved = errno;
		close(fd);
		(void)unlink(wb_path);
		(void)clear_created_marker(source_path);
		errno = saved;
		return -1;
	}

	return fd;
}

static int copy_small_file(int in_fd, int out_fd, off_t size)
{
	char buf[65536];
	off_t off = 0;

	while (off < size) {
		size_t want = (size_t)(size - off);
		if (want > sizeof(buf))
			want = sizeof(buf);
		ssize_t n = pread(in_fd, buf, want, off);
		if (n <= 0)
			return -1;
		ssize_t written = 0;
		while (written < n) {
			ssize_t m = write(out_fd, buf + written, (size_t)(n - written));
			if (m <= 0)
				return -1;
			written += m;
		}
		off += n;
	}
	return 0;
}

static int lo_open_writeback(fuse_req_t req, fuse_ino_t ino, int flags,
                             bool for_write)
{
	struct stat st;
	char source_path[PATH_MAX + 1];
	char wb_path[PATH_MAX + 512];
	char tmp_path[PATH_MAX + 1024];
	char procname[64];
	int fd, in_fd = -1, out_fd = -1;
	int saved;

	if (fstat(lo_fd(req, ino), &st) == -1 || !S_ISREG(st.st_mode))
		return -1;
	if (source_path_for_inode(req, ino, source_path, sizeof(source_path)) == -1)
		return -1;
	if (writeback_path_for_source(source_path, wb_path, sizeof(wb_path)) == -1)
		return -1;

	fd = open(wb_path, flags & ~O_NOFOLLOW);
	if (fd != -1)
		return fd;
	if (!for_write || errno != ENOENT)
		return -1;
	if (!(flags & O_TRUNC) && st.st_size > WRITEBACK_COPY_MAX_SIZE &&
	    !source_is_volatile(source_path))
		return -1;

	if (mkdir_parents(wb_path) == -1)
		return -1;

	snprintf(tmp_path, sizeof(tmp_path), "%s.morainefs.tmp.%ld.%lu",
	         wb_path, (long)getpid(), (unsigned long)pthread_self());
	out_fd = open(tmp_path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC,
	              st.st_mode & 07777);
	if (out_fd == -1)
		return -1;

	if (!(flags & O_TRUNC) && st.st_size > 0) {
		snprintf(procname, sizeof(procname), "/proc/self/fd/%d", lo_fd(req, ino));
		in_fd = open(procname, O_RDONLY | O_CLOEXEC);
		if (in_fd == -1 || copy_small_file(in_fd, out_fd, st.st_size) == -1)
			goto fail;
	}
	if (fchmod(out_fd, st.st_mode & 07777) == -1 ||
	    fchown(out_fd, st.st_uid, st.st_gid) == -1)
		goto fail;
	if (fsync(out_fd) == -1)
		goto fail;
	if (in_fd != -1) {
		close(in_fd);
		in_fd = -1;
	}
	close(out_fd);
	out_fd = -1;

	if (rename(tmp_path, wb_path) == -1)
		goto fail_unlink;
	if (fsync_parent_path(wb_path) == -1)
		return -1;

	return open(wb_path, flags & ~O_NOFOLLOW);

fail:
	saved = errno;
	if (in_fd != -1)
		close(in_fd);
	if (out_fd != -1)
		close(out_fd);
	errno = saved;
fail_unlink:
	(void)unlink(tmp_path);
	return -1;
}

static void notify_checkpoint_path(const char *source_path)
{
	struct sockaddr_un addr = { .sun_family = AF_UNIX };
	const char *checkpoint_socket;
	int sock;

	if (!source_path || source_path[0] != '/')
		return;
	checkpoint_socket = checkpoint_socket_for_source(source_path);
	if (strlen(checkpoint_socket) >= sizeof(addr.sun_path))
		return;
	strcpy(addr.sun_path, checkpoint_socket);
	sock = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
	if (sock == -1)
		return;
	(void)sendto(sock, source_path, strlen(source_path),
	             MSG_DONTWAIT | MSG_NOSIGNAL,
	             (struct sockaddr *)&addr, sizeof(addr));
	close(sock);
}

static void notify_checkpoint_fd(int fd)
{
	char procname[64];
	char path[PATH_MAX + 512];
	ssize_t n;
	const char *roots[] = {
		DURABLE_WRITEBACK_ROOT,
		VOLATILE_WRITEBACK_ROOT,
	};

	snprintf(procname, sizeof(procname), "/proc/self/fd/%d", fd);
	n = readlink(procname, path, sizeof(path) - 1);
	if (n <= 0 || (size_t)n >= sizeof(path) - 1)
		return;
	path[n] = 0;
	for (size_t i = 0; i < sizeof(roots) / sizeof(roots[0]); ++i) {
		size_t root_len = strlen(roots[i]);
		if (strncmp(path, roots[i], root_len) == 0 &&
		    path[root_len] == '/') {
			notify_checkpoint_path(path + root_len);
			return;
		}
	}
}

#define MICRO_ORIGIN_XATTR "user.io_tier.origin_v1"
#define MICRO_ADMIT_SOCKET "/run/morainefs/admit.sock"

struct micro_origin_v1 {
	uint64_t dev;
	uint64_t ino;
	uint64_t size;
	int64_t mtime_sec;
	int64_t mtime_nsec;
	int64_t ctime_sec;
	int64_t ctime_nsec;
};


struct passthrough_handle {
	int fd;
	int backing_id;
	struct passthrough_handle *next;
};

static pthread_mutex_t passthrough_handles_mutex = PTHREAD_MUTEX_INITIALIZER;
static struct passthrough_handle *passthrough_handles;

static bool track_backing_id(int fd, int backing_id)
{
	struct passthrough_handle *h;

	if (backing_id <= 0)
		return true;
	h = malloc(sizeof(*h));
	if (!h)
		return false;
	h->fd = fd;
	h->backing_id = backing_id;

	pthread_mutex_lock(&passthrough_handles_mutex);
	h->next = passthrough_handles;
	passthrough_handles = h;
	pthread_mutex_unlock(&passthrough_handles_mutex);
	return true;
}

static int take_backing_id(int fd)
{
	struct passthrough_handle **pp;
	struct passthrough_handle *h;
	int backing_id = 0;

	pthread_mutex_lock(&passthrough_handles_mutex);
	for (pp = &passthrough_handles; (h = *pp) != NULL; pp = &h->next) {
		if (h->fd != fd)
			continue;
		*pp = h->next;
		backing_id = h->backing_id;
		free(h);
		break;
	}
	pthread_mutex_unlock(&passthrough_handles_mutex);
	return backing_id;
}

static void notify_admission(const char *source_path)
{
	struct sockaddr_un addr = { .sun_family = AF_UNIX };
	char directory[PATH_MAX + 1];
	char *slash;
	int sock;
	size_t len;

	if (!source_path || source_path[0] != '/')
		return;
	len = strnlen(source_path, PATH_MAX + 1);
	if (len == 0 || len > PATH_MAX)
		return;
	memcpy(directory, source_path, len + 1);
	slash = strrchr(directory, '/');
	if (!slash || slash == directory)
		return;
	*slash = '\0';

	if (strlen(MICRO_ADMIT_SOCKET) >= sizeof(addr.sun_path))
		return;
	strcpy(addr.sun_path, MICRO_ADMIT_SOCKET);

	sock = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
	if (sock == -1)
		return;
	(void)sendto(sock, directory, strlen(directory), MSG_DONTWAIT,
	             (struct sockaddr *)&addr, sizeof(addr));
	close(sock);
}

static int lo_open_micro_mirror(fuse_req_t req, fuse_ino_t ino, int flags)
{
	struct stat st;
	char procname[64];
	char source_path[PATH_MAX + 1];
	char mirror_path[PATH_MAX + sizeof(MICRO_ROOT) + 2];
	ssize_t n;
	int fd;

	if ((flags & O_ACCMODE) != O_RDONLY)
		return -1;
	if (fstat(lo_fd(req, ino), &st) == -1 || !S_ISREG(st.st_mode) ||
	    st.st_size > MICRO_MAX_SIZE)
		return -1;

	snprintf(procname, sizeof(procname), "/proc/self/fd/%d", lo_fd(req, ino));
	n = readlink(procname, source_path, PATH_MAX);
	if (n <= 0 || n >= PATH_MAX)
		return -1;
	source_path[n] = '\0';
	if (source_path[0] != '/')
		return -1;

	if (snprintf(mirror_path, sizeof(mirror_path), "%s%s", MICRO_ROOT,
	             source_path) >= (int)sizeof(mirror_path))
		return -1;

	fd = open(mirror_path, flags & ~O_NOFOLLOW);
	if (fd == -1) {
		notify_admission(source_path);
		return -1;
	}

	struct stat mst;
	struct micro_origin_v1 origin;
	ssize_t got = fgetxattr(fd, MICRO_ORIGIN_XATTR, &origin, sizeof(origin));
	if (fstat(fd, &mst) == -1 || mst.st_size != st.st_size ||
	    got != (ssize_t)sizeof(origin) ||
	    origin.dev != (uint64_t)st.st_dev ||
	    origin.ino != (uint64_t)st.st_ino ||
	    origin.size != (uint64_t)st.st_size ||
	    origin.mtime_sec != (int64_t)st.st_mtim.tv_sec ||
	    origin.mtime_nsec != (int64_t)st.st_mtim.tv_nsec ||
	    origin.ctime_sec != (int64_t)st.st_ctim.tv_sec ||
	    origin.ctime_nsec != (int64_t)st.st_ctim.tv_nsec) {
		close(fd);
		notify_admission(source_path);
		return -1;
	}
	return fd;
}

static void lo_open(fuse_req_t req, fuse_ino_t ino, struct fuse_file_info *fi)
{
	int fd;
	char buf[64];
	struct lo_data *lo = lo_data(req);

	if (lo_debug(req))
		fuse_log(FUSE_LOG_DEBUG, "lo_open(ino=%" PRIu64 ", flags=%d)\n",
			ino, fi->flags);

	/* With writeback cache, kernel may send read requests even
	   when userspace opened write-only */
	if (lo->writeback && (fi->flags & O_ACCMODE) == O_WRONLY) {
		fi->flags &= ~O_ACCMODE;
		fi->flags |= O_RDWR;
	}

	/* With writeback cache, O_APPEND is handled by the kernel.
	   This breaks atomicity (since the file may change in the
	   underlying filesystem, so that the kernel's idea of the
	   end of the file isn't accurate anymore). In this example,
	   we just accept that. A more rigorous filesystem may want
	   to return an error here */
	if (lo->writeback && (fi->flags & O_APPEND))
		fi->flags &= ~O_APPEND;

	bool mutating = (fi->flags & O_ACCMODE) != O_RDONLY;
	fd = lo_open_writeback(req, ino, fi->flags, mutating);
	const char *tier = "nvme-writeback";
	if (fd == -1 && !mutating) {
		fd = lo_open_micro_mirror(req, ino, fi->flags);
		tier = "zram";
	}
	if (fd == -1) {
		tier = "canonical";
		sprintf(buf, "/proc/self/fd/%i", lo_fd(req, ino));
		fd = open(buf, fi->flags & ~O_NOFOLLOW);
	}
	if (fd == -1)
		return (void) fuse_reply_err(req, errno);

	fi->fh = (uint64_t)fd;
	bool use_passthrough = true;
	struct stat opened_st;
	if (lo->range_reads && !mutating && strcmp(tier, "canonical") == 0 &&
	    fstat(fd, &opened_st) == 0 && S_ISREG(opened_st.st_mode) &&
	    opened_st.st_size > MICRO_MAX_SIZE)
		use_passthrough = false;

	if (!use_passthrough && !track_range_fd(fd))
		use_passthrough = true;

	fi->backing_id = use_passthrough ? fuse_passthrough_open(req, fd) : 0;
	if (fi->backing_id > 0 && !track_backing_id(fd, fi->backing_id)) {
		(void)fuse_passthrough_close(req, fi->backing_id);
		fi->backing_id = 0;
	}
	if (lo_debug(req))
		fuse_log(FUSE_LOG_DEBUG,
			 "lo_open: tier=%s fd=%d backing_id=%d passthrough=%s\n",
			 tier, fd, fi->backing_id,
			 fi->backing_id > 0 ? "yes" : "no");
	if (lo->cache == CACHE_NEVER)
		fi->direct_io = 1;
	else if (lo->cache == CACHE_ALWAYS)
		fi->keep_cache = 1;

        /* Enable direct_io when open has flags O_DIRECT to enjoy the feature
        parallel_direct_writes (i.e., to get a shared lock, not exclusive lock,
	for writes to the same file in the kernel). */
	if (fi->flags & O_DIRECT)
		fi->direct_io = 1;

	/* parallel_direct_writes feature depends on direct_io features.
	   To make parallel_direct_writes valid, need set fi->direct_io
	   in current function. */
	fi->parallel_direct_writes = 1;

	fuse_reply_open(req, fi);
}

static void lo_release(fuse_req_t req, fuse_ino_t ino, struct fuse_file_info *fi)
{
	int backing_id;

	(void) ino;
	if ((fi->flags & O_ACCMODE) != O_RDONLY)
		notify_checkpoint_fd((int)fi->fh);
	(void)take_range_fd((int)fi->fh);
	backing_id = take_backing_id((int)fi->fh);
	if (backing_id > 0)
		(void)fuse_passthrough_close(req, backing_id);
	close((int)fi->fh);
	fuse_reply_err(req, 0);
}

static void lo_flush(fuse_req_t req, fuse_ino_t ino, struct fuse_file_info *fi)
{
	int res;
	(void) ino;
	res = close(dup((int)fi->fh));
	fuse_reply_err(req, res == -1 ? errno : 0);
}

static void lo_fsync(fuse_req_t req, fuse_ino_t ino, int datasync,
		     struct fuse_file_info *fi)
{
	int res;
	(void) ino;
	if (datasync)
		res = fdatasync((int)fi->fh);
	else
		res = fsync((int)fi->fh);
	if (res != -1 && (fi->flags & O_ACCMODE) != O_RDONLY)
		notify_checkpoint_fd((int)fi->fh);
	fuse_reply_err(req, res == -1 ? errno : 0);
}

static void lo_read(fuse_req_t req, fuse_ino_t ino, size_t size,
		    off_t offset, struct fuse_file_info *fi)
{
	struct fuse_bufvec buf = FUSE_BUFVEC_INIT(size);

	if (lo_debug(req))
		fuse_log(FUSE_LOG_DEBUG, "lo_read(ino=%" PRIu64 ", size=%zu, "
			"off=%jd)\n", (uint64_t)ino, size, (intmax_t)offset);

	size_t range_block_size =
		range_block_size_for_read((int)fi->fh, offset, size);
	if (range_block_size != 0 &&
	    range_cache_reply(req, (int)fi->fh, size, offset, range_block_size))
		return;

	buf.buf[0].flags = FUSE_BUF_IS_FD | FUSE_BUF_FD_SEEK;
	buf.buf[0].fd = (int)fi->fh;
	buf.buf[0].pos = offset;

	fuse_reply_data(req, &buf, FUSE_BUF_SPLICE_MOVE);
}

static void lo_write_buf(fuse_req_t req, fuse_ino_t ino,
			 struct fuse_bufvec *in_buf, off_t off,
			 struct fuse_file_info *fi)
{
	(void) ino;
	ssize_t res;
	struct fuse_bufvec out_buf = FUSE_BUFVEC_INIT(fuse_buf_size(in_buf));

	out_buf.buf[0].flags = FUSE_BUF_IS_FD | FUSE_BUF_FD_SEEK;
	out_buf.buf[0].fd = (int)fi->fh;
	out_buf.buf[0].pos = off;

	if (lo_debug(req))
		fuse_log(FUSE_LOG_DEBUG, "lo_write(ino=%" PRIu64 ", size=%zu, off=%jd)\n",
			(uint64_t)ino, out_buf.buf[0].size, (intmax_t)off);

	res = fuse_buf_copy(&out_buf, in_buf, 0);
	if(res < 0)
		fuse_reply_err(req, (int)-res);
	else
		fuse_reply_write(req, (size_t) res);
}

static void lo_statfs(fuse_req_t req, fuse_ino_t ino)
{
	int res;
	struct statvfs stbuf;

	res = fstatvfs(lo_fd(req, ino), &stbuf);
	if (res == -1)
		fuse_reply_err(req, errno);
	else
		fuse_reply_statfs(req, &stbuf);
}

static void lo_fallocate(fuse_req_t req, fuse_ino_t ino, int mode,
			 off_t offset, off_t length, struct fuse_file_info *fi)
{
	int err = EOPNOTSUPP;
	(void) ino;
	(void) mode;
	(void) offset;
	(void) length;
	(void) fi;

#ifdef HAVE_FALLOCATE
	err = fallocate(fi->fh, mode, offset, length);
	if (err < 0)
		err = errno;

#elif defined(HAVE_POSIX_FALLOCATE)
	if (mode) {
		fuse_reply_err(req, EOPNOTSUPP);
		return;
	}

	err = posix_fallocate(fi->fh, offset, length);
#endif

	fuse_reply_err(req, err);
}

static void lo_flock(fuse_req_t req, fuse_ino_t ino, struct fuse_file_info *fi,
		     int op)
{
	int res;
	(void) ino;

	res = flock((int)fi->fh, op);

	fuse_reply_err(req, res == -1 ? errno : 0);
}

static void lo_getxattr(fuse_req_t req, fuse_ino_t ino, const char *name,
			size_t size)
{
	char *value = NULL;
	char procname[64];
	struct lo_inode *inode = lo_inode(req, ino);
	ssize_t ret;
	int saverr;

	saverr = ENOSYS;
	if (!lo_data(req)->xattr)
		goto out;

	if (lo_debug(req)) {
		fuse_log(FUSE_LOG_DEBUG, "lo_getxattr(ino=%" PRIu64 ", name=%s size=%zu)\n",
			(uint64_t)ino, name, size);
	}

	sprintf(procname, "/proc/self/fd/%i", inode->fd);

	if (size) {
		value = malloc(size);
		if (!value)
			goto out_err;

		ret = getxattr(procname, name, value, size);
		if (ret == -1)
			goto out_err;
		saverr = 0;
		if (ret == 0)
			goto out;

		fuse_reply_buf(req, value, (size_t)ret);
	} else {
		ret = getxattr(procname, name, NULL, 0);
		if (ret == -1)
			goto out_err;

		fuse_reply_xattr(req, (size_t)ret);
	}
out_free:
	free(value);
	return;

out_err:
	saverr = errno;
out:
	fuse_reply_err(req, saverr);
	goto out_free;
}

static void lo_listxattr(fuse_req_t req, fuse_ino_t ino, size_t size)
{
	char *value = NULL;
	char procname[64];
	struct lo_inode *inode = lo_inode(req, ino);
	ssize_t ret;
	int saverr;

	saverr = ENOSYS;
	if (!lo_data(req)->xattr)
		goto out;

	if (lo_debug(req)) {
		fuse_log(FUSE_LOG_DEBUG, "lo_listxattr(ino=%" PRIu64 ", size=%zu)\n",
			(uint64_t)ino, size);
	}

	sprintf(procname, "/proc/self/fd/%i", inode->fd);

	if (size) {
		value = malloc(size);
		if (!value)
			goto out_err;

		ret = listxattr(procname, value, size);
		if (ret == -1)
			goto out_err;
		saverr = 0;
		if (ret == 0)
			goto out;

		fuse_reply_buf(req, value, (size_t)ret);
	} else {
		ret = listxattr(procname, NULL, 0);
		if (ret == -1)
			goto out_err;

		fuse_reply_xattr(req, (size_t)ret);
	}
out_free:
	free(value);
	return;

out_err:
	saverr = errno;
out:
	fuse_reply_err(req, saverr);
	goto out_free;
}

static void lo_setxattr(fuse_req_t req, fuse_ino_t ino, const char *name,
			const char *value, size_t size, int flags)
{
	char procname[64];
	struct lo_inode *inode = lo_inode(req, ino);
	ssize_t ret;
	int saverr;

	saverr = ENOSYS;
	if (!lo_data(req)->xattr)
		goto out;

	if (lo_debug(req)) {
		fuse_log(FUSE_LOG_DEBUG, "lo_setxattr(ino=%" PRIu64 ", name=%s value=%s size=%zu)\n",
			(uint64_t)ino, name, value, size);
	}

	sprintf(procname, "/proc/self/fd/%i", inode->fd);

	ret = setxattr(procname, name, value, size, flags);
	saverr = ret == -1 ? errno : 0;

out:
	fuse_reply_err(req, saverr);
}

static void lo_removexattr(fuse_req_t req, fuse_ino_t ino, const char *name)
{
	char procname[64];
	struct lo_inode *inode = lo_inode(req, ino);
	ssize_t ret;
	int saverr;

	saverr = ENOSYS;
	if (!lo_data(req)->xattr)
		goto out;

	if (lo_debug(req)) {
		fuse_log(FUSE_LOG_DEBUG, "lo_removexattr(ino=%" PRIu64 ", name=%s)\n",
			(uint64_t)ino, name);
	}

	sprintf(procname, "/proc/self/fd/%i", inode->fd);

	ret = removexattr(procname, name);
	saverr = ret == -1 ? errno : 0;

out:
	fuse_reply_err(req, saverr);
}

#ifdef HAVE_COPY_FILE_RANGE
static void lo_copy_file_range(fuse_req_t req, fuse_ino_t ino_in, off_t off_in,
			       struct fuse_file_info *fi_in,
			       fuse_ino_t ino_out, off_t off_out,
			       struct fuse_file_info *fi_out, size_t len,
			       int flags)
{
	ssize_t res;

	if (lo_debug(req))
		fuse_log(FUSE_LOG_DEBUG,
			"lo_copy_file_range(ino=%" PRIu64 "/fd=%" PRIu64 ", "
			"off=%jd, ino=%" PRIu64 "/fd=%" PRIu64 ", "
			"off=%jd, size=%zu, flags=0x%x)\n",
			(uint64_t)ino_in, (uint64_t)fi_in->fh, (intmax_t)off_in,
			(uint64_t)ino_out, (uint64_t)fi_out->fh, (intmax_t)off_out,
			len, flags);

	res = copy_file_range(fi_in->fh, &off_in, fi_out->fh, &off_out, len,
			      flags);
	if (res < 0)
		fuse_reply_err(req, errno);
	else
		fuse_reply_write(req, res);
}
#endif

static void lo_lseek(fuse_req_t req, fuse_ino_t ino, off_t off, int whence,
		     struct fuse_file_info *fi)
{
	off_t res;

	(void)ino;
	res = lseek((int)fi->fh, off, whence);
	if (res != -1)
		fuse_reply_lseek(req, res);
	else
		fuse_reply_err(req, errno);
}

static const struct fuse_lowlevel_ops lo_oper = {
	.init		= lo_init,
	.destroy	= lo_destroy,
	.lookup		= lo_lookup,
	.mkdir		= lo_mkdir,
	.mknod		= lo_mknod,
	.symlink	= lo_symlink,
	.link		= lo_link,
	.unlink		= lo_unlink,
	.rmdir		= lo_rmdir,
	.rename		= lo_rename,
	.forget		= lo_forget,
	.forget_multi	= lo_forget_multi,
	.getattr	= lo_getattr,
	.setattr	= lo_setattr,
	.readlink	= lo_readlink,
	.opendir	= lo_opendir,
	.readdir	= lo_readdir,
	.readdirplus	= lo_readdirplus,
	.releasedir	= lo_releasedir,
	.fsyncdir	= lo_fsyncdir,
	.create		= lo_create,
	.tmpfile	= lo_tmpfile,
	.open		= lo_open,
	.release	= lo_release,
	.flush		= lo_flush,
	.fsync		= lo_fsync,
	.read		= lo_read,
	.write_buf      = lo_write_buf,
	.statfs		= lo_statfs,
	.fallocate	= lo_fallocate,
	.flock		= lo_flock,
	.getxattr	= lo_getxattr,
	.listxattr	= lo_listxattr,
	.setxattr	= lo_setxattr,
	.removexattr	= lo_removexattr,
#ifdef HAVE_COPY_FILE_RANGE
	.copy_file_range = lo_copy_file_range,
#endif
	.lseek		= lo_lseek,
};


static int parse_policy_kind(const char *word, enum path_policy_kind *kind)
{
	if (strcmp(word, "durable") == 0) {
		*kind = PATH_POLICY_DURABLE;
		return 0;
	}
	if (strcmp(word, "volatile") == 0 || strcmp(word, "async") == 0) {
		*kind = PATH_POLICY_VOLATILE;
		return 0;
	}
	errno = EINVAL;
	return -1;
}

static int load_policy_file(const char *path)
{
	FILE *f;
	char line[PATH_MAX + 128];
	unsigned int lineno = 0;

	if (!path)
		return 0;
	f = fopen(path, "re");
	if (!f)
		return -1;

	while (fgets(line, sizeof(line), f)) {
		char *save = NULL;
		char *first;
		char *second;
		char *extra;
		enum path_policy_kind kind;
		++lineno;

		first = strtok_r(line, " \t\r\n", &save);
		if (!first || first[0] == '#')
			continue;
		second = strtok_r(NULL, " \t\r\n", &save);
		extra = strtok_r(NULL, " \t\r\n", &save);
		if (!second || (extra && extra[0] != '#')) {
			fprintf(stderr,
			        "policy_file %s:%u: expected '<mode> <prefix>' or 'default <mode>'\n",
			        path, lineno);
			errno = EINVAL;
			fclose(f);
			return -1;
		}

		if (strcmp(first, "default") == 0) {
			if (parse_policy_kind(second, &kind) == -1) {
				fprintf(stderr, "policy_file %s:%u: unknown mode '%s'\n",
				        path, lineno, second);
				fclose(f);
				return -1;
			}
			g_default_policy = kind;
			continue;
		}

		if (parse_policy_kind(first, &kind) == -1) {
			fprintf(stderr, "policy_file %s:%u: unknown mode '%s'\n",
			        path, lineno, first);
			fclose(f);
			return -1;
		}
		if (second[0] != '/') {
			fprintf(stderr, "policy_file %s:%u: prefix must be absolute\n",
			        path, lineno);
			errno = EINVAL;
			fclose(f);
			return -1;
		}
		if (g_policy_rule_count >= MAX_POLICY_RULES) {
			fprintf(stderr, "policy_file %s:%u: too many rules (max %d)\n",
			        path, lineno, MAX_POLICY_RULES);
			errno = E2BIG;
			fclose(f);
			return -1;
		}

		size_t n = strlen(second);
		while (n > 1 && second[n - 1] == '/')
			second[--n] = 0;
		if (n >= sizeof(g_policy_rules[0].prefix)) {
			errno = ENAMETOOLONG;
			fclose(f);
			return -1;
		}
		strcpy(g_policy_rules[g_policy_rule_count].prefix, second);
		g_policy_rules[g_policy_rule_count].kind = kind;
		++g_policy_rule_count;
	}
	if (ferror(f)) {
		int saved = errno ? errno : EIO;
		fclose(f);
		errno = saved;
		return -1;
	}
	fclose(f);
	return 0;
}

int main(int argc, char *argv[])
{
	struct fuse_args args = FUSE_ARGS_INIT(argc, argv);
	struct fuse_session *se;
	struct fuse_cmdline_opts opts;
	struct fuse_loop_config *config;
	struct lo_data lo = { .debug = 0,
	                      .writeback = 0 };
	int ret = -1;

	/* Don't mask creation mode, kernel already did that */
	umask(0);

	pthread_mutex_init(&lo.mutex, NULL);
	lo.root.next = lo.root.prev = &lo.root;
	lo.root.fd = -1;
	lo.cache = CACHE_NORMAL;

	if (fuse_parse_cmdline(&args, &opts) != 0)
		return 1;
	if (opts.show_help) {
		printf("usage: %s [options] <mountpoint>\n\n", argv[0]);
		fuse_cmdline_help();
		fuse_lowlevel_help();
		passthrough_ll_help();
		ret = 0;
		goto err_out1;
	} else if (opts.show_version) {
		printf("FUSE library version %s\n", fuse_pkgversion());
		fuse_lowlevel_version();
		ret = 0;
		goto err_out1;
	}

	if(opts.mountpoint == NULL) {
		printf("usage: %s [options] <mountpoint>\n", argv[0]);
		printf("       %s --help\n", argv[0]);
		ret = 1;
		goto err_out1;
	}

	if (fuse_opt_parse(&args, &lo, lo_opts, NULL)== -1)
		return 1;
	if (load_policy_file(lo.policy_file) == -1) {
		fuse_log(FUSE_LOG_ERR, "failed to load policy file %s: %m\n",
		         lo.policy_file ? lo.policy_file : "(null)");
		return 1;
	}

	lo.debug = opts.debug;
	lo.root.refcount = 2;
	if (lo.source) {
		struct stat stat;
		int res;

		res = lstat(lo.source, &stat);
		if (res == -1) {
			fuse_log(FUSE_LOG_ERR, "failed to stat source (\"%s\"): %m\n",
				 lo.source);
			exit(1);
		}
		if (!S_ISDIR(stat.st_mode)) {
			fuse_log(FUSE_LOG_ERR, "source is not a directory\n");
			exit(1);
		}

	} else {
		lo.source = strdup("/");
		if(!lo.source) {
			fuse_log(FUSE_LOG_ERR, "fuse: memory allocation failed\n");
			exit(1);
		}
	}
	if (!lo.timeout_set) {
		switch (lo.cache) {
		case CACHE_NEVER:
			lo.timeout = 0.0;
			break;

		case CACHE_NORMAL:
			lo.timeout = 1.0;
			break;

		case CACHE_ALWAYS:
			lo.timeout = 86400.0;
			break;
		}
	} else if (lo.timeout < 0) {
		fuse_log(FUSE_LOG_ERR, "timeout is negative (%lf)\n",
			 lo.timeout);
		exit(1);
	}

	lo.root.fd = open(lo.source, O_PATH);
	if (lo.root.fd == -1) {
		fuse_log(FUSE_LOG_ERR, "open(\"%s\", O_PATH): %m\n",
			 lo.source);
		exit(1);
	}

	se = fuse_session_new(&args, &lo_oper, sizeof(lo_oper), &lo);
	if (se == NULL)
	    goto err_out1;

	if (fuse_set_signal_handlers(se) != 0)
	    goto err_out2;

	if (fuse_session_mount(se, opts.mountpoint) != 0)
	    goto err_out3;

	fuse_daemonize(opts.foreground);

	/* Block until ctrl+c or fusermount -u */
	if (opts.singlethread)
		ret = fuse_session_loop(se);
	else {
		config = fuse_loop_cfg_create();
		fuse_loop_cfg_set_clone_fd(config, (unsigned int)opts.clone_fd);
		fuse_loop_cfg_set_max_threads(config, opts.max_threads);
		ret = fuse_session_loop_mt(se, config);
		fuse_loop_cfg_destroy(config);
		config = NULL;
	}

	fuse_session_unmount(se);
err_out3:
	fuse_remove_signal_handlers(se);
err_out2:
	fuse_session_destroy(se);
err_out1:
	free(opts.mountpoint);
	fuse_opt_free_args(&args);

	if (lo.root.fd >= 0)
		close(lo.root.fd);

	free(lo.source);
	free(lo.policy_file);
	return ret ? 1 : 0;
}
