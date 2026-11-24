/*
 * Native ARM64 ONNX Runtime openWakeWord probe.
 *
 * Mirrors apps/robot/src/listen/wake.rs (Pipeline::open, Pipeline::score, Gate)
 * and loads ORT_DYLIB_PATH via dlopen with ORT_API_VERSION = 20 (matching
 * ort = "=2.0.0-rc.9" / ort-sys ORT_API_VERSION).
 */
#define _POSIX_C_SOURCE 200809L

#include <dlfcn.h>
#include <errno.h>
#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "onnxruntime_c_api.h"

#define HOP 1280
#define OVERLAP 480
#define BINS 32
#define MEL_ROWS 76
#define MEL_CAP (MEL_ROWS * BINS)
#define WIDTH 96
#define REQUIRED_ORT_API_VERSION 20

static const OrtApi *g_ort = NULL;

static void die_status(OrtStatus *status, const char *ctx) {
    if (status != NULL) {
        const char *msg = g_ort->GetErrorMessage(status);
        fprintf(stderr, "ort_wake_probe error (%s): %s\n", ctx, msg ? msg : "unknown");
        g_ort->ReleaseStatus(status);
        exit(2);
    }
}

static double now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec * 1000.0 + (double)ts.tv_nsec / 1e6;
}

typedef struct {
    OrtEnv *env;
    OrtSessionOptions *opts;
    OrtMemoryInfo *mem;
    OrtSession *mel;
    OrtSession *encoder;
    OrtSession *classifier;
    char *mel_in;
    char *mel_out;
    char *enc_in;
    char *enc_out;
    char *cls_in;
    char *cls_out;
    size_t steps;
    float overlap[OVERLAP];
    size_t overlap_len;
    float bands[MEL_CAP];
    bool bands_init;
    float *embeddings;
    size_t emb_len;
} Pipeline;

static void get_io_names(OrtSession *sess, char **in_name, char **out_name) {
    OrtAllocator *alloc = NULL;
    die_status(g_ort->GetAllocatorWithDefaultOptions(&alloc), "GetAllocatorWithDefaultOptions");
    size_t n_in = 0, n_out = 0;
    die_status(g_ort->SessionGetInputCount(sess, &n_in), "SessionGetInputCount");
    die_status(g_ort->SessionGetOutputCount(sess, &n_out), "SessionGetOutputCount");
    if (n_in != 1 || n_out != 1) {
        fprintf(stderr, "expected 1 input and 1 output, got in=%zu out=%zu\n", n_in, n_out);
        exit(2);
    }
    char *in_raw = NULL, *out_raw = NULL;
    die_status(g_ort->SessionGetInputName(sess, 0, alloc, &in_raw), "SessionGetInputName");
    die_status(g_ort->SessionGetOutputName(sess, 0, alloc, &out_raw), "SessionGetOutputName");
    *in_name = strdup(in_raw);
    *out_name = strdup(out_raw);
    die_status(g_ort->AllocatorFree(alloc, in_raw), "AllocatorFree in");
    die_status(g_ort->AllocatorFree(alloc, out_raw), "AllocatorFree out");
}

static OrtSession *load_session(OrtEnv *env, OrtSessionOptions *opts, const char *dir, const char *file) {
    char path[4096];
    int n = snprintf(path, sizeof(path), "%s/%s", dir, file);
    if (n <= 0 || (size_t)n >= sizeof(path)) {
        fprintf(stderr, "model path too long\n");
        exit(2);
    }
    OrtSession *sess = NULL;
    die_status(g_ort->CreateSession(env, path, opts, &sess), file);
    return sess;
}

