/**
 * @file edge_processing.c
 * @brief ADR-039 Edge Intelligence — dual-core CSI processing pipeline.
 *
 * Core 0 (WiFi path): Pushes raw CSI frames into lock-free SPSC ring buffer.
 * Second core when present (DSP task): pops frames, runs signal processing pipeline.
 * On unicore targets (e.g. ESP32-C6), the DSP task is pinned to core 0.
 *   1. Phase extraction from I/Q pairs
 *   2. Phase unwrapping (continuous phase)
 *   3. Welford variance tracking per subcarrier
 *   4. Top-K subcarrier selection by variance
 *   5. Biquad IIR bandpass → breathing (0.1-0.5 Hz), heart rate (0.8-2.0 Hz)
 *   6. Zero-crossing BPM estimation
 *   7. Presence detection (adaptive or fixed threshold)
 *   8. Fall detection (phase acceleration)
 *   9. Multi-person vitals via subcarrier group clustering
 *  10. Delta compression (XOR + RLE) for bandwidth reduction
 *  11. Vitals packet broadcast (magic 0xC5110002)
 */

#include "edge_processing.h"
#include "nvs_config.h"
#include "csi_collector.h"  /* csi_collector_get_node_id() - defensive #390 */
#include "mmwave_sensor.h"

/* Runtime config — declared in main.c, loaded from NVS at boot. */
extern nvs_config_t g_nvs_config;
#include "wasm_runtime.h"
#include "stream_sender.h"

#include <math.h>
#include <string.h>
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "nvs.h"
#include "sdkconfig.h"

static const char *TAG = "edge_proc";

#define EDGE_HUMAN_FFT_LEN          EDGE_PHASE_HISTORY_LEN
#define EDGE_HUMAN_FREQ_MIN_HZ      0.1f
#define EDGE_HUMAN_FREQ_MAX_HZ      2.0f
#define EDGE_HUMAN_BAND_RATIO_MIN   0.08f
#define EDGE_HUMAN_PEAK_RATIO_MIN   4.0f
#define EDGE_NVS_RECALIB_CHECK_MS   5000
#define EDGE_DIAG_LOG_INTERVAL_US   30000000LL

/* ======================================================================
 * SPSC Ring Buffer (lock-free, single-producer single-consumer)
 * ====================================================================== */

static edge_ring_buf_t s_ring;
static uint32_t s_ring_drops;  /* Frames dropped due to full ring buffer. */

/* Scratch buffers for BPM estimation — moved from stack to static to avoid
 * stack overflow.  process_frame + update_multi_person_vitals combined used
 * ~6.5-7.5 KB of the 8 KB task stack.  These save ~4 KB of stack. */
static float s_scratch_br[EDGE_PHASE_HISTORY_LEN];
static float s_scratch_hr[EDGE_PHASE_HISTORY_LEN];
static float s_fft_real[EDGE_HUMAN_FFT_LEN];
static float s_fft_imag[EDGE_HUMAN_FFT_LEN];
static float s_baseline_sort[EDGE_BASELINE_WINDOW_FRAMES];

static inline bool ring_push(const uint8_t *iq, uint16_t len,
                             int8_t rssi, uint8_t channel)
{
    uint32_t next = (s_ring.head + 1) % EDGE_RING_SLOTS;
    if (next == s_ring.tail) {
        s_ring_drops++;
        return false;  /* Full — drop frame. */
    }

    edge_ring_slot_t *slot = &s_ring.slots[s_ring.head];
    uint16_t copy_len = (len > EDGE_MAX_IQ_BYTES) ? EDGE_MAX_IQ_BYTES : len;
    memcpy(slot->iq_data, iq, copy_len);
    slot->iq_len = copy_len;
    slot->rssi = rssi;
    slot->channel = channel;
    slot->timestamp_us = (uint32_t)(esp_timer_get_time() & 0xFFFFFFFF);

    /* Memory barrier: ensure slot data is visible before advancing head. */
    __sync_synchronize();
    s_ring.head = next;
    return true;
}

static inline bool ring_pop(edge_ring_slot_t *out)
{
    if (s_ring.tail == s_ring.head) {
        return false;  /* Empty. */
    }

    memcpy(out, &s_ring.slots[s_ring.tail], sizeof(edge_ring_slot_t));

    __sync_synchronize();
    s_ring.tail = (s_ring.tail + 1) % EDGE_RING_SLOTS;
    return true;
}

/* ======================================================================
 * Biquad IIR Filter
 * ====================================================================== */

/**
 * Design a 2nd-order Butterworth bandpass biquad.
 *
 * @param bq   Output biquad state.
 * @param fs   Sampling frequency (Hz).
 * @param f_lo Low cutoff frequency (Hz).
 * @param f_hi High cutoff frequency (Hz).
 */
static void biquad_bandpass_design(edge_biquad_t *bq, float fs,
                                   float f_lo, float f_hi)
{
    float w0 = 2.0f * M_PI * (f_lo + f_hi) / 2.0f / fs;
    float bw = 2.0f * M_PI * (f_hi - f_lo) / fs;
    float alpha = sinf(w0) * sinhf(logf(2.0f) / 2.0f * bw / sinf(w0));

    float a0_inv = 1.0f / (1.0f + alpha);
    bq->b0 =  alpha * a0_inv;
    bq->b1 =  0.0f;
    bq->b2 = -alpha * a0_inv;
    bq->a1 = -2.0f * cosf(w0) * a0_inv;
    bq->a2 =  (1.0f - alpha) * a0_inv;

    bq->x1 = bq->x2 = 0.0f;
    bq->y1 = bq->y2 = 0.0f;
}

static inline float biquad_process(edge_biquad_t *bq, float x)
{
    float y = bq->b0 * x + bq->b1 * bq->x1 + bq->b2 * bq->x2
            - bq->a1 * bq->y1 - bq->a2 * bq->y2;
    bq->x2 = bq->x1;
    bq->x1 = x;
    bq->y2 = bq->y1;
    bq->y1 = y;
    return y;
}

/* ======================================================================
 * Phase Extraction and Unwrapping
 * ====================================================================== */

/** Extract phase (radians) from an I/Q pair at byte offset. */
static inline float extract_phase(const uint8_t *iq, uint16_t idx)
{
    int8_t i_val = (int8_t)iq[idx * 2];
    int8_t q_val = (int8_t)iq[idx * 2 + 1];
    return atan2f((float)q_val, (float)i_val);
}

/** Unwrap phase to maintain continuity (avoid 2*pi jumps). */
static inline float unwrap_phase(float prev, float curr)
{
    float diff = curr - prev;
    if (diff > M_PI)       diff -= 2.0f * M_PI;
    else if (diff < -M_PI) diff += 2.0f * M_PI;
    return prev + diff;
}

/* ======================================================================
 * Welford Running Statistics
 * ====================================================================== */

static inline void welford_reset(edge_welford_t *w)
{
    w->mean = 0.0;
    w->m2   = 0.0;
    w->count = 0;
}

static inline void welford_update(edge_welford_t *w, double x)
{
    w->count++;
    double delta = x - w->mean;
    w->mean += delta / (double)w->count;
    double delta2 = x - w->mean;
    w->m2 += delta * delta2;
}

