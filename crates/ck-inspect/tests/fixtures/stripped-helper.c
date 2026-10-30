#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

__attribute__((noinline)) static void inspect_busy_loop(void) {
    volatile uint64_t counter = 0;
    for (;;) {
        counter++;
    }
}

int main(void) {
    FILE *pid_file = fopen(getenv("INSPECT_FIXTURE_PID_PATH"), "w");
    if (pid_file == NULL) {
        return 1;
    }
    fprintf(pid_file, "%d", getpid());
    fclose(pid_file);
    inspect_busy_loop();
}