static void pipeline_open(Pipeline *p, const char *dir) {
    memset(p, 0, sizeof(*p));
    die_status(g_ort->CreateEnv(ORT_LOGGING_LEVEL_WARNING, "ort_wake_probe", &p->env), "CreateEnv");
    die_status(g_ort->CreateSessionOptions(&p->opts), "CreateSessionOptions");
    die_status(g_ort->SetIntraOpNumThreads(p->opts, 1), "SetIntraOpNumThreads");
    die_status(g_ort->SetInterOpNumThreads(p->opts, 1), "SetInterOpNumThreads");
    die_status(g_ort->CreateCpuMemoryInfo(OrtArenaAllocator, OrtMemTypeDefault, &p->mem), "CreateCpuMemoryInfo");

    p->classifier = load_session(p->env, p->opts, dir, "hey_pip.onnx");
    OrtTypeInfo *ti = NULL;
    die_status(g_ort->SessionGetInputTypeInfo(p->classifier, 0, &ti), "classifier InputTypeInfo");
    const OrtTensorTypeAndShapeInfo *tsi = NULL;
    die_status(g_ort->CastTypeInfoToTensorInfo(ti, &tsi), "CastTypeInfoToTensorInfo");
    size_t ndims = 0;
    die_status(g_ort->GetDimensionsCount(tsi, &ndims), "GetDimensionsCount");
    if (ndims != 3) {
        fprintf(stderr, "unsupported classifier ndims=%zu\n", ndims);
        exit(2);
    }
    int64_t dims[3] = {0, 0, 0};
    die_status(g_ort->GetDimensions(tsi, dims, 3), "GetDimensions");
    g_ort->ReleaseTypeInfo(ti);
    if (dims[1] < 1 || dims[1] > 64 || dims[2] != WIDTH) {
        fprintf(stderr, "unsupported wake classifier shape [%lld, %lld, %lld]\n",
                (long long)dims[0], (long long)dims[1], (long long)dims[2]);
        exit(2);
    }
    p->steps = (size_t)dims[1];
    p->embeddings = (float *)calloc(p->steps * WIDTH, sizeof(float));
    if (!p->embeddings) {
        fprintf(stderr, "oom allocating embeddings\n");
        exit(2);
    }

    p->mel = load_session(p->env, p->opts, dir, "melspectrogram.onnx");
    p->encoder = load_session(p->env, p->opts, dir, "embedding_model.onnx");

    get_io_names(p->mel, &p->mel_in, &p->mel_out);
    get_io_names(p->encoder, &p->enc_in, &p->enc_out);
    get_io_names(p->classifier, &p->cls_in, &p->cls_out);
}

static size_t run_infer(
    Pipeline *p,
    OrtSession *sess,
    const char *in_name,
    const char *out_name,
    const int64_t *shape,
    size_t ndims,
    const float *input_data,
    size_t input_count,
    float *out_buf,
    size_t out_cap
) {
    OrtValue *in_tensor = NULL;
    die_status(
        g_ort->CreateTensorWithDataAsOrtValue(
            p->mem,
            (void *)input_data,
            input_count * sizeof(float),
            shape,
            ndims,
            ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT,
            &in_tensor
        ),
        "CreateTensorWithDataAsOrtValue"
    );
    const char *in_names[1] = {in_name};
    const char *out_names[1] = {out_name};
    OrtValue *out_tensor = NULL;
    die_status(
        g_ort->Run(sess, NULL, in_names, (const OrtValue *const *)&in_tensor, 1, out_names, 1, &out_tensor),
        "Run"
    );
    g_ort->ReleaseValue(in_tensor);

    OrtTensorTypeAndShapeInfo *out_info = NULL;
    die_status(g_ort->GetTensorTypeAndShape(out_tensor, &out_info), "GetTensorTypeAndShape");
    size_t out_elems = 0;
    die_status(g_ort->GetTensorShapeElementCount(out_info, &out_elems), "GetTensorShapeElementCount");
    g_ort->ReleaseTensorTypeAndShapeInfo(out_info);

    if (out_elems > out_cap) {
        fprintf(stderr, "output buffer overflow: %zu > %zu\n", out_elems, out_cap);
        exit(2);
    }
    float *out_ptr = NULL;
    die_status(g_ort->GetTensorMutableData(out_tensor, (void **)&out_ptr), "GetTensorMutableData");
    memcpy(out_buf, out_ptr, out_elems * sizeof(float));
    g_ort->ReleaseValue(out_tensor);
    return out_elems;
}

