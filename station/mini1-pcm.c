/* YNDX-0004 has an ARM32 ALSA library even when the assistant runs as ARM64.
 * Read mono S16_LE on stdin; keep the volume and mixing of Alice's default PCM. */
#include <dlfcn.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

typedef struct snd_pcm pcm_t;
enum { PLAYBACK = 0, S16_LE = 2, RW_INTERLEAVED = 3 };

#define LOAD(name) do { \
    name = dlsym(library, #name); \
    if (!name) { fprintf(stderr, "%s\n", dlerror()); return 1; } \
} while (0)

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    char *end;
    long rate = strtol(argv[1], &end, 10);
    if (*end || rate < 8000 || rate > 192000) return 2;

    void *library = dlopen("libasound.so.2", RTLD_NOW);
    if (!library) {
        fprintf(stderr, "%s\n", dlerror());
        return 1;
    }
    int (*snd_pcm_open)(pcm_t **, const char *, int, int);
    int (*snd_pcm_set_params)(pcm_t *, int, int, unsigned, unsigned, int, unsigned);
    long (*snd_pcm_writei)(pcm_t *, const void *, unsigned long);
    int (*snd_pcm_recover)(pcm_t *, int, int);
    int (*snd_pcm_drain)(pcm_t *);
    int (*snd_pcm_close)(pcm_t *);
    const char *(*snd_strerror)(int);
    LOAD(snd_pcm_open);
    LOAD(snd_pcm_set_params);
    LOAD(snd_pcm_writei);
    LOAD(snd_pcm_recover);
    LOAD(snd_pcm_drain);
    LOAD(snd_pcm_close);
    LOAD(snd_strerror);

    pcm_t *pcm;
    int err = snd_pcm_open(&pcm, "default", PLAYBACK, 0);
    if (err < 0) {
        fprintf(stderr, "ALSA open: %s\n", snd_strerror(err));
        return 1;
    }
    err = snd_pcm_set_params(pcm, S16_LE, RW_INTERLEAVED, 2, (unsigned)rate, 1, 120000);
    if (err < 0) goto done;
    int16_t mono[480], stereo[960];
    size_t frames;
    while ((frames = fread(mono, sizeof(*mono), 480, stdin)) > 0) {
        for (size_t i = 0; i < frames; ++i) {
            stereo[2 * i] = stereo[2 * i + 1] = mono[i];
        }
        for (size_t i = 0; i < frames;) {
            long written = snd_pcm_writei(pcm, stereo + 2 * i, frames - i);
            if (written > 0) {
                i += (size_t)written;
            } else {
                err = written ? snd_pcm_recover(pcm, (int)written, 1) : -EIO;
                if (err < 0) goto done;
            }
        }
    }
    err = ferror(stdin) ? -EIO : snd_pcm_drain(pcm);
done:
    if (err < 0) fprintf(stderr, "ALSA playback: %s\n", snd_strerror(err));
    snd_pcm_close(pcm);
    return err < 0;
}
