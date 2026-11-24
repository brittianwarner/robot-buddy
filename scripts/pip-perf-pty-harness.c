/*
 * Dual-PTY hardware simulator for Linux ARM64 recording-replay E2E tests.
 *
 * Exposes two real Linux PTYs (no physical hardware):
 *   1. Face PTY (ESP32-S3 JSON-line serial protocol, 115200 raw CLOCAL -HUPCL)
 *   2. Neck PTY (Feetech STS3215 half-duplex binary protocol, 1 Mbaud raw)
 *
 * Usage:
 *   pip-perf-pty-harness <ready.json> <face.jsonl> <neck.jsonl>
 *
 * Writes {"face_pty":"/dev/pts/X","neck_pty":"/dev/pts/Y"} atomically to
 * <ready.json>, then serves both PTYs until SIGTERM or SIGINT.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t g_stop = 0;

static void on_signal(int sig) {
    (void)sig;
    g_stop = 1;
}

static uint64_t now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000ULL + (uint64_t)(ts.tv_nsec / 1000000L);
}

static int open_pty_pair(int *master_out, int *slave_out, char *path_buf, size_t path_len) {
    int master = posix_openpt(O_RDWR | O_NOCTTY | O_NONBLOCK);
    if (master < 0) return -1;
    if (grantpt(master) != 0 || unlockpt(master) != 0) {
        close(master);
        return -1;
    }
    if (ptsname_r(master, path_buf, path_len) != 0) {
        close(master);
        return -1;
    }
    if (chmod(path_buf, 0660) != 0) {
        close(master);
        return -1;
    }
    /* Hold slave open (never flock'd) so master reads never fail with EIO before robot opens. */
    int slave = open(path_buf, O_RDWR | O_NOCTTY | O_NONBLOCK);
    if (slave < 0) {
        close(master);
        return -1;
    }
    struct termios t;
    if (tcgetattr(slave, &t) == 0) {
        /* Start from cooked 9600+HUPCL defaults so robot's open_serial() reconfiguration is proven. */
        cfsetspeed(&t, B9600);
        t.c_lflag |= (ECHO | ICANON | ISIG);
        t.c_oflag |= (OPOST | ONLCR);
        t.c_iflag |= ICRNL;
        t.c_cflag |= HUPCL;
        t.c_cflag &= ~(tcflag_t)CLOCAL;
        tcsetattr(slave, TCSANOW, &t);
    }
    *master_out = master;
    *slave_out = slave;
    return 0;
}

static void write_all(int fd, const uint8_t *buf, size_t len) {
    size_t sent = 0;
    while (sent < len && !g_stop) {
        ssize_t n = write(fd, buf + sent, len - sent);
        if (n > 0) {
            sent += (size_t)n;
        } else if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)) {
            struct pollfd pfd = {.fd = fd, .events = POLLOUT};
            poll(&pfd, 1, 20);
        } else {
            break;
        }
    }
}

/* ---------- Face PTY thread ---------- */

struct face_ctx {
    int master;
    int slave;
    FILE *log;
    uint64_t t0;
};

static void record_face_termios(struct face_ctx *ctx) {
    struct termios t;
    if (tcgetattr(ctx->slave, &t) != 0) return;
    speed_t ospeed = cfgetospeed(&t);
    bool baud_115200 = (ospeed == B115200);
    bool clocal = (t.c_cflag & CLOCAL) != 0;
    bool hupcl = (t.c_cflag & HUPCL) != 0;
    bool echo = (t.c_lflag & ECHO) != 0;
    bool icanon = (t.c_lflag & ICANON) != 0;
    bool opost = (t.c_oflag & OPOST) != 0;
    fprintf(
        ctx->log,
        "{\"t_ms\":%llu,\"type\":\"termios\",\"baud_115200\":%s,\"clocal\":%s,\"hupcl\":%s,"
        "\"echo\":%s,\"icanon\":%s,\"opost\":%s}\n",
        (unsigned long long)(now_ms() - ctx->t0),
        baud_115200 ? "true" : "false",
        clocal ? "true" : "false",
        hupcl ? "true" : "false",
        echo ? "true" : "false",
        icanon ? "true" : "false",
        opost ? "true" : "false"
    );
    fflush(ctx->log);
}