static inline double welford_variance(const edge_welford_t *w)
{
    return (w->count > 1) ? (w->m2 / (double)(w->count - 1)) : 0.0;
}

/* ======================================================================
 * Zero-Crossing BPM Estimation
 * ====================================================================== */

/**
 * Estimate BPM from a filtered signal using positive zero-crossings.
 *
 * @param history     Signal buffer (filtered phase).
 * @param len         Number of samples.
 * @param sample_rate Sampling rate in Hz.
 * @return Estimated BPM, or 0 if insufficient crossings.
 */
static float estimate_bpm_zero_crossing(const float *history, uint16_t len,
                                        float sample_rate)
{
    if (len < 4) return 0.0f;

    uint16_t crossings[128];
    uint16_t n_cross = 0;

    for (uint16_t i = 1; i < len && n_cross < 128; i++) {
        if (history[i - 1] <= 0.0f && history[i] > 0.0f) {
            crossings[n_cross++] = i;
        }
    }

    if (n_cross < 2) return 0.0f;

    /* Average period from consecutive crossings. */
    float total_period = 0.0f;
    for (uint16_t i = 1; i < n_cross; i++) {
        total_period += (float)(crossings[i] - crossings[i - 1]);
    }
    float avg_period_samples = total_period / (float)(n_cross - 1);

    if (avg_period_samples < 1.0f) return 0.0f;

    float freq_hz = sample_rate / avg_period_samples;
    return freq_hz * 60.0f;  /* Hz to BPM. */
}

/* ======================================================================
 * DSP Pipeline State
 * ====================================================================== */

/** Edge processing configuration. */
static edge_config_t s_cfg;

/** Per-subcarrier running variance (for top-K selection). */
static edge_welford_t s_subcarrier_var[EDGE_MAX_SUBCARRIERS];

/** Previous phase per subcarrier (for unwrapping). */
static float s_prev_phase[EDGE_MAX_SUBCARRIERS];
static bool  s_phase_initialized;

/** Top-K subcarrier indices (sorted by variance, descending). */
static uint8_t s_top_k[EDGE_TOP_K];
static uint8_t s_top_k_count;

/** Phase history for the primary (highest-variance) subcarrier. */
static float s_phase_history[EDGE_PHASE_HISTORY_LEN];
static uint16_t s_history_len;
static uint16_t s_history_idx;

/** Biquad filters for breathing and heart rate. */
static edge_biquad_t s_bq_breathing;
static edge_biquad_t s_bq_heartrate;

/** Filtered signal histories for BPM estimation. */
static float s_breathing_filtered[EDGE_PHASE_HISTORY_LEN];
static float s_heartrate_filtered[EDGE_PHASE_HISTORY_LEN];

/** Latest vitals state. */
static float    s_breathing_bpm;
static float    s_heartrate_bpm;
static float    s_motion_energy;
static float    s_presence_score;
static bool     s_presence_detected;
static bool     s_fall_detected;
static int8_t   s_latest_rssi;
static uint32_t s_frame_count;

/** Previous phase velocity for fall detection (acceleration). */
static float s_prev_phase_velocity;

/** Fall detection debounce state (issue #263). */
static uint8_t  s_fall_consec_count;   /**< Consecutive frames above threshold. */
static int64_t  s_fall_last_alert_us;  /**< Timestamp of last fall alert (debounce). */

/** Rolling ambient baseline state. */
static bool     s_calibrated;
static float    s_baseline_samples[EDGE_BASELINE_WINDOW_FRAMES];
static uint16_t s_baseline_count;
static uint16_t s_baseline_idx;
static int64_t  s_last_baseline_update_us;
static float    s_baseline_median;
static float    s_baseline_mad_sigma;
static float    s_adaptive_threshold;

/** Manual recalibration state, triggered by API or NVS key "recalib"=1. */
static volatile bool s_recalib_requested;
static bool     s_recalib_active;
static int64_t  s_recalib_start_us;
static float    s_recalib_samples[EDGE_BASELINE_WINDOW_FRAMES];
static uint16_t s_recalib_count;
static int64_t  s_last_recalib_nvs_check_us;

/** Presence RF-classification diagnostics. */
static int64_t s_last_no_human_log_us;
static int64_t s_last_threshold_log_us;

/** Last vitals send timestamp. */
static int64_t s_last_vitals_send_us;

/** Delta compression state. */
static uint8_t s_prev_iq[EDGE_MAX_IQ_BYTES];
static uint16_t s_prev_iq_len;
static bool s_has_prev_iq;

/** ADR-069: Feature vector sequence counter. */
static uint16_t s_feature_seq;

/** Multi-person vitals state. */
static edge_person_vitals_t s_persons[EDGE_MAX_PERSONS];
static edge_biquad_t s_person_bq_br[EDGE_MAX_PERSONS];
static edge_biquad_t s_person_bq_hr[EDGE_MAX_PERSONS];
static float s_person_br_filt[EDGE_MAX_PERSONS][EDGE_PHASE_HISTORY_LEN];
static float s_person_hr_filt[EDGE_MAX_PERSONS][EDGE_PHASE_HISTORY_LEN];

/** Latest vitals packet (thread-safe via volatile copy). */
static volatile edge_vitals_pkt_t s_latest_pkt;
static volatile bool s_pkt_valid;

/* ======================================================================
 * Top-K Subcarrier Selection
 * ====================================================================== */

/**
 * Select top-K subcarriers by variance (descending).
 * Uses partial insertion sort — O(n*K) which is fine for n <= 128.
 */
static void update_top_k(uint16_t n_subcarriers)
{
    uint8_t k = s_cfg.top_k_count;
    if (k > EDGE_TOP_K) k = EDGE_TOP_K;
    if (k > n_subcarriers) k = (uint8_t)n_subcarriers;

    /* Simple selection: find K largest variances. */
    bool used[EDGE_MAX_SUBCARRIERS];
    memset(used, 0, sizeof(used));

    for (uint8_t ki = 0; ki < k; ki++) {
        double best_var = -1.0;
        uint8_t best_idx = 0;

        for (uint16_t sc = 0; sc < n_subcarriers; sc++) {
            if (!used[sc]) {
                double v = welford_variance(&s_subcarrier_var[sc]);
                if (v > best_var) {
                    best_var = v;
                    best_idx = (uint8_t)sc;
                }
            }
        }

        s_top_k[ki] = best_idx;
        used[best_idx] = true;
    }

    s_top_k_count = k;
}

/* ======================================================================
 * Rolling Ambient Baseline and RF Presence Classification
 * ====================================================================== */

typedef struct {
    bool  has_component;
    float band_ratio;
    float peak_hz;
    float peak_ratio;
} edge_human_freq_result_t;

static void sort_float_ascending(float *values, uint16_t count)
{
    for (uint16_t i = 1; i < count; i++) {
        float key = values[i];
        uint16_t j = i;
        while (j > 0 && values[j - 1] > key) {
            values[j] = values[j - 1];
            j--;
        }
        values[j] = key;
    }
}

static float median_from_samples(const float *samples, uint16_t count)
{
    if (count == 0) return 0.0f;

    for (uint16_t i = 0; i < count; i++) {
        s_baseline_sort[i] = samples[i];
    }
    sort_float_ascending(s_baseline_sort, count);

    uint16_t mid = count / 2;
    if ((count & 1U) != 0) {
        return s_baseline_sort[mid];
    }
    return 0.5f * (s_baseline_sort[mid - 1] + s_baseline_sort[mid]);
}

