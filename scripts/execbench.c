/* Process-launch timer for the AOT benchmarks.
 *
 * fork+exec a command N times and report the min/median per-run wall time.
 *
 * Measuring a compiled binary with an external timer (python3 subprocess, `time`,
 * hyperfine) charges the *timer's* own fork/exec and interpreter startup to the
 * binary: on this machine that overhead is ~27 ms, which is 10x the thing being
 * measured. Forking from C and exec'ing the target directly measures the cost
 * the user actually pays.
 *
 * usage: execbench <n> <cmd> [args...]
 *        execbench <n> --stdin <cmd> [args...]   (same; stdin inherited)
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static double now_ms(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (double)ts.tv_sec * 1000.0 + (double)ts.tv_nsec / 1e6;
}

static int cmp_d(const void *a, const void *b) {
  double x = *(const double *)a, y = *(const double *)b;
  return (x > y) - (x < y);
}

int main(int argc, char **argv) {
  if (argc < 3) {
    fprintf(stderr, "usage: %s <n> <cmd> [args...]\n", argv[0]);
    return 2;
  }
  int n = atoi(argv[1]);
  if (n < 1) {
    fprintf(stderr, "n must be >= 1\n");
    return 2;
  }
  double *ts = malloc(sizeof(double) * (size_t)n);
  if (!ts) return 1;

  for (int i = 0; i < n; i++) {
    int devnull = open("/dev/null", O_WRONLY);
    double t0 = now_ms();
    pid_t p = fork();
    if (p == 0) {
      if (devnull >= 0) {
        dup2(devnull, 1); /* stdout */
        dup2(devnull, 2); /* stderr */
        if (devnull > 2) close(devnull);
      }
      execv(argv[2], &argv[2]);
      _exit(127);
    }
    if (p < 0) {
      perror("fork");
      return 1;
    }
    int st = 0;
    waitpid(p, &st, 0);
    ts[i] = now_ms() - t0;
    if (devnull >= 0) close(devnull);
  }
  qsort(ts, (size_t)n, sizeof(double), cmp_d);
  printf("min=%.3fms  p50=%.3fms  n=%d\n", ts[0], ts[n / 2], n);
  free(ts);
  return 0;
}