static float pipeline_score(Pipeline *p, const int16_t *raw, bool reset) {
    if (reset || !p->bands_init) {
        p->overlap_len = 0;
        for (size_t i = 0; i < MEL_CAP; ++i) {
            p->bands[i] = 1.0f;
        }
        p->bands_init = true;
        p->emb_len = 0;
    }

    float wave[OVERLAP + HOP];
    size_t wave_len = p->overlap_len + HOP;
    if (p->overlap_len > 0) {
        memcpy(wave, p->overlap, p->overlap_len * sizeof(float));
    }
    for (size_t i = 0; i < HOP; ++i) {
        wave[p->overlap_len + i] = (float)raw[i];
    }
    memcpy(p->overlap, wave + (wave_len - OVERLAP), OVERLAP * sizeof(float));
    p->overlap_len = OVERLAP;

    int64_t mel_shape[2] = {1, (int64_t)wave_len};
    float mel_out[512];
    size_t mel_len = run_infer(p, p->mel, p->mel_in, p->mel_out, mel_shape, 2, wave, wave_len, mel_out, 512);
    if (mel_len != 160 && mel_len != 256) {
        fprintf(stderr, "unexpected mel output len=%zu\n", mel_len);
        exit(2);
    }
    memmove(p->bands, p->bands + mel_len, (MEL_CAP - mel_len) * sizeof(float));
    for (size_t i = 0; i < mel_len; ++i) {
        p->bands[MEL_CAP - mel_len + i] = mel_out[i] / 10.0f + 2.0f;
    }

    int64_t enc_shape[4] = {1, MEL_ROWS, BINS, 1};
    float emb_out[WIDTH];
    size_t emb_len = run_infer(p, p->encoder, p->enc_in, p->enc_out, enc_shape, 4, p->bands, MEL_CAP, emb_out, WIDTH);
    if (emb_len != WIDTH) {
        fprintf(stderr, "unexpected embedding width=%zu\n", emb_len);
        exit(2);
    }
    size_t emb_cap = p->steps * WIDTH;
    if (p->emb_len == emb_cap) {
        memmove(p->embeddings, p->embeddings + WIDTH, (emb_cap - WIDTH) * sizeof(float));
        p->emb_len -= WIDTH;
    }
    memcpy(p->embeddings + p->emb_len, emb_out, WIDTH * sizeof(float));
    p->emb_len += WIDTH;

    if (p->emb_len < emb_cap) {
        return 0.0f;
    }

    int64_t cls_shape[3] = {1, (int64_t)p->steps, WIDTH};
    float score_out[4];
    size_t score_len = run_infer(p, p->classifier, p->cls_in, p->cls_out, cls_shape, 3, p->embeddings, emb_cap, score_out, 4);
    if (score_len != 1 || !isfinite(score_out[0]) || score_out[0] < 0.0f || score_out[0] > 1.0f) {
        fprintf(stderr, "invalid wake score len=%zu val=%f\n", score_len, score_out[0]);
        exit(2);
    }
    return score_out[0];
}

static int cmp_double(const void *a, const void *b) {
    double da = *(const double *)a;
    double db = *(const double *)b;
    return (da > db) - (da < db);
}