static bool baseline_apply_samples(const float *samples, uint16_t count,
                                   const char *reason)
{
    if (count == 0) return false;

    float median = median_from_samples(samples, count);
    for (uint16_t i = 0; i < count; i++) {
        s_baseline_sort[i] = fabsf(samples[i] - median);
    }
    sort_float_ascending(s_baseline_sort, count);

    uint16_t mid = count / 2;
    float mad;
    if ((count & 1U) != 0) {
        mad = s_baseline_sort[mid];
    } else {
        mad = 0.5f * (s_baseline_sort[mid - 1] + s_baseline_sort[mid]);
    }

    float robust_sigma = mad * 1.4826f;
    if (robust_sigma < 0.001f) {
        robust_sigma = 0.001f;
    }

    s_baseline_median = median;
    s_baseline_mad_sigma = robust_sigma;
    s_adaptive_threshold = median + EDGE_CALIB_SIGMA_MULT * robust_sigma;
    if (s_adaptive_threshold < 0.01f) {
        s_adaptive_threshold = 0.01f;
    }
    s_calibrated = true;

    ESP_LOGI(TAG, "Baseline updated (%s): median=%.4f sigma=%.4f "
             "threshold=%.4f samples=%u",
             reason, s_baseline_median, s_baseline_mad_sigma,
             s_adaptive_threshold, (unsigned)count);
    return true;
}

static void baseline_add_quiet_sample(float motion)
{
    s_baseline_samples[s_baseline_idx] = motion;
    s_baseline_idx = (s_baseline_idx + 1) % EDGE_BASELINE_WINDOW_FRAMES;
    if (s_baseline_count < EDGE_BASELINE_WINDOW_FRAMES) {
        s_baseline_count++;
    }
}

static void rolling_baseline_update(float motion, int64_t now_us)
{
    if (s_cfg.presence_thresh != 0.0f || s_recalib_active) return;
    if (motion >= EDGE_BASELINE_QUIET_MOTION) return;

    baseline_add_quiet_sample(motion);
    if (s_baseline_count < EDGE_BASELINE_WINDOW_FRAMES) return;

    int64_t update_interval_us = (int64_t)EDGE_BASELINE_UPDATE_MS * 1000;
    if (s_last_baseline_update_us == 0
        || (now_us - s_last_baseline_update_us) >= update_interval_us)
    {
        if (baseline_apply_samples(s_baseline_samples, s_baseline_count,
                                   "rolling median")) {
            s_last_baseline_update_us = now_us;
        }
    }
}

static void recalibration_begin(int64_t now_us)
{
    s_recalib_active = true;
    s_recalib_start_us = now_us;
    s_recalib_count = 0;
    ESP_LOGI(TAG, "Manual recalibration started: %ums window",
             (unsigned)EDGE_RECALIBRATION_MS);
}

static void recalibration_update(float motion, int64_t now_us)
{
    if (s_recalib_requested) {
        s_recalib_requested = false;
        recalibration_begin(now_us);
    }

    if (!s_recalib_active) return;

    if (motion < EDGE_BASELINE_QUIET_MOTION
        && s_recalib_count < EDGE_BASELINE_WINDOW_FRAMES)
    {
        s_recalib_samples[s_recalib_count++] = motion;
    }

    int64_t recalib_us = (int64_t)EDGE_RECALIBRATION_MS * 1000;
    if ((now_us - s_recalib_start_us) < recalib_us) return;

    if (s_cfg.presence_thresh != 0.0f) {
        ESP_LOGI(TAG, "Manual recalibration complete but fixed threshold %.4f "
                 "is configured; adaptive baseline unchanged",
                 s_cfg.presence_thresh);
    } else if (s_recalib_count > 0) {
        baseline_apply_samples(s_recalib_samples, s_recalib_count,
                               "manual 30s recalibration");

        uint16_t copy_count = s_recalib_count;
        if (copy_count > EDGE_BASELINE_WINDOW_FRAMES) {
            copy_count = EDGE_BASELINE_WINDOW_FRAMES;
        }
        memcpy(s_baseline_samples, s_recalib_samples,
               copy_count * sizeof(s_baseline_samples[0]));
        s_baseline_count = copy_count;
        s_baseline_idx = copy_count % EDGE_BASELINE_WINDOW_FRAMES;
        s_last_baseline_update_us = now_us;
    } else {
        ESP_LOGI(TAG, "Manual recalibration complete: no quiet samples, "
                 "baseline unchanged");
    }

    s_recalib_active = false;
}

static void edge_check_nvs_recalibration(int64_t now_us)
{
    int64_t check_interval_us = (int64_t)EDGE_NVS_RECALIB_CHECK_MS * 1000;
    if (s_last_recalib_nvs_check_us != 0
        && (now_us - s_last_recalib_nvs_check_us) < check_interval_us)
    {
        return;
    }
    s_last_recalib_nvs_check_us = now_us;

    nvs_handle_t handle;
    if (nvs_open("csi_cfg", NVS_READWRITE, &handle) != ESP_OK) {
        return;
    }

    bool requested = false;
    uint8_t u8_val = 0;
    uint32_t u32_val = 0;
    if (nvs_get_u8(handle, "recalib", &u8_val) == ESP_OK) {
        requested = (u8_val == 1);
    } else if (nvs_get_u32(handle, "recalib", &u32_val) == ESP_OK) {
        requested = (u32_val == 1);
    }

    if (requested) {
        edge_request_recalibration();
        ESP_LOGI(TAG, "NVS recalib=1 consumed; starting background recalibration");
        if (nvs_erase_key(handle, "recalib") == ESP_OK) {
            nvs_commit(handle);
        }
    }

    nvs_close(handle);
}

static void log_threshold_if_due(float threshold, int64_t now_us)
{
    if (s_last_threshold_log_us != 0
        && (now_us - s_last_threshold_log_us) < EDGE_DIAG_LOG_INTERVAL_US)
    {
        return;
    }
    s_last_threshold_log_us = now_us;

    ESP_LOGI(TAG, "Presence threshold current=%.4f calibrated=%s "
             "baseline_median=%.4f baseline_sigma=%.4f",
             threshold,
             s_calibrated ? "yes" : "no",
             s_baseline_median,
             s_baseline_mad_sigma);
}

static void fft_inplace_256(float *real, float *imag)
{
    const uint16_t n = EDGE_HUMAN_FFT_LEN;

    for (uint16_t i = 1, j = 0; i < n; i++) {
        uint16_t bit = n >> 1;
        for (; (j & bit) != 0; bit >>= 1) {
            j ^= bit;
        }
        j ^= bit;

        if (i < j) {
            float tr = real[i];
            float ti = imag[i];
            real[i] = real[j];
            imag[i] = imag[j];
            real[j] = tr;
            imag[j] = ti;
        }
    }

    for (uint16_t len = 2; len <= n; len <<= 1) {
        float ang = -2.0f * M_PI / (float)len;
        float wlen_r = cosf(ang);
        float wlen_i = sinf(ang);
        uint16_t half = len >> 1;

        for (uint16_t i = 0; i < n; i += len) {
            float wr = 1.0f;
            float wi = 0.0f;
            for (uint16_t j = 0; j < half; j++) {
                uint16_t even = i + j;
                uint16_t odd = even + half;
                float vr = real[odd] * wr - imag[odd] * wi;
                float vi = real[odd] * wi + imag[odd] * wr;
                float ur = real[even];
                float ui = imag[even];

                real[even] = ur + vr;
                imag[even] = ui + vi;
                real[odd] = ur - vr;
                imag[odd] = ui - vi;

                float next_wr = wr * wlen_r - wi * wlen_i;
                wi = wr * wlen_i + wi * wlen_r;
                wr = next_wr;
            }
        }
    }
}

