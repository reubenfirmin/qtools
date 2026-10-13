// Linux x86-64 diagnostic: alter only thread creation to test CLONE_IO.
// Build: cc -O2 -Wall -Wextra -o /tmp/dq-clone-io-probe dev/research/clone-io-probe.c
// Usage: /tmp/dq-clone-io-probe --shared|--independent EXECUTABLE [ARGS...]
// Both modes trace only clone/clone3, leaving ordinary I/O syscalls untraced.
// This traces our own unprivileged child and never changes system settings.
#define _GNU_SOURCE
#include <errno.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <sched.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error This diagnostic supports Linux x86-64 only.
#endif

static void fail(const char *what) {
    perror(what);
    exit(125);
}

static void install_clone_filter(void) {
    struct sock_filter code[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_clone, 2, 0),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_clone3, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_TRACE),
    };
    struct sock_fprog program = {sizeof(code) / sizeof(code[0]), code};
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) fail("no_new_privs");
    if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &program)) fail("clone filter");
}

int main(int argc, char **argv) {
    if (getuid() == 0 || geteuid() != getuid()) {
        fprintf(stderr, "This diagnostic must run as an ordinary unprivileged user.\n");
        return 125;
    }
    if (argc < 3 || (strcmp(argv[1], "--shared") && strcmp(argv[1], "--independent"))) {
        fprintf(stderr, "Usage: %s --shared|--independent EXECUTABLE [ARGS...]\n", argv[0]);
        return 125;
    }
    int shared = !strcmp(argv[1], "--shared");
    pid_t child = fork();
    if (child < 0) fail("fork");
    if (!child) {
        // Allocate an I/O context without changing the inherited priority.
        long priority = syscall(SYS_ioprio_get, 1, 0);
        if (priority < 0 || syscall(SYS_ioprio_set, 1, 0, priority)) fail("I/O context");
        if (ptrace(PTRACE_TRACEME, 0, NULL, NULL)) fail("trace own child");
        raise(SIGSTOP);
        install_clone_filter();
        execv(argv[2], argv + 2);
        fail("exec");
    }
    int status;
    if (waitpid(child, &status, 0) != child || !WIFSTOPPED(status)) fail("initial child stop");
    long options = PTRACE_O_TRACESECCOMP | PTRACE_O_TRACECLONE | PTRACE_O_EXITKILL;
    if (ptrace(PTRACE_SETOPTIONS, child, NULL, (void *)options)) fail("trace options");
    if (ptrace(PTRACE_CONT, child, NULL, NULL)) fail("start child");
    unsigned changed = 0, observed = 0;
    int result = 125;
    for (;;) {
        pid_t tid = waitpid(-1, &status, __WALL);
        if (tid < 0) {
            if (errno == EINTR) continue;
            if (errno == ECHILD) break;
            fail("wait for child");
        }
        if (WIFEXITED(status) || WIFSIGNALED(status)) {
            if (tid == child) result = WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
            continue;
        }
        if (!WIFSTOPPED(status)) continue;
        unsigned event = (unsigned)status >> 16;
        if (event == PTRACE_EVENT_SECCOMP) {
            struct user_regs_struct registers;
            if (ptrace(PTRACE_GETREGS, tid, NULL, &registers)) fail("read clone registers");
            unsigned long flags;
            int legacy = registers.orig_rax == SYS_clone;
            if (legacy) flags = registers.rdi;
            else if (registers.orig_rax == SYS_clone3) {
                errno = 0;
                flags = (unsigned long)ptrace(PTRACE_PEEKDATA, tid, (void *)registers.rdi, NULL);
                if (errno) fail("read clone3 flags");
            } else {
                fprintf(stderr, "Unexpected syscall in clone filter.\n");
                return 125;
            }
            observed++;
            if (shared && (flags & CLONE_THREAD) && !(flags & CLONE_IO)) {
                flags |= (unsigned long)CLONE_IO;
                if (legacy) {
                    registers.rdi = flags;
                    if (ptrace(PTRACE_SETREGS, tid, NULL, &registers)) fail("set clone flags");
                } else if (ptrace(PTRACE_POKEDATA, tid, (void *)registers.rdi, (void *)flags)) fail("set clone3 flags");
                changed++;
            }
        }
        int signal = WSTOPSIG(status);
        if (event || signal == SIGTRAP || signal == SIGSTOP) signal = 0;
        if (ptrace(PTRACE_CONT, tid, NULL, (void *)(intptr_t)signal) && errno != ESRCH) fail("continue child");
    }
    fprintf(stderr, "dq[io-context-probe]: mode=%s clone-calls=%u modified=%u\n", shared ? "shared" : "independent", observed, changed);
    return result;
}
