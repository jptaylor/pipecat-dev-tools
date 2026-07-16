// C ABI for the macOS system-audio tap (Core Audio process taps, macOS 14.4+)
// and Core Audio device latency queries. Consumed from Rust via FFI.
#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// Called on a Core Audio dispatch queue. Must be realtime-safe.
// `host_time` is the mach host time of the first frame in the buffer.
typedef void (*sysaudio_data_cb)(void *ctx, const float *interleaved,
                                 uint32_t frames, uint32_t channels,
                                 double sample_rate, uint64_t host_time);

// event_code: 1 = default output device changed, tap restarted (timeline discontinuity)
//             2 = tap failed and could not be restarted
typedef void (*sysaudio_event_cb)(void *ctx, int32_t event_code);

// Returns 0 on success. On failure writes a message into `err`.
int32_t sysaudio_start(sysaudio_data_cb data_cb, sysaudio_event_cb event_cb,
                       void *ctx, char *err, int32_t err_len);
void sysaudio_stop(void);

// 1 if the OS supports system-audio taps (macOS 14.4+), else 0.
int32_t sysaudio_supported(void);

typedef struct {
    char name[256];
    double device_latency_ms;   // kAudioDevicePropertyLatency
    double safety_offset_ms;    // kAudioDevicePropertySafetyOffset
    double buffer_ms;           // kAudioDevicePropertyBufferFrameSize
    double stream_latency_ms;   // kAudioStreamPropertyLatency (first stream)
    double total_latency_ms;    // sum of the above
    double sample_rate;
    uint32_t transport;         // fourcc from kAudioDevicePropertyTransportType
    int32_t is_bluetooth;
} sysaudio_device_info_t;

// Both return 0 on success, nonzero if the device was not found.
int32_t sysaudio_default_output_info(sysaudio_device_info_t *out);
int32_t sysaudio_input_info_by_name(const char *name_utf8, sysaudio_device_info_t *out);

#ifdef __cplusplus
}
#endif
