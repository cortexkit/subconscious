#include <sys/socket.h>
#include <sys/un.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    char path[104];
    while (fgets(path, sizeof(path), stdin)) {
        path[strcspn(path, "\n")] = 0;
        struct sockaddr_un address = {0};
        address.sun_family = AF_UNIX;
        memcpy(address.sun_path, path, strlen(path) + 1);
        int fd = socket(AF_UNIX, SOCK_STREAM, 0);
        if (fd < 0 || connect(fd, (struct sockaddr *)&address, sizeof(address))) return 2;
        char byte;
        while (read(fd, &byte, 1) > 0) {}
        close(fd);
    }
    return 0;
}