static void *face_thread(void *arg) {
    struct face_ctx *ctx = (struct face_ctx *)arg;
    char line[2048];
    size_t len = 0;
    bool greeted = false;
    uint64_t next_hb = 0;
    uint32_t hb_seq = 1;
    uint8_t buf[512];

    while (!g_stop) {
        if (greeted && now_ms() >= next_hb) {
            char hb[64];
            int hlen = snprintf(hb, sizeof(hb), "{\"hb\":%u,\"fps\":30}\n", hb_seq++);
            if (hlen > 0) {
                write_all(ctx->master, (const uint8_t *)hb, (size_t)hlen);
            }
            next_hb = now_ms() + 1500;
        }
        struct pollfd pfd = {.fd = ctx->master, .events = POLLIN};
        int ready = poll(&pfd, 1, 50);
        if (ready <= 0) continue;
        if (!(pfd.revents & POLLIN)) continue;

        ssize_t n = read(ctx->master, buf, sizeof(buf));
        if (n <= 0) continue;

        if (!greeted) {
            greeted = true;
            record_face_termios(ctx);
            const char *hello = "{\"hello\":\"pip-face\",\"v\":1}\n{\"hb\":1,\"fps\":30}\n";
            write_all(ctx->master, (const uint8_t *)hello, strlen(hello));
            hb_seq = 2;
            next_hb = now_ms() + 1500;
        }

        for (ssize_t i = 0; i < n; i++) {
            uint8_t c = buf[i];
            if (c == '\n') {
                line[len] = '\0';
                if (len > 0) {
                    fprintf(
                        ctx->log,
                        "{\"t_ms\":%llu,\"type\":\"cmd\",\"cmd\":%s}\n",
                        (unsigned long long)(now_ms() - ctx->t0),
                        line
                    );
                    fflush(ctx->log);
                }
                len = 0;
            } else if (c != '\r' && len + 1 < sizeof(line)) {
                line[len++] = (char)c;
            }
        }
    }
    return NULL;
}

/* ---------- STS3215 Neck PTY thread ---------- */

struct neck_ctx {
    int master;
    int slave;
    FILE *log;
    uint64_t t0;
};

static uint8_t sts_checksum(const uint8_t *bytes, size_t len) {
    uint8_t sum = 0;
    for (size_t i = 0; i < len; i++) {
        sum = (uint8_t)(sum + bytes[i]);
    }
    return (uint8_t)(~sum);
}

static void record_neck_termios(struct neck_ctx *ctx) {
    struct termios t;
    if (tcgetattr(ctx->slave, &t) != 0) return;
    bool megabaud = (cfgetospeed(&t) == B1000000);
    bool clocal = (t.c_cflag & CLOCAL) != 0;
    bool hupcl = (t.c_cflag & HUPCL) != 0;
    fprintf(
        ctx->log,
        "{\"t_ms\":%llu,\"type\":\"termios\",\"megabaud\":%s,\"clocal\":%s,\"hupcl\":%s}\n",
        (unsigned long long)(now_ms() - ctx->t0),
        megabaud ? "true" : "false",
        clocal ? "true" : "false",
        hupcl ? "true" : "false"
    );
    fflush(ctx->log);
}

static void init_servo_mem(uint8_t mem[3][64]) {
    memset(mem, 0, 3 * 64);
    const uint16_t mins[3] = {1888, 1820, 1888};
    const uint16_t maxs[3] = {2208, 2276, 2208};
    const uint16_t center = 2048;
    for (int i = 0; i < 3; i++) {
        mem[i][9] = (uint8_t)(mins[i] & 0xFF);
        mem[i][10] = (uint8_t)(mins[i] >> 8);
        mem[i][11] = (uint8_t)(maxs[i] & 0xFF);
        mem[i][12] = (uint8_t)(maxs[i] >> 8);
        mem[i][33] = 0; /* MODE: position */
        mem[i][40] = 0; /* TORQUE: off */
        mem[i][42] = (uint8_t)(center & 0xFF);
        mem[i][43] = (uint8_t)(center >> 8);
        mem[i][55] = 1; /* LOCK: locked */
        mem[i][56] = (uint8_t)(center & 0xFF);
        mem[i][57] = (uint8_t)(center >> 8);
    }
}

