#include <errno.h>
#include <fcntl.h>
#include <spawn.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    int queue = kqueue();
    if (queue < 0) return 3;

    for (int index = 0; index < 2000; ++index) {
        int out[2], err[2];
        if (pipe(out) || pipe(err)) return 4;
        posix_spawn_file_actions_t actions;
        if (posix_spawn_file_actions_init(&actions)) return 5;
        posix_spawn_file_actions_adddup2(&actions, out[1], STDOUT_FILENO);
        posix_spawn_file_actions_adddup2(&actions, err[1], STDERR_FILENO);
        posix_spawn_file_actions_addclose(&actions, out[0]);
        posix_spawn_file_actions_addclose(&actions, out[1]);
        posix_spawn_file_actions_addclose(&actions, err[0]);
        posix_spawn_file_actions_addclose(&actions, err[1]);
        char *child_argv[] = {argv[1], "--list", NULL};
        pid_t pid = 0;
        int spawn_code = posix_spawn(&pid, argv[1], &actions, NULL, child_argv, environ);
        posix_spawn_file_actions_destroy(&actions);
        close(out[1]);
        close(err[1]);
        if (spawn_code) return 6;

        usleep(2500);
        struct kevent change;
        EV_SET(&change, (uintptr_t)pid, EVFILT_PROC, EV_ADD | EV_ONESHOT, NOTE_EXIT, 0, NULL);
        errno = 0;
        int watch_result = kevent(queue, &change, 1, NULL, 0, NULL);
        int watch_errno = errno;
        if (watch_result < 0 && watch_errno == ESRCH) {
            int status = 0;
            errno = 0;
            pid_t wait_result = waitpid(pid, &status, WNOHANG);
            int wait_errno = errno;
            int kill_result = kill(pid, 0);
            int kill_errno = errno;
            printf("index=%d pid=%d watch_errno=%d wait_result=%d wait_errno=%d raw_status=%d kill_result=%d kill_errno=%d\n",
                index, pid, watch_errno, wait_result, wait_errno, status, kill_result, kill_errno);
            if (wait_result == 0) {
                char command[160];
                snprintf(command, sizeof(command), "ps -o pid,ppid,state,command -p %d", pid);
                fflush(stdout);
                system(command);
                int retry_result = kevent(queue, &change, 1, NULL, 0, NULL);
                printf("retry_result=%d retry_errno=%d\n", retry_result, errno);
            }
            if (wait_result > 0) pid = 0;
        }
        if (pid) {
            int status = 0;
            waitpid(pid, &status, 0);
        }
        close(out[0]);
        close(err[0]);
    }
    close(queue);
    return 0;
}
