#ifndef MOZART_AEC3_H
#define MOZART_AEC3_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct mozart_aec3 mozart_aec3_t;

typedef struct {
    int    has_delay;
    int    delay_ms;
    int    has_erle;
    double erle_db;
    int    has_erl;
    double erl_db;
    int    has_divergent;
    double divergent_fraction;
    int    has_residual_echo;
    double residual_echo_likelihood;
    int    has_delay_median;
    int    delay_median_ms;
    int    has_delay_std;
    int    delay_std_ms;
} mozart_aec3_stats_t;

mozart_aec3_t* mozart_aec3_create(int sample_rate_hz);
void mozart_aec3_destroy(mozart_aec3_t* handle);
int mozart_aec3_reset(mozart_aec3_t* handle);
int mozart_aec3_process_reverse(mozart_aec3_t* handle, const float* pcm, size_t samples);
int mozart_aec3_process_capture(mozart_aec3_t* handle, float* pcm, size_t samples, int stream_delay_ms);
int mozart_aec3_get_stats(const mozart_aec3_t* handle, mozart_aec3_stats_t* out);

#ifdef __cplusplus
}
#endif

#endif // MOZART_AEC3_H