static edge_human_freq_result_t detect_human_frequency_component(float sample_rate)
{
    edge_human_freq_result_t result = {0};
    if (s_history_len < EDGE_HUMAN_FFT_LEN) {
        result.has_component = true;
        return result;
    }

    float mean = 0.0f;
    for (uint16_t i = 0; i < EDGE_HUMAN_FFT_LEN; i++) {
        uint16_t ri = (s_history_idx + EDGE_PHASE_HISTORY_LEN
                       - EDGE_HUMAN_FFT_LEN + i) % EDGE_PHASE_HISTORY_LEN;
        mean += s_phase_history[ri];
    }
    mean /= (float)EDGE_HUMAN_FFT_LEN;

    for (uint16_t i = 0; i < EDGE_HUMAN_FFT_LEN; i++) {
        uint16_t ri = (s_history_idx + EDGE_PHASE_HISTORY_LEN
                       - EDGE_HUMAN_FFT_LEN + i) % EDGE_PHASE_HISTORY_LEN;
        float hann = 0.5f - 0.5f * cosf((2.0f * M_PI * (float)i)
                                        / (float)(EDGE_HUMAN_FFT_LEN - 1));
        s_fft_real[i] = (s_phase_history[ri] - mean) * hann;
        s_fft_imag[i] = 0.0f;
    }

    fft_inplace_256(s_fft_real, s_fft_imag);

    float total_power = 0.0f;
    float band_power = 0.0f;
    float max_band_power = 0.0f;
    uint16_t max_band_bin = 0;
    uint16_t bin_count = (EDGE_HUMAN_FFT_LEN / 2) - 1;

    for (uint16_t bin = 1; bin < EDGE_HUMAN_FFT_LEN / 2; bin++) {
        float power = s_fft_real[bin] * s_fft_real[bin]
                    + s_fft_imag[bin] * s_fft_imag[bin];
        float hz = ((float)bin * sample_rate) / (float)EDGE_HUMAN_FFT_LEN;
        total_power += power;

        if (hz >= EDGE_HUMAN_FREQ_MIN_HZ && hz <= EDGE_HUMAN_FREQ_MAX_HZ) {
            band_power += power;
            if (power > max_band_power) {
                max_band_power = power;
                max_band_bin = bin;
            }
        }
    }

    if (total_power <= 0.000001f) {
        result.has_component = false;
        return result;
    }

    float mean_power = total_power / (float)bin_count;
    result.band_ratio = band_power / total_power;
    result.peak_ratio = max_band_power / (mean_power + 0.000001f);
    result.peak_hz = ((float)max_band_bin * sample_rate)
                   / (float)EDGE_HUMAN_FFT_LEN;
    result.has_component =
        (result.band_ratio >= EDGE_HUMAN_BAND_RATIO_MIN)
        || (result.peak_ratio >= EDGE_HUMAN_PEAK_RATIO_MIN);

    return result;
}

/* ======================================================================
 * Delta Compression (XOR + RLE)
 * ====================================================================== */

/**
 * Delta-compress I/Q data relative to previous frame.
 * Format: [XOR'd bytes], then RLE-encoded.
 *
 * @param curr       Current I/Q data.
 * @param len        Length of I/Q data.
 * @param out        Output compressed buffer.
 * @param out_max    Max output buffer size.
 * @return Compressed size, or 0 if compression would expand the data.
 */
static uint16_t delta_compress(const uint8_t *curr, uint16_t len,
                               uint8_t *out, uint16_t out_max)
{
    if (!s_has_prev_iq || len != s_prev_iq_len || len == 0) {
        return 0;
    }

    /* XOR delta. */
    uint8_t xor_buf[EDGE_MAX_IQ_BYTES];
    for (uint16_t i = 0; i < len; i++) {
        xor_buf[i] = curr[i] ^ s_prev_iq[i];
    }

    /* RLE encode: [value, count] pairs.
     * If count > 255, emit multiple pairs. */
    uint16_t out_idx = 0;

    uint16_t i = 0;
    while (i < len) {
        uint8_t val = xor_buf[i];
        uint16_t run = 1;
        while (i + run < len && xor_buf[i + run] == val && run < 255) {
            run++;
        }

        if (out_idx + 2 > out_max) return 0;  /* Would overflow. */
        out[out_idx++] = val;
        out[out_idx++] = (uint8_t)run;
        i += run;
    }

    /* Only use compression if it actually saves space. */
    if (out_idx >= len) {
        return 0;
    }

    return out_idx;
}

/**
 * Send a compressed CSI frame (magic 0xC5110005, reassigned from 0xC5110003 for ADR-069).
 *
 * Header:
 *   [0..3]   Magic 0xC5110005 (LE)
 *   [4]      Node ID
 *   [5]      Channel
 *   [6..7]   Original I/Q length (LE u16)
 *   [8..9]   Compressed length (LE u16)
 *   [10..]   Compressed data
 */
static void send_compressed_frame(const uint8_t *iq_data, uint16_t iq_len,
                                  uint8_t channel)
{
    uint8_t comp_buf[EDGE_MAX_IQ_BYTES];
    uint16_t comp_len = delta_compress(iq_data, iq_len,
                                       comp_buf, sizeof(comp_buf));
    if (comp_len == 0) {
        /* Compression didn't help — skip sending compressed version. */
        goto store_prev;
    }

    /* Build compressed frame packet. */
    uint16_t pkt_size = 10 + comp_len;
    uint8_t pkt[10 + EDGE_MAX_IQ_BYTES];

    uint32_t magic = EDGE_COMPRESSED_MAGIC;
    memcpy(&pkt[0], &magic, 4);

    pkt[4] = csi_collector_get_node_id();  /* #390: defensive copy */
    pkt[5] = channel;
    memcpy(&pkt[6], &iq_len, 2);
    memcpy(&pkt[8], &comp_len, 2);
    memcpy(&pkt[10], comp_buf, comp_len);

    stream_sender_send(pkt, pkt_size);

    ESP_LOGD(TAG, "Compressed frame: %u → %u bytes (%.0f%% reduction)",
             iq_len, comp_len,
             (1.0f - (float)comp_len / (float)iq_len) * 100.0f);

store_prev:
    /* Store current frame as reference for next delta. */
    memcpy(s_prev_iq, iq_data, iq_len);
    s_prev_iq_len = iq_len;
    s_has_prev_iq = true;
}

/* ======================================================================
 * Multi-Person Vitals
 * ====================================================================== */