static void handle_sts_frame(
    struct neck_ctx *ctx,
    uint8_t mem[3][64],
    const uint8_t *frame,
    size_t flen
) {
    uint8_t id = frame[2];
    uint8_t plen = frame[3];
    uint8_t instr = frame[4];
    const uint8_t *params = frame + 5;
    size_t param_count = (size_t)plen - 2;

    if (sts_checksum(frame + 2, flen - 3) != frame[flen - 1]) {
        fprintf(
            ctx->log,
            "{\"t_ms\":%llu,\"type\":\"bad_checksum\",\"id\":%u}\n",
            (unsigned long long)(now_ms() - ctx->t0),
            (unsigned)id
        );
        fflush(ctx->log);
        return;
    }

    if (instr == 0x01 && id >= 1 && id <= 3) {
        /* PING */
        uint8_t reply[6] = {0xFF, 0xFF, id, 2, 0, 0};
        reply[5] = sts_checksum(reply + 2, 3);
        write_all(ctx->master, reply, sizeof(reply));
    } else if (instr == 0x02 && id >= 1 && id <= 3 && param_count == 2) {
        /* READ */
        uint8_t reg = params[0];
        uint8_t count = params[1];
        if ((size_t)reg + (size_t)count <= 64 && count <= 32) {
            uint8_t reply[40];
            reply[0] = 0xFF;
            reply[1] = 0xFF;
            reply[2] = id;
            reply[3] = (uint8_t)(count + 2);
            reply[4] = 0;
            memcpy(reply + 5, &mem[id - 1][reg], count);
            reply[5 + count] = sts_checksum(reply + 2, (size_t)count + 3);
            write_all(ctx->master, reply, (size_t)count + 6);
        }
    } else if (instr == 0x03 && id >= 1 && id <= 3 && param_count >= 2) {
        /* Addressed WRITE (e.g. TORQUE / LOCK / LIMITS) */
        uint8_t reg = params[0];
        const uint8_t *data = params + 1;
        size_t dlen = param_count - 1;
        if ((size_t)reg + dlen <= 64) {
            memcpy(&mem[id - 1][reg], data, dlen);
        }
        if (reg == 40 && dlen == 1) {
            fprintf(
                ctx->log,
                "{\"t_ms\":%llu,\"type\":\"torque\",\"id\":%u,\"on\":%s}\n",
                (unsigned long long)(now_ms() - ctx->t0),
                (unsigned)id,
                data[0] ? "true" : "false"
            );
            fflush(ctx->log);
        }
        uint8_t ack[6] = {0xFF, 0xFF, id, 2, 0, 0};
        ack[5] = sts_checksum(ack + 2, 3);
        write_all(ctx->master, ack, sizeof(ack));
    } else if (instr == 0x83 && id == 254 && param_count >= 2) {
        /* SYNC WRITE */
        uint8_t reg = params[0];
        uint8_t dlen = params[1];
        size_t pos = 2;
        bool saw_goals = (reg == 41 && dlen == 7);
        uint16_t goals[3] = {2048, 2048, 2048};
        int goal_count = 0;

        while (pos + 1 + dlen <= param_count) {
            uint8_t sid = params[pos];
            const uint8_t *data = params + pos + 1;
            pos += 1 + dlen;
            if (sid < 1 || sid > 3) continue;
            if ((size_t)reg + (size_t)dlen <= 64) {
                memcpy(&mem[sid - 1][reg], data, dlen);
            }
            if (reg == 40 && dlen == 1) {
                fprintf(
                    ctx->log,
                    "{\"t_ms\":%llu,\"type\":\"torque\",\"id\":%u,\"on\":%s}\n",
                    (unsigned long long)(now_ms() - ctx->t0),
                    (unsigned)sid,
                    data[0] ? "true" : "false"
                );
                fflush(ctx->log);
            } else if (saw_goals) {
                uint16_t steps = (uint16_t)data[1] | ((uint16_t)data[2] << 8);
                goals[sid - 1] = steps;
                goal_count++;
                /* Keep simulated PRESENT tracking commanded goal within limits. */
                mem[sid - 1][56] = data[1];
                mem[sid - 1][57] = data[2];
            }
        }
        if (saw_goals && goal_count == 3) {
            fprintf(
                ctx->log,
                "{\"t_ms\":%llu,\"type\":\"goals\",\"steps\":[%u,%u,%u]}\n",
                (unsigned long long)(now_ms() - ctx->t0),
                (unsigned)goals[0],
                (unsigned)goals[1],
                (unsigned)goals[2]
            );
            fflush(ctx->log);
        }
    }
}

