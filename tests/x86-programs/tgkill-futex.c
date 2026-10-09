// A thread parked in sem_wait gets a tgkill'd signal; its handler posts an ack
// (Boehm's stop-the-world shape). Then it is released with a second signal.
#define _GNU_SOURCE
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>
static sem_t never, ack;
static int handled;
static pid_t tids[4];
static void on_sig(int s) { (void)s; __atomic_fetch_add(&handled, 1, __ATOMIC_SEQ_CST); sem_post(&ack); }
static void *worker(void *p) {
  tids[(long)p] = syscall(SYS_gettid);
  sem_post(&ack);
  while (sem_wait(&never) != 0) {}
  return 0;
}
int main(int argc, char **argv) { (void)argv;
  sem_init(&never, 0, 0); sem_init(&ack, 0, 0);
  struct sigaction sa = {0}; sa.sa_handler = on_sig; sa.sa_flags = SA_RESTART; sigemptyset(&sa.sa_mask);
  sigaction(SIGPWR, &sa, 0);
  pthread_t t[4];
  for (long i = 0; i < 4; i++) pthread_create(&t[i], 0, worker, (void *)i);
  for (int i = 0; i < 4; i++) sem_wait(&ack);
  usleep(200000);
  for (int round = 0; round < (argc > 1 ? 0 : 3); round++) {
    for (int i = 0; i < 4; i++) syscall(SYS_tgkill, getpid(), tids[i], SIGPWR);
    for (int i = 0; i < 4; i++) sem_wait(&ack);
    usleep(50000);
  }
  printf("acks %d\n", handled);
  for (int i = 0; i < 4; i++) sem_post(&never);
  for (int i = 0; i < 4; i++) pthread_join(t[i], 0);
  printf("joined\n");
  return 0;
}
