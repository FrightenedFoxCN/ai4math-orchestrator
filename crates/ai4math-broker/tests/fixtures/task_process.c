#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static int denied(void) { return errno == EPERM || errno == EACCES; }

int main(void) {
    struct stat st;
    if (fstat(STDIN_FILENO, &st) != 0 || !S_ISREG(st.st_mode) ||
        (fcntl(STDIN_FILENO, F_GETFL) & O_ACCMODE) != O_RDONLY) return 10;
    char byte;
    if (read(STDOUT_FILENO, &byte, 1) != -1 || errno != EBADF) return 11;
    if (environ[0] != NULL) return 12;

    char input[8192];
    if (fgets(input, sizeof input, stdin) == NULL) return 13;
    if (strcmp(input, "\"wait\"") == 0) {
        for (;;) pause();
    }
    if (strcmp(input, "\"exit\"") == 0) {
        fputs("stderr-is-not-result", stderr);
        fputs("stdout-is-not-an-error-diagnostic", stdout);
        return 42;
    }
    if (strcmp(input, "\"overflow\"") == 0 || strcmp(input, "\"full\"") == 0) {
        if (signal(SIGXFSZ, SIG_IGN) == SIG_ERR) return 14;
        char chunk[8192] = {0};
        for (int i = 0; i < 128; ++i) {
            if (write(STDOUT_FILENO, chunk, sizeof chunk) != sizeof chunk) return 15;
        }
        if (strcmp(input, "\"overflow\"") == 0) {
            if (write(STDOUT_FILENO, chunk, 1) != 1) return 16;
            if (write(STDOUT_FILENO, chunk, sizeof chunk) != -1 || errno != EFBIG) return 17;
        }
        return 0;
    }

    char workspace[4096];
    struct sockaddr_un remote = {.sun_family = AF_UNIX};
    int file_fd, socket_fd;
    if (sscanf(input, "[\"%4095[^\"]\",\"%103[^\"]\",%d,%d]",
               workspace, remote.sun_path, &file_fd, &socket_fd) != 4) return 18;
    if (fcntl(file_fd, F_GETFD) != -1 || errno != EBADF ||
        fcntl(socket_fd, F_GETFD) != -1 || errno != EBADF) return 19;
    int fd = open(workspace, O_RDONLY);
    if (fd >= 0 || !denied()) return 20;
    fd = open(workspace, O_WRONLY);
    if (fd >= 0 || !denied()) return 21;
    if (open("scratch", O_CREAT | O_WRONLY | O_EXCL, 0600) >= 0 || !denied()) return 22;
    fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
        if (!denied()) return 23;
    } else {
        if (connect(fd, (struct sockaddr *)&remote, SUN_LEN(&remote)) != -1 || !denied()) return 24;
        close(fd);
    }
    struct sockaddr_in local = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) {
        if (!denied()) return 25;
    } else {
        if (bind(fd, (struct sockaddr *)&local, sizeof local) != -1 || !denied()) return 26;
        close(fd);
    }
    pid_t child = fork();
    if (child == 0) _exit(0);
    if (child > 0) {
        waitpid(child, NULL, 0);
        return 27;
    }
    if (!denied()) return 28;
    if (kill(getppid(), SIGCONT) != -1 || !denied()) return 31;
    char *const argv[] = {"true", NULL};
    execve("/usr/bin/true", argv, environ);
    if (!denied()) return 29;

    struct rlimit limits;
    if (getrlimit(RLIMIT_CPU, &limits) != 0 || limits.rlim_max > 5 ||
        getrlimit(RLIMIT_FSIZE, &limits) != 0 || limits.rlim_max > 1024 * 1024 + 1 ||
        getrlimit(RLIMIT_CORE, &limits) != 0 || limits.rlim_max != 0) return 30;
    fputs("stderr-is-not-result", stderr);
    fputs("{\"isolated\":true}", stdout);
    return 0;
}
