// Non-platform executable: Apple-protected tools may independently strip DYLD_*.
#include <libproc.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static uint64_t coalition(const char *pid) {
    uint64_t info[5] = {0};
    int size = sizeof(info);
    if (proc_pidinfo(atoi(pid), 20, 0, info, size) != size) {
        perror("proc_pidinfo");
        exit(1);
    }
    return info[0];
}

int main(int argc, char **argv) {
    if (argc == 3) {
        uint64_t core = coalition(argv[1]), caller = coalition(argv[2]);
        printf("core coalition=%llu, caller coalition=%llu\n",
               (unsigned long long)core, (unsigned long long)caller);
        return core == caller;
    }
    const char *value = getenv("DYLD_LIBRARY_PATH");
    printf("DYLD_LIBRARY_PATH=%s\n", value ? value : "(unset)");
    return !value || strcmp(value, "/herdr-identity-test") != 0;
}
