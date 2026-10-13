/* Fresh directory-I/O diagnostic, not a disk-usage scanner or an index.
 *
 * Compare native order with a bounded physical-extent reorder window. Each
 * invocation discovers its own namespace, reads authoritative directory
 * metadata, and opens no raw devices. File allocation and dq's coverage
 * certificate are deliberately absent, so these times are never dq results.
 *
 * Build: cc -O2 -pthread -Wall -Wextra dev/research/dq-dir-io-probe.c -o /tmp/PROBE
 * Physical variant: add -DPHYSICAL_ORDER=1; select window via -DWINDOW=8192.
 * Both phases and namespace discovery are included in external wall time.
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/fiemap.h>
#include <linux/fs.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#ifndef WINDOW
#define WINDOW 8192
#endif
#ifndef PHYSICAL_ORDER
#define PHYSICAL_ORDER 0
#endif

struct linux_dirent64 {
    uint64_t inode;
    int64_t offset;
    unsigned short length;
    unsigned char type;
    char name[];
};
struct work { char *path; int fd; uint64_t physical; };
struct counters { uint64_t directories, files, entries, errors, mappings, checksum; };
struct pool {
    pthread_mutex_t lock;
    pthread_cond_t changed, finished;
    unsigned generation, completed, threads;
    bool stopping;
    int phase;
    atomic_size_t next;
    struct work *work;
    size_t count;
    dev_t device;
    char **pending;
    size_t pending_count, pending_capacity;
};
struct worker { struct pool *pool; struct counters count; };

static void die(const char *reason) { perror(reason); exit(1); }
static void *allocate(size_t size) {
    void *memory = malloc(size);
    if (!memory) die("malloc");
    return memory;
}
static uint64_t hash_name(const char *name) {
    uint64_t value = UINT64_C(14695981039346656037);
    for (const unsigned char *p = (const unsigned char *)name; *p; ++p)
        value = (value ^ *p) * UINT64_C(1099511628211);
    return value;
}
static char *child_path(const char *parent, const char *name) {
    size_t first = strlen(parent), second = strlen(name);
    char *path = allocate(first + second + 2);
    memcpy(path, parent, first);
    path[first] = '/';
    memcpy(path + first + 1, name, second + 1);
    return path;
}
static void append_paths(struct pool *pool, char **paths, size_t count) {
    if (!count) return;
    pthread_mutex_lock(&pool->lock);
    size_t needed = pool->pending_count + count;
    if (needed > pool->pending_capacity) {
        size_t capacity = needed * 2;
        void *memory = realloc(pool->pending, capacity * sizeof(char *));
        if (!memory) die("realloc pending");
        pool->pending = memory;
        pool->pending_capacity = capacity;
    }
    memcpy(pool->pending + pool->pending_count, paths, count * sizeof(char *));
    pool->pending_count = needed;
    pthread_mutex_unlock(&pool->lock);
}
static void prepare(struct pool *pool, struct work *work, struct counters *count) {
    work->physical = UINT64_MAX;
    work->fd = open(work->path, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    if (work->fd < 0) { ++count->errors; return; }
    struct stat metadata;
    if (fstat(work->fd, &metadata) || metadata.st_dev != pool->device) {
        ++count->errors;
        close(work->fd); work->fd = -1;
        return;
    }
    if (PHYSICAL_ORDER) {
        /* UAPI fiemap has a flexible extent array. Allocate its full payload. */
        unsigned char storage[sizeof(struct fiemap) + sizeof(struct fiemap_extent)]
            __attribute__((aligned(8))) = {0};
        struct fiemap *mapping = (struct fiemap *)storage;
        mapping->fm_length = 4096;
        mapping->fm_extent_count = 1;
        if (ioctl(work->fd, FS_IOC_FIEMAP, mapping) == 0 && mapping->fm_mapped_extents
            && !(mapping->fm_extents[0].fe_flags & (FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_DELALLOC))) {
            work->physical = mapping->fm_extents[0].fe_physical;
            ++count->mappings;
        }
    }
}
static void read_names(struct pool *pool, struct work *work, struct counters *count) {
    if (work->fd < 0) return;
    unsigned char buffer[65536] __attribute__((aligned(8)));
    char **children = NULL;
    size_t child_count = 0, child_capacity = 0;
    ++count->directories;
    count->checksum += hash_name(work->path);
    for (;;) {
        ssize_t bytes = syscall(SYS_getdents64, work->fd, buffer, sizeof(buffer));
        if (bytes < 0 && errno == EINTR) continue;
        if (bytes < 0) { ++count->errors; break; }
        if (!bytes) break;
        size_t position = 0;
        while (position < (size_t)bytes) {
            struct linux_dirent64 *entry = (struct linux_dirent64 *)(buffer + position);
            size_t header = offsetof(struct linux_dirent64, name);
            if ((size_t)bytes - position < header + 1 || entry->length < header + 1
                || entry->length > (size_t)bytes - position
                || !memchr(entry->name, '\0', entry->length - header)) {
                ++count->errors;
                position = (size_t)bytes;
                break;
            }
            position += entry->length;
            if (!strcmp(entry->name, ".") || !strcmp(entry->name, "..")) continue;
            ++count->entries;
            count->checksum += entry->inode ^ hash_name(entry->name);
            unsigned char type = entry->type;
            if (type == DT_UNKNOWN) {
                struct stat item;
                if (fstatat(work->fd, entry->name, &item, AT_SYMLINK_NOFOLLOW)) {
                    ++count->errors; continue;
                }
                type = S_ISDIR(item.st_mode) ? DT_DIR : S_ISREG(item.st_mode) ? DT_REG : DT_UNKNOWN;
            }
            if (type == DT_REG) ++count->files;
            if (type != DT_DIR) continue;
            if (child_count == child_capacity) {
                child_capacity = child_capacity ? child_capacity * 2 : 16;
                void *memory = realloc(children, child_capacity * sizeof(char *));
                if (!memory) die("realloc children");
                children = memory;
            }
            children[child_count++] = child_path(work->path, entry->name);
        }
    }
    close(work->fd); work->fd = -1;
    append_paths(pool, children, child_count);
    free(children);
}
static void *worker_main(void *argument) {
    struct worker *worker = argument;
    struct pool *pool = worker->pool;
    unsigned generation = 0;
    pthread_mutex_lock(&pool->lock);
    for (;;) {
        while (!pool->stopping && pool->generation == generation)
            pthread_cond_wait(&pool->changed, &pool->lock);
        if (pool->stopping) break;
        generation = pool->generation;
        pthread_mutex_unlock(&pool->lock);
        for (;;) {
            size_t index = atomic_fetch_add(&pool->next, 1);
            if (index >= pool->count) break;
            if (pool->phase == 0) prepare(pool, &pool->work[index], &worker->count);
            else read_names(pool, &pool->work[index], &worker->count);
        }
        pthread_mutex_lock(&pool->lock);
        if (++pool->completed == pool->threads) pthread_cond_signal(&pool->finished);
    }
    pthread_mutex_unlock(&pool->lock);
    return NULL;
}
static void phase(struct pool *pool, int phase) {
    pthread_mutex_lock(&pool->lock);
    pool->phase = phase; pool->completed = 0;
    atomic_store(&pool->next, 0);
    ++pool->generation;
    pthread_cond_broadcast(&pool->changed);
    while (pool->completed < pool->threads)
        pthread_cond_wait(&pool->finished, &pool->lock);
    pthread_mutex_unlock(&pool->lock);
}
static int physical_compare(const void *first, const void *second) {
    const struct work *a = first, *b = second;
    if (a->physical != b->physical) return a->physical < b->physical ? -1 : 1;
    return strcmp(a->path, b->path);
}
int main(int argc, char **argv) {
    if (!getuid() || getuid() != geteuid()) {
        fputs("Directory-I/O probe requires an ordinary unchanged UID.\n", stderr);
        return 1;
    }
    const char *root = NULL;
    unsigned threads = 32;
    for (int index = 1; index < argc; ++index) {
        if (!strcmp(argv[index], "--noprogress") || !strcmp(argv[index], "--no-color")) continue;
        if (!strcmp(argv[index], "--threads") && index + 1 < argc) {
            char *end;
            unsigned long value = strtoul(argv[++index], &end, 10);
            if (*end || !value || value > 256) return 2;
            threads = (unsigned)value;
        } else if (argv[index][0] != '-' && !root) root = argv[index];
        else return 2;
    }
    if (!root) return 2;
    struct stat metadata;
    if (stat(root, &metadata)) die("stat root");
    struct rlimit limits;
    if (getrlimit(RLIMIT_NOFILE, &limits)) die("getrlimit");
    size_t window = WINDOW;
    if (limits.rlim_cur < 128) { fputs("Insufficient descriptor limit.\n", stderr); return 1; }
    if (window > limits.rlim_cur - 64) window = limits.rlim_cur - 64;
    struct pool pool = {.lock = PTHREAD_MUTEX_INITIALIZER, .changed = PTHREAD_COND_INITIALIZER,
        .finished = PTHREAD_COND_INITIALIZER, .threads = threads, .device = metadata.st_dev};
    struct worker *workers = calloc(threads, sizeof(*workers));
    pthread_t *handles = allocate(threads * sizeof(*handles));
    pool.work = allocate(window * sizeof(*pool.work));
    char *root_path = strdup(root);
    if (!workers || !root_path) die("calloc root");
    append_paths(&pool, &root_path, 1);
    struct timespec started, ended;
    clock_gettime(CLOCK_MONOTONIC, &started);
    for (unsigned index = 0; index < threads; ++index) {
        workers[index].pool = &pool;
        int error = pthread_create(&handles[index], NULL, worker_main, &workers[index]);
        if (error) { errno = error; die("pthread_create"); }
    }
    while (pool.pending_count) {
        pool.count = pool.pending_count < window ? pool.pending_count : window;
        for (size_t index = 0; index < pool.count; ++index)
            pool.work[index] = (struct work){.path = pool.pending[--pool.pending_count], .fd = -1};
        phase(&pool, 0);
        if (PHYSICAL_ORDER) qsort(pool.work, pool.count, sizeof(*pool.work), physical_compare);
        phase(&pool, 1);
        for (size_t index = 0; index < pool.count; ++index) free(pool.work[index].path);
    }
    pthread_mutex_lock(&pool.lock); pool.stopping = true;
    pthread_cond_broadcast(&pool.changed); pthread_mutex_unlock(&pool.lock);
    struct counters total = {0};
    for (unsigned index = 0; index < threads; ++index) {
        pthread_join(handles[index], NULL);
        total.directories += workers[index].count.directories;
        total.files += workers[index].count.files; total.entries += workers[index].count.entries;
        total.errors += workers[index].count.errors; total.mappings += workers[index].count.mappings;
        total.checksum += workers[index].count.checksum;
    }
    clock_gettime(CLOCK_MONOTONIC, &ended);
    uint64_t milliseconds = (uint64_t)((ended.tv_sec - started.tv_sec) * 1000
        + (ended.tv_nsec - started.tv_nsec) / 1000000);
    fprintf(stderr, "dq[debug]: directory-io-probe purpose=directory-only-diagnostic physical=%d window=%zu threads=%u mappings=%" PRIu64 " entries=%" PRIu64 " checksum=%" PRIu64 "\n",
        PHYSICAL_ORDER, window, threads, total.mappings, total.entries, total.checksum);
    fprintf(stderr, "dq[debug]: scan=%" PRIu64 "ms files=%" PRIu64 " directories=%" PRIu64 " errors=%" PRIu64 "\n",
        milliseconds, total.files, total.directories, total.errors);
    free(pool.work); free(pool.pending); free(workers); free(handles);
    return 0;
}