/**
 * Update multi-person vitals by assigning top-K subcarriers to person groups.
 *
 * Division strategy: top-K subcarriers are evenly divided among
 * up to EDGE_MAX_PERSONS groups. Each group tracks independent
 * phase history and BPM estimation.
 */
static void update_multi_person_vitals(const uint8_t *iq_data, uint16_t n_sc,
                                       float sample_rate)
{
    if (s_top_k_count < 2) return;

    /* Determine number of active persons based on available subcarriers. */
    uint8_t n_persons = s_top_k_count / 2;
    if (n_persons > EDGE_MAX_PERSONS) n_persons = EDGE_MAX_PERSONS;
    if (n_persons < 1) n_persons = 1;

    uint8_t subs_per_person = s_top_k_count / n_persons;

    for (uint8_t p = 0; p < n_persons; p++) {
        edge_person_vitals_t *pv = &s_persons[p];
        pv->active = true;
        pv->subcarrier_idx = s_top_k[p * subs_per_person];

        /* Average phase across this person's subcarrier group. */
        float avg_phase = 0.0f;
        uint8_t count = 0;
        for (uint8_t s = 0; s < subs_per_person; s++) {
            uint8_t sc_idx = s_top_k[p * subs_per_person + s];
            if (sc_idx < n_sc) {
                avg_phase += extract_phase(iq_data, sc_idx);
                count++;
            }
        }
        if (count > 0) avg_phase /= (float)count;

        /* Unwrap and store in history. */
        if (pv->history_len > 0) {
            uint16_t prev_idx = (pv->history_idx + EDGE_PHASE_HISTORY_LEN - 1)
                                % EDGE_PHASE_HISTORY_LEN;
            avg_phase = unwrap_phase(pv->phase_history[prev_idx], avg_phase);
        }

        pv->phase_history[pv->history_idx] = avg_phase;
        pv->history_idx = (pv->history_idx + 1) % EDGE_PHASE_HISTORY_LEN;
        if (pv->history_len < EDGE_PHASE_HISTORY_LEN) pv->history_len++;

        /* Filter and estimate BPM. */
        float br_val = biquad_process(&s_person_bq_br[p], avg_phase);
        float hr_val = biquad_process(&s_person_bq_hr[p], avg_phase);

        uint16_t idx = (pv->history_idx + EDGE_PHASE_HISTORY_LEN - 1)
                       % EDGE_PHASE_HISTORY_LEN;
        s_person_br_filt[p][idx] = br_val;
        s_person_hr_filt[p][idx] = hr_val;

        /* Estimate BPM when we have enough history. */
        if (pv->history_len >= 64) {
            /* Build contiguous buffer (reuse static scratch to save ~2 KB stack). */
            uint16_t buf_len = pv->history_len;

            for (uint16_t i = 0; i < buf_len; i++) {
                uint16_t ri = (pv->history_idx + EDGE_PHASE_HISTORY_LEN
                               - buf_len + i) % EDGE_PHASE_HISTORY_LEN;
                s_scratch_br[i] = s_person_br_filt[p][ri];
                s_scratch_hr[i] = s_person_hr_filt[p][ri];
            }

            float br = estimate_bpm_zero_crossing(s_scratch_br, buf_len, sample_rate);
            float hr = estimate_bpm_zero_crossing(s_scratch_hr, buf_len, sample_rate);

            /* Sanity clamp. */
            if (br >= 6.0f && br <= 40.0f) pv->breathing_bpm = br;
            if (hr >= 40.0f && hr <= 180.0f) pv->heartrate_bpm = hr;
        }
    }

    /* Mark remaining persons as inactive. */
    for (uint8_t p = n_persons; p < EDGE_MAX_PERSONS; p++) {
        s_persons[p].active = false;
    }
}

/* ======================================================================
 * Vitals Packet Sending
 * ====================================================================== */

static void send_vitals_packet(void)
{
    edge_vitals_pkt_t pkt;
    memset(&pkt, 0, sizeof(pkt));

    pkt.magic = EDGE_VITALS_MAGIC;
    pkt.node_id = csi_collector_get_node_id();  /* #390: defensive copy */

    pkt.flags = 0;
    if (s_presence_detected) pkt.flags |= 0x01;
    if (s_fall_detected)     pkt.flags |= 0x02;
    if (s_motion_energy > 0.01f) pkt.flags |= 0x04;

    pkt.breathing_rate = (uint16_t)(s_breathing_bpm * 100.0f);
    pkt.heartrate = (uint32_t)(s_heartrate_bpm * 10000.0f);
    pkt.rssi = s_latest_rssi;

    /* Count active persons. */
    uint8_t n_active = 0;
    for (uint8_t p = 0; p < EDGE_MAX_PERSONS; p++) {
        if (s_persons[p].active) n_active++;
    }
    pkt.n_persons = n_active;

    pkt.motion_energy = s_motion_energy;
    pkt.presence_score = s_presence_score;
    pkt.timestamp_ms = (uint32_t)(esp_timer_get_time() / 1000);

    /* Update thread-safe copy. */
    s_latest_pkt = pkt;
    s_pkt_valid = true;

    /* ADR-063: If mmWave is active, send fused 48-byte packet instead. */
    mmwave_state_t mw;
    if (mmwave_sensor_get_state(&mw) && mw.detected) {
        edge_fused_vitals_pkt_t fpkt;
        memset(&fpkt, 0, sizeof(fpkt));

        fpkt.magic = EDGE_FUSED_MAGIC;
        fpkt.node_id = pkt.node_id;
        fpkt.flags = pkt.flags;
        if (mw.person_present) fpkt.flags |= 0x08;  /* Bit3 = mmwave_present */
        fpkt.rssi = pkt.rssi;
        fpkt.n_persons = pkt.n_persons;
        fpkt.mmwave_type = (uint8_t)mw.type;
        fpkt.motion_energy = pkt.motion_energy;
        fpkt.presence_score = pkt.presence_score;
        fpkt.timestamp_ms = pkt.timestamp_ms;

        /* Kalman-style fusion: prefer mmWave when available, CSI as fallback. */
        if (mw.heart_rate_bpm > 0.0f && s_heartrate_bpm > 0.0f) {
            /* Weighted average: mmWave 80%, CSI 20% (mmWave is more accurate). */
            float fused_hr = mw.heart_rate_bpm * 0.8f + s_heartrate_bpm * 0.2f;
            fpkt.heartrate = (uint32_t)(fused_hr * 10000.0f);
            fpkt.fusion_confidence = 90;
        } else if (mw.heart_rate_bpm > 0.0f) {
            fpkt.heartrate = (uint32_t)(mw.heart_rate_bpm * 10000.0f);
            fpkt.fusion_confidence = 85;
        } else {
            fpkt.heartrate = pkt.heartrate;
            fpkt.fusion_confidence = 50;
        }

        if (mw.breathing_rate > 0.0f && s_breathing_bpm > 0.0f) {
            float fused_br = mw.breathing_rate * 0.8f + s_breathing_bpm * 0.2f;
            fpkt.breathing_rate = (uint16_t)(fused_br * 100.0f);
        } else if (mw.breathing_rate > 0.0f) {
            fpkt.breathing_rate = (uint16_t)(mw.breathing_rate * 100.0f);
        } else {
            fpkt.breathing_rate = pkt.breathing_rate;
        }

        /* Raw mmWave values for server-side analysis. */
        fpkt.mmwave_hr_bpm = mw.heart_rate_bpm;
        fpkt.mmwave_br_bpm = mw.breathing_rate;
        fpkt.mmwave_distance = mw.distance_cm;
        fpkt.mmwave_targets = mw.target_count;
        fpkt.mmwave_confidence = (mw.frame_count > 10) ? 80 : 40;

        stream_sender_send((const uint8_t *)&fpkt, sizeof(fpkt));
    } else {
        /* No mmWave — send standard 32-byte packet. */
        stream_sender_send((const uint8_t *)&pkt, sizeof(pkt));
    }
}

