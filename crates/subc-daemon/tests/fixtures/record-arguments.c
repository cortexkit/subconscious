#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    const char *path = getenv("RECOVERY_ARGUMENT_FILE");
    if (!path) return 2;
    FILE *file = fopen(path, "wb");
    if (!file) return 3;
    for (int i = 0; i < argc; ++i) {
        if (fwrite(argv[i], strlen(argv[i]) + 1, 1, file) != 1) return 4;
    }
    if (fclose(file) != 0) return 5;
    for (;;) pause();
}