int main(int argc, char **argv) {
    const char *wake_dir = NULL;
    const char *pcm_path = NULL;
    const char *scores_out_path = NULL;
    const char *ref_scores_path = NULL;
    float threshold = 0.5f;
    float busy_threshold = 0.8f;
    bool busy = false;

    for (int i = 1; i < argc; ++i) {
        if (strcmp(argv[i], "--wake-dir") == 0 && i + 1 < argc) {
            wake_dir = argv[++i];
        } else if (strcmp(argv[i], "--pcm") == 0 && i + 1 < argc) {
            pcm_path = argv[++i];
        } else if (strcmp(argv[i], "--scores-out") == 0 && i + 1 < argc) {
            scores_out_path = argv[++i];
        } else if (strcmp(argv[i], "--reference-scores") == 0 && i + 1 < argc) {
            ref_scores_path = argv[++i];
        } else if (strcmp(argv[i], "--threshold") == 0 && i + 1 < argc) {
            threshold = strtof(argv[++i], NULL);
        } else if (strcmp(argv[i], "--busy-threshold") == 0 && i + 1 < argc) {
            busy_threshold = strtof(argv[++i], NULL);
        } else if (strcmp(argv[i], "--busy") == 0) {
            busy = true;
        } else {
            fprintf(stderr, "unknown argument: %s\n", argv[i]);
            return 1;
        }
    }

    const char *dylib = getenv("ORT_DYLIB_PATH");
    if (!dylib || !*dylib) {
        fprintf(stderr, "ORT_DYLIB_PATH must be set to libonnxruntime.so\n");
        return 1;
    }
    if (!wake_dir || !pcm_path) {
        fprintf(stderr, "usage: ort-wake-probe --wake-dir <dir> --pcm <file> [--threshold 0.5] [--busy-threshold 0.8] [--busy] [--scores-out <file>] [--reference-scores <file>]\n");
        return 1;
    }

    void *handle = dlopen(dylib, RTLD_NOW | RTLD_LOCAL);
    if (!handle) {
        fprintf(stderr, "dlopen(%s) failed: %s\n", dylib, dlerror());
        return 2;
    }
    typedef const OrtApiBase *(ORT_API_CALL *GetApiBaseFn)(void);
    GetApiBaseFn get_api_base = (GetApiBaseFn)dlsym(handle, "OrtGetApiBase");
    if (!get_api_base) {
        fprintf(stderr, "OrtGetApiBase symbol missing in %s\n", dylib);
        return 2;
    }
    const OrtApiBase *base = get_api_base();
    const char *ort_version = base->GetVersionString();
    g_ort = base->GetApi(REQUIRED_ORT_API_VERSION);
    if (!g_ort) {
        fprintf(stderr, "ORT_API_VERSION %d unsupported by %s (version %s)\n",
                REQUIRED_ORT_API_VERSION, dylib, ort_version ? ort_version : "unknown");
        return 2;
    }

    double t_load_start = now_ms();
    Pipeline p;
    pipeline_open(&p, wake_dir);
    double load_ms = now_ms() - t_load_start;

    FILE *fp = fopen(pcm_path, "rb");
    if (!fp) {
        fprintf(stderr, "failed to open pcm %s: %s\n", pcm_path, strerror(errno));
        return 2;
    }
    if (fseek(fp, 0, SEEK_END) != 0) {
        fprintf(stderr, "fseek failed\n");
        return 2;
    }
    long file_bytes = ftell(fp);
    if (file_bytes < 0) {
        fprintf(stderr, "ftell failed\n");
        return 2;
    }
    rewind(fp);

    size_t hops = (size_t)file_bytes / (HOP * sizeof(int16_t));
    if (hops == 0) {
        fprintf(stderr, "pcm shorter than one 1280-sample hop (%ld bytes)\n", file_bytes);
        return 2;
    }
    int16_t *pcm = (int16_t *)malloc(hops * HOP * sizeof(int16_t));
    if (!pcm || fread(pcm, sizeof(int16_t), hops * HOP, fp) != hops * HOP) {
        fprintf(stderr, "failed reading pcm samples\n");
        return 2;
    }
    fclose(fp);

    FILE *scores_fp = NULL;
    if (scores_out_path) {
        scores_fp = fopen(scores_out_path, "w");
        if (!scores_fp) {
            fprintf(stderr, "failed to open scores output %s\n", scores_out_path);
            return 2;
        }
    }

    double *latencies = (double *)malloc(hops * sizeof(double));
    float *scores = (float *)malloc(hops * sizeof(float));
    if (!latencies || !scores) {
        fprintf(stderr, "oom\n");
        return 2;
    }

    bool armed = true;
    float active_threshold = busy ? busy_threshold : threshold;
    size_t fire_count = 0;
    long first_fire_hop = -1;
    size_t warmup_nonzero = 0;
    float min_score = 1.0f;
    float max_score = 0.0f;
    double sum_score = 0.0;
    size_t post_warmup_count = 0;

    for (size_t h = 0; h < hops; ++h) {
        double t0 = now_ms();
        float s = pipeline_score(&p, pcm + h * HOP, h == 0);
        double dt = now_ms() - t0;
        latencies[h] = dt;
        scores[h] = s;
        if (scores_fp) {
            fprintf(scores_fp, "%.6f\n", s);
        }
        if (h + 1 < p.steps) {
            if (s != 0.0f) {
                warmup_nonzero++;
            }
        } else {
            if (s < min_score) min_score = s;
            if (s > max_score) max_score = s;
            sum_score += (double)s;
            post_warmup_count++;
        }
        bool fire = armed && s >= active_threshold;
        if (fire) {
            armed = false;
            fire_count++;
            if (first_fire_hop < 0) first_fire_hop = (long)h;
        } else if (s < active_threshold * 0.6f) {
            armed = true;
        }
    }
    if (scores_fp) fclose(scores_fp);

    bool ref_checked = false;
    bool ref_passed = false;
    double ref_max_abs_err = 0.0;
    size_t ref_mismatches = 0;
    if (ref_scores_path) {
        ref_checked = true;
        FILE *rfp = fopen(ref_scores_path, "r");
        if (!rfp) {
            fprintf(stderr, "failed to open reference scores %s\n", ref_scores_path);
            return 2;
        }
        size_t idx = 0;
        char line[128];
        while (fgets(line, sizeof(line), rfp) && idx < hops) {
            float exp = strtof(line, NULL);
            if (idx >= 50) {
                double diff = fabs((double)scores[idx] - (double)exp);
                if (diff > ref_max_abs_err) ref_max_abs_err = diff;
                if (diff >= 0.02 || (scores[idx] >= 0.5f) != (exp >= 0.5f)) {
                    ref_mismatches++;
                }
            }
            idx++;
        }
        fclose(rfp);
        ref_passed = (idx == hops) && (hops > 50) && (ref_mismatches == 0);
    }

    double sum_lat = 0.0;
    for (size_t h = 0; h < hops; ++h) sum_lat += latencies[h];
    qsort(latencies, hops, sizeof(double), cmp_double);
    double mean_lat = sum_lat / (double)hops;
    double p50_lat = latencies[hops / 2];
    double p95_lat = latencies[(size_t)((double)(hops - 1) * 0.95)];
    double max_lat = latencies[hops - 1];
    double mean_score = post_warmup_count > 0 ? sum_score / (double)post_warmup_count : 0.0;
    if (post_warmup_count == 0) min_score = 0.0f;

    printf("{\n");
    printf("  \"ort_version\": \"%s\",\n", ort_version ? ort_version : "unknown");
    printf("  \"ort_api_version\": %d,\n", REQUIRED_ORT_API_VERSION);
    printf("  \"classifier_steps\": %zu,\n", p.steps);
    printf("  \"load_ms\": %.3f,\n", load_ms);
    printf("  \"hops\": %zu,\n", hops);
    printf("  \"audio_duration_s\": %.3f,\n", (double)hops * 0.08);
    printf("  \"busy\": %s,\n", busy ? "true" : "false");
    printf("  \"threshold\": %.4f,\n", active_threshold);
    printf("  \"warmup_nonzero_hops\": %zu,\n", warmup_nonzero);
    printf("  \"post_warmup_hops\": %zu,\n", post_warmup_count);
    printf("  \"score_min\": %.6f,\n", min_score);
    printf("  \"score_mean\": %.6f,\n", mean_score);
    printf("  \"score_max\": %.6f,\n", max_score);
    printf("  \"fired\": %s,\n", fire_count > 0 ? "true" : "false");
    printf("  \"fire_count\": %zu,\n", fire_count);
    printf("  \"first_fire_hop\": %ld,\n", first_fire_hop);
    printf("  \"hop_latency_ms\": {\"mean\": %.3f, \"p50\": %.3f, \"p95\": %.3f, \"max\": %.3f},\n",
           mean_lat, p50_lat, p95_lat, max_lat);
    printf("  \"rtf\": %.5f,\n", mean_lat / 80.0);
    printf("  \"reference_parity\": {\"checked\": %s, \"passed\": %s, \"max_abs_err\": %.6f, \"mismatches\": %zu}\n",
           ref_checked ? "true" : "false",
           ref_passed ? "true" : "false",
           ref_max_abs_err,
           ref_mismatches);
    printf("}\n");

    free(latencies);
    free(scores);
    free(pcm);
    free(p.embeddings);
    free(p.mel_in);
    free(p.mel_out);
    free(p.enc_in);
    free(p.enc_out);
    free(p.cls_in);
    free(p.cls_out);
    g_ort->ReleaseSession(p.mel);
    g_ort->ReleaseSession(p.encoder);
    g_ort->ReleaseSession(p.classifier);
    g_ort->ReleaseMemoryInfo(p.mem);
    g_ort->ReleaseSessionOptions(p.opts);
    g_ort->ReleaseEnv(p.env);
    dlclose(handle);
    return 0;
}
