/* LD_PRELOAD diagnostic shim: this sandbox denies socket(AF_UNIX) but allows
 * AF_INET. Chromium's ProcessSingleton wants an AF_UNIX listening socket, so we
 * hand it a real AF_INET loopback socket instead and translate its bind().
 *
 * This exists ONLY to measure CEF rendering inside a restricted sandbox. It is
 * not part of Cinder and must never ship: it removes the single-instance lock.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dlfcn.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

typedef int (*socket_fn)(int, int, int);
typedef int (*bind_fn)(int, const struct sockaddr*, socklen_t);
typedef int (*listen_fn)(int, int);
typedef int (*connect_fn)(int, const struct sockaddr*, socklen_t);

#define MAX_FAKE 64
static int g_fake[MAX_FAKE];
static int g_fake_count = 0;

static int is_fake(int fd) {
  for (int i = 0; i < g_fake_count; i++) {
    if (g_fake[i] == fd) return 1;
  }
  return 0;
}

int socket(int domain, int type, int protocol) {
  static socket_fn real = NULL;
  if (!real) real = (socket_fn)dlsym(RTLD_NEXT, "socket");
  if (domain == AF_UNIX) {
    int fd = real(AF_INET, type, 0);
    if (fd >= 0) {
      if (g_fake_count < MAX_FAKE) {
        g_fake[g_fake_count++] = fd;
      }
      fprintf(stderr, "[sock] socket(AF_UNIX) -> AF_INET fd %d\n", fd);
      return fd;
    }
    fprintf(stderr, "[sock] AF_INET fallback failed: %s\n", strerror(errno));
  }
  return real(domain, type, protocol);
}

int bind(int fd, const struct sockaddr* addr, socklen_t len) {
  static bind_fn real = NULL;
  if (!real) real = (bind_fn)dlsym(RTLD_NEXT, "bind");
  if (is_fake(fd)) {
    struct sockaddr_in loop;
    memset(&loop, 0, sizeof(loop));
    loop.sin_family = AF_INET;
    loop.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    loop.sin_port = 0;
    int rc = real(fd, (struct sockaddr*)&loop, sizeof(loop));
    fprintf(stderr, "[sock] bind(fd %d) -> loopback: %s\n", fd,
            rc == 0 ? "ok" : strerror(errno));
    return rc;
  }
  return real(fd, addr, len);
}

int listen(int fd, int backlog) {
  static listen_fn real = NULL;
  if (!real) real = (listen_fn)dlsym(RTLD_NEXT, "listen");
  if (is_fake(fd)) {
    return real(fd, backlog);
  }
  return real(fd, backlog);
}

int connect(int fd, const struct sockaddr* addr, socklen_t len) {
  static connect_fn real = NULL;
  if (!real) real = (connect_fn)dlsym(RTLD_NEXT, "connect");
  if (is_fake(fd)) {
    errno = ECONNREFUSED;
    return -1;
  }
  return real(fd, addr, len);
}