/* ======================================================================
 * ADR-069: Feature Vector Packet (48 bytes, sent at 1 Hz alongside vitals)
 * ====================================================================== */

static void send_feature_vector(void)
{
    edge_feature_pkt_t pkt;
    memset(&pkt, 0, sizeof(pkt));

    pkt.magic = EDGE_FEATURE_MAGIC;
    pkt.node_id = csi_collector_get_node_id();  /* #390: defensive copy */
    pkt.reserved = 0;
    pkt.seq = s_feature_seq++;
    pkt.timestamp_us = esp_timer_get_time();

    /* Dim 0: Presence score (0.0-1.0, normalized from raw score) */
    float p = s_presence_score;
    pkt.features[0] = p > 10.0f ? 1.0f : (p < 0.0f ? 0.0f : p / 10.0f);

    /* Dim 1: Motion energy (normalized, 0-1 range) */
    float m = s_motion_energy;
    pkt.features[1] = m > 10.0f ? 1.0f : (m < 0.0f ? 0.0f : m / 10.0f);

    /* Dim 2: Breathing rate (BPM / 30, 0-1 range) */
    pkt.features[2] = s_breathing_bpm > 0.0f
        ? (s_breathing_bpm / 30.0f > 1.0f ? 1.0f : s_breathing_bpm / 30.0f)
        : 0.0f;

    /* Dim 3: Heart rate (BPM / 120, 0-1 range) */
    pkt.features[3] = s_heartrate_bpm > 0.0f
        ? (s_heartrate_bpm / 120.0f > 1.0f ? 1.0f : s_heartrate_bpm / 120.0f)
        : 0.0f;

    /* Dim 4: Phase variance mean (top-K subcarriers) */
    float var_mean = 0.0f;
    if (s_top_k_count > 0) {
        float var_sum = 0.0f;
        uint8_t k = s_top_k_count < EDGE_TOP_K ? s_top_k_count : EDGE_TOP_K;
        for (uint8_t i = 0; i < k; i++) {
            var_sum += (float)welford_variance(&s_subcarrier_var[s_top_k[i]]);
        }
        var_mean = var_sum / (float)k;
    }
    pkt.features[4] = var_mean > 1.0f ? 1.0f : (var_mean < 0.0f ? 0.0f : var_mean);

    /* Dim 5: Person count (n_persons / 4, 0-1 range) */
    uint8_t n_active = 0;
    for (uint8_t i = 0; i < EDGE_MAX_PERSONS; i++) {
        if (s_persons[i].active) n_active++;
    }
    pkt.features[5] = (float)n_active / 4.0f;
    if (pkt.features[5] > 1.0f) pkt.features[5] = 1.0f;

    /* Dim 6: Fall risk (0.0 or 1.0 based on recent detection) */
    pkt.features[6] = s_fall_detected ? 1.0f : 0.0f;

    /* Dim 7: RSSI normalized ((rssi + 100) / 100, 0-1 range) */
    pkt.features[7] = ((float)s_latest_rssi + 100.0f) / 100.0f;
    if (pkt.features[7] > 1.0f) pkt.features[7] = 1.0f;
    if (pkt.features[7] < 0.0f) pkt.features[7] = 0.0f;

    stream_sender_send((const uint8_t *)&pkt, sizeof(pkt));
}

/* ======================================================================
 * Main DSP Pipeline (runs on Core 1)
 * ====================================================================== */

