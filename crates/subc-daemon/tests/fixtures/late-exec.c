#include <unistd.h>

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    while (access(argv[1], F_OK) != 0) usleep(1000);
    execl("/bin/sleep", "sleep", "600", (char *)0);
    return 3;
}