static void *neck_thread(void *arg) {
    struct neck_ctx *ctx = (struct neck_ctx *)arg;
    uint8_t mem[3][64];
    init_servo_mem(mem);

    uint8_t stream[512];
    size_t slen = 0;
    uint8_t chunk[128];
    bool recorded_termios = false;

    while (!g_stop) {
        struct pollfd pfd = {.fd = ctx->master, .events = POLLIN};
        int ready = poll(&pfd, 1, 20);
        if (ready <= 0) continue;
        if (!(pfd.revents & POLLIN)) continue;

        ssize_t n = read(ctx->master, chunk, sizeof(chunk));
        if (n <= 0) continue;

        if (!recorded_termios) {
            recorded_termios = true;
            record_neck_termios(ctx);
        }

        if (slen + (size_t)n > sizeof(stream)) {
            slen = 0;
        }
        memcpy(stream + slen, chunk, (size_t)n);
        slen += (size_t)n;

        size_t offset = 0;
        while (offset + 4 <= slen) {
            if (stream[offset] != 0xFF || stream[offset + 1] != 0xFF) {
                offset++;
                continue;
            }
            uint8_t plen = stream[offset + 3];
            if (plen < 2 || plen > 64) {
                offset += 2;
                continue;
            }
            size_t flen = (size_t)plen + 4;
            if (offset + flen > slen) break;
            handle_sts_frame(ctx, mem, stream + offset, flen);
            offset += flen;
        }
        if (offset > 0) {
            memmove(stream, stream + offset, slen - offset);
            slen -= offset;
        }
    }
    return NULL;
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: %s <ready.json> <face.jsonl> <neck.jsonl>\n", argv[0]);
        return 2;
    }
    signal(SIGTERM, on_signal);
    signal(SIGINT, on_signal);
    signal(SIGPIPE, SIG_IGN);

    int face_master = -1, face_slave = -1;
    int neck_master = -1, neck_slave = -1;
    char face_path[128] = {0};
    char neck_path[128] = {0};

    if (open_pty_pair(&face_master, &face_slave, face_path, sizeof(face_path)) != 0) {
        perror("open face pty");
        return 1;
    }
    if (open_pty_pair(&neck_master, &neck_slave, neck_path, sizeof(neck_path)) != 0) {
        perror("open neck pty");
        return 1;
    }

    FILE *face_log = fopen(argv[2], "w");
    FILE *neck_log = fopen(argv[3], "w");
    if (!face_log || !neck_log) {
        perror("open log files");
        return 1;
    }

    uint64_t t0 = now_ms();
    struct face_ctx fctx = {.master = face_master, .slave = face_slave, .log = face_log, .t0 = t0};
    struct neck_ctx nctx = {.master = neck_master, .slave = neck_slave, .log = neck_log, .t0 = t0};

    pthread_t ft, nt;
    if (pthread_create(&ft, NULL, face_thread, &fctx) != 0 ||
        pthread_create(&nt, NULL, neck_thread, &nctx) != 0) {
        perror("pthread_create");
        return 1;
    }

    char tmp_ready[512];
    snprintf(tmp_ready, sizeof(tmp_ready), "%s.tmp", argv[1]);
    FILE *rf = fopen(tmp_ready, "w");
    if (!rf) {
        perror("open ready file");
        return 1;
    }
    fprintf(rf, "{\"face_pty\":\"%s\",\"neck_pty\":\"%s\"}\n", face_path, neck_path);
    fclose(rf);
    if (rename(tmp_ready, argv[1]) != 0) {
        perror("rename ready file");
        return 1;
    }

    while (!g_stop) {
        struct timespec ts = {.tv_sec = 0, .tv_nsec = 50000000L};
        nanosleep(&ts, NULL);
    }

    pthread_join(ft, NULL);
    pthread_join(nt, NULL);
    fclose(face_log);
    fclose(neck_log);
    close(face_master);
    close(face_slave);
    close(neck_master);
    close(neck_slave);
    return 0;
}