static void process_frame(const edge_ring_slot_t *slot)
{
    uint16_t n_subcarriers = slot->iq_len / 2;
    if (n_subcarriers == 0 || n_subcarriers > EDGE_MAX_SUBCARRIERS) return;

    s_frame_count++;
    s_latest_rssi = slot->rssi;
    int64_t frame_now_us = esp_timer_get_time();

    /* CSI sample rate. MGMT-only promiscuous filter (RuView#396, csi_collector.c)
     * yields ~10 Hz from beacons; keep this value aligned with csi_collector's
     * effective callback rate or estimate_bpm_zero_crossing() reports the wrong
     * BPM (2× rate mismatch → 2× wrong breathing/HR). */
    const float sample_rate = 10.0f;

    /* --- Step 1-2: Phase extraction + unwrapping per subcarrier --- */
    float phases[EDGE_MAX_SUBCARRIERS];
    for (uint16_t sc = 0; sc < n_subcarriers; sc++) {
        float raw_phase = extract_phase(slot->iq_data, sc);

        if (s_phase_initialized) {
            phases[sc] = unwrap_phase(s_prev_phase[sc], raw_phase);
        } else {
            phases[sc] = raw_phase;
        }
        s_prev_phase[sc] = phases[sc];
    }
    s_phase_initialized = true;

    /* --- Step 3: Welford variance update per subcarrier --- */
    for (uint16_t sc = 0; sc < n_subcarriers; sc++) {
        welford_update(&s_subcarrier_var[sc], (double)phases[sc]);
    }

    /* --- Step 4: Top-K selection (every 100 frames to amortize cost) --- */
    if ((s_frame_count % 100) == 1 || s_top_k_count == 0) {
        update_top_k(n_subcarriers);
    }

    if (s_top_k_count == 0) return;

    /* --- Step 5: Phase of primary (highest-variance) subcarrier --- */
    float primary_phase = phases[s_top_k[0]];

    /* Store in phase history ring buffer. */
    s_phase_history[s_history_idx] = primary_phase;
    s_history_idx = (s_history_idx + 1) % EDGE_PHASE_HISTORY_LEN;
    if (s_history_len < EDGE_PHASE_HISTORY_LEN) s_history_len++;

    /* --- Step 6: Biquad bandpass filtering --- */
    float br_val = biquad_process(&s_bq_breathing, primary_phase);
    float hr_val = biquad_process(&s_bq_heartrate, primary_phase);

    uint16_t filt_idx = (s_history_idx + EDGE_PHASE_HISTORY_LEN - 1)
                        % EDGE_PHASE_HISTORY_LEN;
    s_breathing_filtered[filt_idx] = br_val;
    s_heartrate_filtered[filt_idx] = hr_val;

    /* --- Step 7: BPM estimation (zero-crossing) --- */
    if (s_history_len >= 64) {
        /* Build contiguous buffers from ring (using static scratch to save stack). */
        uint16_t buf_len = s_history_len;

        for (uint16_t i = 0; i < buf_len; i++) {
            uint16_t ri = (s_history_idx + EDGE_PHASE_HISTORY_LEN
                           - buf_len + i) % EDGE_PHASE_HISTORY_LEN;
            s_scratch_br[i] = s_breathing_filtered[ri];
            s_scratch_hr[i] = s_heartrate_filtered[ri];
        }

        float br_bpm = estimate_bpm_zero_crossing(s_scratch_br, buf_len, sample_rate);
        float hr_bpm = estimate_bpm_zero_crossing(s_scratch_hr, buf_len, sample_rate);

        /* Sanity clamp: breathing 6-40 BPM, heart rate 40-180 BPM. */
        if (br_bpm >= 6.0f && br_bpm <= 40.0f) s_breathing_bpm = br_bpm;
        if (hr_bpm >= 40.0f && hr_bpm <= 180.0f) s_heartrate_bpm = hr_bpm;
    }

    /* --- Step 8: Motion energy (variance of recent phases) --- */
    if (s_history_len >= 10) {
        float sum = 0.0f, sum2 = 0.0f;
        uint16_t window = (s_history_len < 20) ? s_history_len : 20;
        for (uint16_t i = 0; i < window; i++) {
            uint16_t ri = (s_history_idx + EDGE_PHASE_HISTORY_LEN
                           - window + i) % EDGE_PHASE_HISTORY_LEN;
            float v = s_phase_history[ri];
            sum += v;
            sum2 += v * v;
        }
        float mean = sum / (float)window;
        s_motion_energy = (sum2 / (float)window) - (mean * mean);
        if (s_motion_energy < 0.0f) s_motion_energy = 0.0f;
    }

    /* --- Step 9: Presence detection --- */
    s_presence_score = s_motion_energy;

    edge_check_nvs_recalibration(frame_now_us);
    recalibration_update(s_motion_energy, frame_now_us);
    rolling_baseline_update(s_motion_energy, frame_now_us);

    float threshold = s_cfg.presence_thresh;
    if (threshold == 0.0f && s_calibrated) {
        threshold = s_adaptive_threshold;
    } else if (threshold == 0.0f) {
        threshold = 0.05f;  /* Default until calibrated. */
    }

    log_threshold_if_due(threshold, frame_now_us);

    bool presence_candidate = (s_presence_score > threshold);
    s_presence_detected = presence_candidate;
    if (presence_candidate) {
        edge_human_freq_result_t human =
            detect_human_frequency_component(sample_rate);
        if (!human.has_component) {
            s_presence_detected = false;
            if (s_last_no_human_log_us == 0
                || (frame_now_us - s_last_no_human_log_us)
                    >= EDGE_DIAG_LOG_INTERVAL_US)
            {
                s_last_no_human_log_us = frame_now_us;
                ESP_LOGI(TAG, "Presence rejected: no human frequency component "
                         "(motion=%.4f threshold=%.4f band_ratio=%.3f "
                         "peak=%.2fHz peak_ratio=%.2f)",
                         s_presence_score, threshold, human.band_ratio,
                         human.peak_hz, human.peak_ratio);
            }
        }
    }

    /* --- Step 10: Fall detection (phase acceleration + debounce, issue #263) --- */
    if (s_history_len >= 3) {
        uint16_t i0 = (s_history_idx + EDGE_PHASE_HISTORY_LEN - 1) % EDGE_PHASE_HISTORY_LEN;
        uint16_t i1 = (s_history_idx + EDGE_PHASE_HISTORY_LEN - 2) % EDGE_PHASE_HISTORY_LEN;
        float velocity = s_phase_history[i0] - s_phase_history[i1];
        float accel = fabsf(velocity - s_prev_phase_velocity);
        s_prev_phase_velocity = velocity;

        if (accel > s_cfg.fall_thresh) {
            s_fall_consec_count++;
        } else {
            s_fall_consec_count = 0;
        }

        /* Require EDGE_FALL_CONSEC_MIN consecutive frames above threshold,
         * plus a cooldown period to prevent alert storms. */
        int64_t now_us = esp_timer_get_time();
        int64_t cooldown_us = (int64_t)EDGE_FALL_COOLDOWN_MS * 1000;
        if (s_fall_consec_count >= EDGE_FALL_CONSEC_MIN
            && (now_us - s_fall_last_alert_us) >= cooldown_us)
        {
            s_fall_detected = true;
            s_fall_last_alert_us = now_us;
            s_fall_consec_count = 0;
            ESP_LOGW(TAG, "Fall detected! accel=%.4f > thresh=%.4f (consec=%u)",
                     accel, s_cfg.fall_thresh, EDGE_FALL_CONSEC_MIN);
        } else if (s_fall_consec_count == 0) {
            s_fall_detected = false;
        }
    }

    /* --- Step 11: Multi-person vitals --- */
    update_multi_person_vitals(slot->iq_data, n_subcarriers, sample_rate);
    /* Yield after multi-person DSP so IDLE1 can feed Core 1 watchdog (#683). */
    if (s_cfg.tier >= 2) vTaskDelay(1);

    /* --- Step 12: Delta compression --- */
    if (s_cfg.tier >= 2) {
        send_compressed_frame(slot->iq_data, slot->iq_len, slot->channel);
    }

    /* --- Step 13: Send vitals packet at configured interval --- */
    int64_t now_us = frame_now_us;
    int64_t interval_us = (int64_t)s_cfg.vital_interval_ms * 1000;
    if ((now_us - s_last_vitals_send_us) >= interval_us) {
        send_vitals_packet();
        send_feature_vector();  /* ADR-069: 48-byte feature vector at same 1 Hz cadence. */
        s_last_vitals_send_us = now_us;

        if ((s_frame_count % 200) == 0) {
            ESP_LOGI(TAG, "Vitals: br=%.1f hr=%.1f motion=%.4f thresh=%.4f pres=%s "
                     "fall=%s persons=%u frames=%lu drops=%lu",
                     s_breathing_bpm, s_heartrate_bpm, s_motion_energy,
                     threshold,
                     s_presence_detected ? "YES" : "no",
                     s_fall_detected ? "YES" : "no",
                     (unsigned)s_latest_pkt.n_persons,
                     (unsigned long)s_frame_count,
                     (unsigned long)s_ring_drops);
        }
    }

    /* --- Step 14 (ADR-040): Dispatch to WASM modules --- */
    if (s_cfg.tier >= 2 && s_pkt_valid) {
        /* Extract amplitudes from I/Q for WASM host API. */
        float amplitudes[EDGE_MAX_SUBCARRIERS];
        for (uint16_t sc = 0; sc < n_subcarriers; sc++) {
            int8_t i_val = (int8_t)slot->iq_data[sc * 2];
            int8_t q_val = (int8_t)slot->iq_data[sc * 2 + 1];
            amplitudes[sc] = sqrtf((float)(i_val * i_val + q_val * q_val));
        }

        /* Build variance array from Welford state. */
        float variances[EDGE_MAX_SUBCARRIERS];
        for (uint16_t sc = 0; sc < n_subcarriers; sc++) {
            variances[sc] = (float)welford_variance(&s_subcarrier_var[sc]);
        }

        wasm_runtime_on_frame(phases, amplitudes, variances,
                              n_subcarriers,
                              (const edge_vitals_pkt_t *)&s_latest_pkt);
        /* Yield after WASM dispatch to feed Core 1 watchdog (#683). */
        vTaskDelay(1);
    }
}

/* ======================================================================
 * Edge Processing Task (pinned to Core 1)
 * ====================================================================== */

