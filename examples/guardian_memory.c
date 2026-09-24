#include <stdint.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
int main(void) {
    const size_t cap = (size_t)2048*1024*1024, chunk = 8*1024*1024;
    uint64_t *memory = malloc(cap);
    if (!memory) return 1;
    uint64_t x = (uint64_t)getpid();
    size_t used = 0;
    time_t deadline = time(NULL) + 140;
    while (time(NULL) < deadline) {
        size_t end = used < cap ? used + chunk : used;
        for (size_t i=used/8; i<end/8; i++) {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            memory[i] = x;
        }
        used = end;
        for (size_t i=0; i<used/8; i+=512) ((volatile uint64_t*)memory)[i] ^= x;
        usleep(250000);
    }
    free(memory);
    return 0;
}