static void edge_task(void *arg)
{
    (void)arg;
    ESP_LOGI(TAG, "Edge DSP task started on core %d (tier=%u)",
             xPortGetCoreID(), s_cfg.tier);

    edge_ring_slot_t slot;

    /* Maximum frames to process before a longer yield.  On busy LANs
     * (corporate networks, many APs), the ring buffer fills continuously.
     * Without a batch limit the task processes frames back-to-back with
     * only 1-tick yields, which on high frame rates can still starve
     * IDLE1 enough to trip the 5-second task watchdog.  See #266, #321. */

    while (1) {
        uint8_t processed = 0;

        while (processed < EDGE_BATCH_LIMIT && ring_pop(&slot)) {
            process_frame(&slot);
            processed++;
            /* 1-tick yield between frames within a batch. */
            vTaskDelay(1);
        }

        if (processed > 0) {
            /* Post-batch yield: ~20 ms so IDLE1 can run and feed the
             * Core 1 watchdog even under sustained load.  Uses pdMS_TO_TICKS
             * for tick-rate independence (minimum 1 tick). */
            { TickType_t d = pdMS_TO_TICKS(20); vTaskDelay(d > 0 ? d : 1); }
        } else {
            /* No frames available — sleep one full tick.
             * NOTE: pdMS_TO_TICKS(5) == 0 at 100 Hz, which would busy-spin. */
            edge_check_nvs_recalibration(esp_timer_get_time());
            vTaskDelay(1);
        }
    }
}

/* ======================================================================
 * Public API
 * ====================================================================== */

bool edge_enqueue_csi(const uint8_t *iq_data, uint16_t iq_len,
                      int8_t rssi, uint8_t channel)
{
    return ring_push(iq_data, iq_len, rssi, channel);
}

bool edge_get_vitals(edge_vitals_pkt_t *pkt)
{
    if (!s_pkt_valid || pkt == NULL) return false;
    memcpy(pkt, (const void *)&s_latest_pkt, sizeof(edge_vitals_pkt_t));
    return true;
}

void edge_get_multi_person(edge_person_vitals_t *persons, uint8_t *n_active)
{
    uint8_t active = 0;
    for (uint8_t p = 0; p < EDGE_MAX_PERSONS; p++) {
        if (persons) persons[p] = s_persons[p];
        if (s_persons[p].active) active++;
    }
    if (n_active) *n_active = active;
}

void edge_get_phase_history(const float **out_buf, uint16_t *out_len,
                            uint16_t *out_idx)
{
    if (out_buf) *out_buf = s_phase_history;
    if (out_len) *out_len = s_history_len;
    if (out_idx) *out_idx = s_history_idx;
}

void edge_get_variances(float *out_variances, uint16_t n_subcarriers)
{
    if (out_variances == NULL) return;
    uint16_t n = (n_subcarriers > EDGE_MAX_SUBCARRIERS) ? EDGE_MAX_SUBCARRIERS : n_subcarriers;
    for (uint16_t i = 0; i < n; i++) {
        out_variances[i] = (float)welford_variance(&s_subcarrier_var[i]);
    }
}

void edge_request_recalibration(void)
{
    s_recalib_requested = true;
}

esp_err_t edge_processing_init(const edge_config_t *cfg)
{
    if (cfg == NULL) {
        ESP_LOGE(TAG, "edge_processing_init: cfg is NULL");
        return ESP_ERR_INVALID_ARG;
    }

    /* Store config. */
    s_cfg = *cfg;

    ESP_LOGI(TAG, "Initializing edge processing (tier=%u, top_k=%u, "
             "vital_interval=%ums, presence_thresh=%.3f)",
             s_cfg.tier, s_cfg.top_k_count,
             s_cfg.vital_interval_ms, s_cfg.presence_thresh);

    /* Reset all state. */
    memset(&s_ring, 0, sizeof(s_ring));
    memset(s_subcarrier_var, 0, sizeof(s_subcarrier_var));
    memset(s_prev_phase, 0, sizeof(s_prev_phase));
    s_phase_initialized = false;
    s_top_k_count = 0;
    s_history_len = 0;
    s_history_idx = 0;
    s_breathing_bpm = 0.0f;
    s_heartrate_bpm = 0.0f;
    s_motion_energy = 0.0f;
    s_presence_score = 0.0f;
    s_presence_detected = false;
    s_fall_detected = false;
    s_latest_rssi = 0;
    s_frame_count = 0;
    s_prev_phase_velocity = 0.0f;
    s_fall_consec_count = 0;
    s_fall_last_alert_us = 0;
    s_last_vitals_send_us = 0;
    s_has_prev_iq = false;
    s_prev_iq_len = 0;
    s_pkt_valid = false;

    /* Reset calibration state. */
    s_calibrated = false;
    memset(s_baseline_samples, 0, sizeof(s_baseline_samples));
    s_baseline_count = 0;
    s_baseline_idx = 0;
    s_last_baseline_update_us = 0;
    s_baseline_median = 0.0f;
    s_baseline_mad_sigma = 0.0f;
    s_adaptive_threshold = 0.05f;
    s_recalib_requested = false;
    s_recalib_active = false;
    s_recalib_start_us = 0;
    memset(s_recalib_samples, 0, sizeof(s_recalib_samples));
    s_recalib_count = 0;
    s_last_recalib_nvs_check_us = 0;
    s_last_no_human_log_us = 0;
    s_last_threshold_log_us = 0;

    /* Reset multi-person state. */
    memset(s_persons, 0, sizeof(s_persons));
    for (uint8_t p = 0; p < EDGE_MAX_PERSONS; p++) {
        s_persons[p].active = false;
    }

    /* Design biquad bandpass filters.
     * Sampling rate ~20 Hz (typical ESP32 CSI callback rate). */
    const float fs = 20.0f;
    biquad_bandpass_design(&s_bq_breathing, fs, 0.1f, 0.5f);
    biquad_bandpass_design(&s_bq_heartrate, fs, 0.8f, 2.0f);

    /* Design per-person filters. */
    for (uint8_t p = 0; p < EDGE_MAX_PERSONS; p++) {
        biquad_bandpass_design(&s_person_bq_br[p], fs, 0.1f, 0.5f);
        biquad_bandpass_design(&s_person_bq_hr[p], fs, 0.8f, 2.0f);
    }

    if (s_cfg.tier == 0) {
        ESP_LOGI(TAG, "Edge tier 0: raw passthrough (no DSP task)");
        return ESP_OK;
    }

    /* Pin DSP off WiFi's preferred core when SMP; else core 0 only (ESP32-C6). */
    const BaseType_t dsp_core = (portNUM_PROCESSORS > 1) ? (BaseType_t)1 : (BaseType_t)0;

    BaseType_t ret = xTaskCreatePinnedToCore(
        edge_task,
        "edge_dsp",
        8192,       /* 8 KB stack — sufficient for DSP pipeline. */
        NULL,
        5,          /* Priority 5 — above idle, below WiFi. */
        NULL,
        dsp_core);

    if (ret != pdPASS) {
        ESP_LOGE(TAG, "Failed to create edge DSP task");
        return ESP_ERR_NO_MEM;
    }

    ESP_LOGI(TAG, "Edge DSP task created on core %d (stack=8192, priority=5)",
             (int)dsp_core);
    return ESP_OK;
}
