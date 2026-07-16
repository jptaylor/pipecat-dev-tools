// System-audio capture via Core Audio process taps (macOS 14.4+).
//
// Flow: CATapDescription (global tap = a stereo mixdown of ALL processes'
// audio, regardless of which output device each process plays to) ->
// AudioHardwareCreateProcessTap -> private aggregate device containing the
// tap -> IOProc on the aggregate reads the tapped audio with hardware host
// timestamps.
//
// Aggregate composition: we first try a tap-only aggregate. Including the
// default output device as a sub-device (the AudioCap-style recipe) pulls
// that device's INPUT streams into the IOProc buffer list when the output
// device is an external audio interface with inputs — ahead of the tap
// stream. If the tap-only aggregate cannot start, we fall back to including
// the output device and rely on the tap stream being last in the list.
//
// When the default output device changes (e.g. AirPods connect) the whole
// stack is torn down and rebuilt, and the caller is notified so it can mark
// a timeline discontinuity.

#import <Foundation/Foundation.h>
#import <CoreAudio/CoreAudio.h>

#if __has_include(<CoreAudio/CATapDescription.h>) && __has_include(<CoreAudio/AudioHardwareTapping.h>)
#import <CoreAudio/CATapDescription.h>
#import <CoreAudio/AudioHardwareTapping.h>
#define SYSAUDIO_HAVE_TAP 1
#endif

#include "system_tap.h"
#include <mach/mach_time.h>

static sysaudio_data_cb g_data_cb = NULL;
static sysaudio_event_cb g_event_cb = NULL;
static void *g_ctx = NULL;

static AudioObjectID g_tap = kAudioObjectUnknown;
static AudioObjectID g_agg = kAudioObjectUnknown;
static AudioDeviceIOProcID g_ioproc = NULL;
static double g_rate = 0;
static uint32_t g_channels = 0;
static BOOL g_running = NO;
static dispatch_queue_t g_io_queue = NULL;   // IOProc dispatch queue
static dispatch_queue_t g_ctrl_queue = NULL; // serializes start/stop/restart

static const AudioObjectPropertyAddress kDefaultOutAddr = {
    kAudioHardwarePropertyDefaultOutputDevice,
    kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyElementMain,
};

static void set_err(char *err, int32_t len, NSString *msg) {
    if (err && len > 0) {
        snprintf(err, (size_t)len, "%s", msg.UTF8String);
    }
}

int32_t sysaudio_supported(void) {
#ifdef SYSAUDIO_HAVE_TAP
    if (@available(macOS 14.4, *)) {
        return 1;
    }
#endif
    return 0;
}

static AudioObjectID default_output_device(void) {
    AudioObjectID dev = kAudioObjectUnknown;
    UInt32 sz = sizeof(dev);
    AudioObjectGetPropertyData(kAudioObjectSystemObject, &kDefaultOutAddr, 0, NULL, &sz, &dev);
    return dev;
}

static NSString *device_uid(AudioObjectID dev) {
    CFStringRef uid = NULL;
    UInt32 sz = sizeof(uid);
    AudioObjectPropertyAddress a = {
        kAudioDevicePropertyDeviceUID,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    };
    if (AudioObjectGetPropertyData(dev, &a, 0, NULL, &sz, &uid) != noErr || uid == NULL) {
        return nil;
    }
    return (__bridge_transfer NSString *)uid;
}

#ifdef SYSAUDIO_HAVE_TAP

API_AVAILABLE(macos(14.4))
static void teardown_io_and_aggregate(void) {
    if (g_agg != kAudioObjectUnknown) {
        if (g_ioproc != NULL) {
            AudioDeviceStop(g_agg, g_ioproc);
            AudioDeviceDestroyIOProcID(g_agg, g_ioproc);
            g_ioproc = NULL;
        }
        AudioHardwareDestroyAggregateDevice(g_agg);
        g_agg = kAudioObjectUnknown;
    }
}

API_AVAILABLE(macos(14.4))
static void teardown_locked(void) {
    teardown_io_and_aggregate();
    if (g_tap != kAudioObjectUnknown) {
        AudioHardwareDestroyProcessTap(g_tap);
        g_tap = kAudioObjectUnknown;
    }
}

API_AVAILABLE(macos(14.4))
static int32_t create_aggregate(NSString *tap_uid, BOOL include_output_device, char *err,
                                int32_t err_len) {
    NSMutableDictionary *agg = [@{
        @kAudioAggregateDeviceNameKey : @"Pipecat Audio Metrics Tap",
        @kAudioAggregateDeviceUIDKey :
            [NSString stringWithFormat:@"co.daily.pipecat-audio-metrics.agg.%@", tap_uid],
        @kAudioAggregateDeviceIsPrivateKey : @YES,
        @kAudioAggregateDeviceIsStackedKey : @NO,
        @kAudioAggregateDeviceTapAutoStartKey : @YES,
        @kAudioAggregateDeviceTapListKey : @[ @{
            @kAudioSubTapUIDKey : tap_uid,
            @kAudioSubTapDriftCompensationKey : @YES,
        } ],
    } mutableCopy];

    if (include_output_device) {
        AudioObjectID out_dev = default_output_device();
        NSString *out_uid = (out_dev != kAudioObjectUnknown) ? device_uid(out_dev) : nil;
        if (out_uid != nil) {
            agg[@kAudioAggregateDeviceMainSubDeviceKey] = out_uid;
            agg[@kAudioAggregateDeviceSubDeviceListKey] = @[ @{@kAudioSubDeviceUIDKey : out_uid} ];
        }
    }

    OSStatus st = AudioHardwareCreateAggregateDevice((__bridge CFDictionaryRef)agg, &g_agg);
    if (st != noErr || g_agg == kAudioObjectUnknown) {
        set_err(err, err_len,
                [NSString stringWithFormat:@"AudioHardwareCreateAggregateDevice failed (%d)", (int)st]);
        g_agg = kAudioObjectUnknown;
        return -5;
    }
    return 0;
}

API_AVAILABLE(macos(14.4))
static int32_t start_io(char *err, int32_t err_len) {
    OSStatus st = AudioDeviceCreateIOProcIDWithBlock(
        &g_ioproc, g_agg, g_io_queue,
        ^(const AudioTimeStamp *inNow, const AudioBufferList *inInputData,
          const AudioTimeStamp *inInputTime, AudioBufferList *outOutputData,
          const AudioTimeStamp *inOutputTime) {
          (void)inNow; (void)outOutputData; (void)inOutputTime;
          sysaudio_data_cb cb = g_data_cb;
          if (cb == NULL || inInputData == NULL || inInputData->mNumberBuffers == 0) {
              return;
          }
          // Sub-device input streams (e.g. an external audio interface's mic
          // inputs) precede tap streams in the aggregate's buffer list, so
          // the tap is the LAST buffer. With a tap-only aggregate there is
          // exactly one buffer and this still picks it.
          const AudioBuffer *buf = &inInputData->mBuffers[inInputData->mNumberBuffers - 1];
          if (buf->mData == NULL || buf->mDataByteSize == 0) {
              return;
          }
          uint32_t chans = buf->mNumberChannels > 0 ? buf->mNumberChannels : g_channels;
          if (chans == 0) chans = 2;
          uint32_t frames = (uint32_t)(buf->mDataByteSize / (sizeof(float) * chans));
          uint64_t host_time = (inInputTime != NULL &&
                                (inInputTime->mFlags & kAudioTimeStampHostTimeValid))
                                   ? inInputTime->mHostTime
                                   : mach_absolute_time();
          cb(g_ctx, (const float *)buf->mData, frames, chans, g_rate, host_time);
        });
    if (st != noErr) {
        set_err(err, err_len,
                [NSString stringWithFormat:@"AudioDeviceCreateIOProcIDWithBlock failed (%d)", (int)st]);
        return -6;
    }

    st = AudioDeviceStart(g_agg, g_ioproc);
    if (st != noErr) {
        set_err(err, err_len, [NSString stringWithFormat:@"AudioDeviceStart failed (%d)", (int)st]);
        return -7;
    }
    return 0;
}

API_AVAILABLE(macos(14.4))
static int32_t start_locked(char *err, int32_t err_len) {
    CATapDescription *desc = [[CATapDescription alloc] initStereoGlobalTapButExcludeProcesses:@[]];
    desc.name = @"Pipecat Audio Metrics system tap";
    desc.privateTap = YES;
    desc.muteBehavior = CATapUnmuted;

    OSStatus st = AudioHardwareCreateProcessTap(desc, &g_tap);
    if (st != noErr || g_tap == kAudioObjectUnknown) {
        set_err(err, err_len,
                [NSString stringWithFormat:@"AudioHardwareCreateProcessTap failed (%d). "
                                           @"Check System Settings > Privacy & Security > "
                                           @"Screen & System Audio Recording.", (int)st]);
        g_tap = kAudioObjectUnknown;
        return -3;
    }

    AudioStreamBasicDescription asbd = {0};
    UInt32 sz = sizeof(asbd);
    AudioObjectPropertyAddress fmt_addr = {
        kAudioTapPropertyFormat,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    };
    st = AudioObjectGetPropertyData(g_tap, &fmt_addr, 0, NULL, &sz, &asbd);
    if (st != noErr) {
        set_err(err, err_len, [NSString stringWithFormat:@"kAudioTapPropertyFormat failed (%d)", (int)st]);
        teardown_locked();
        return -4;
    }
    g_rate = asbd.mSampleRate;
    g_channels = asbd.mChannelsPerFrame > 0 ? asbd.mChannelsPerFrame : 2;

    NSString *tap_uid = desc.UUID.UUIDString;

    // Preferred: tap-only aggregate (only stream in the IOProc is the tap).
    int32_t rc = create_aggregate(tap_uid, NO, err, err_len);
    if (rc == 0) {
        rc = start_io(err, err_len);
        if (rc != 0) {
            teardown_io_and_aggregate();
        }
    }
    // Fallback: include the default output device as sub-device (clock
    // source); the IOProc picks the last buffer so the tap is still read.
    if (rc != 0) {
        rc = create_aggregate(tap_uid, YES, err, err_len);
        if (rc == 0) {
            rc = start_io(err, err_len);
        }
    }
    if (rc != 0) {
        teardown_locked();
        return rc;
    }
    return 0;
}

static OSStatus default_out_changed(AudioObjectID inObjectID, UInt32 inNumberAddresses,
                                    const AudioObjectPropertyAddress *inAddresses,
                                    void *inClientData) {
    (void)inObjectID; (void)inNumberAddresses; (void)inAddresses; (void)inClientData;
    if (g_ctrl_queue == NULL) return noErr;
    dispatch_async(g_ctrl_queue, ^{
      if (!g_running) return;
      if (@available(macOS 14.4, *)) {
          teardown_locked();
          char err[256] = {0};
          if (start_locked(err, sizeof(err)) == 0) {
              if (g_event_cb) g_event_cb(g_ctx, 1);
          } else {
              g_running = NO;
              if (g_event_cb) g_event_cb(g_ctx, 2);
          }
      }
    });
    return noErr;
}

#endif // SYSAUDIO_HAVE_TAP

int32_t sysaudio_start(sysaudio_data_cb data_cb, sysaudio_event_cb event_cb, void *ctx,
                       char *err, int32_t err_len) {
#ifdef SYSAUDIO_HAVE_TAP
    if (@available(macOS 14.4, *)) {
        @autoreleasepool {
            if (g_running) {
                set_err(err, err_len, @"system tap already running");
                return -1;
            }
            g_data_cb = data_cb;
            g_event_cb = event_cb;
            g_ctx = ctx;
            if (g_io_queue == NULL) {
                g_io_queue = dispatch_queue_create("co.daily.pipecat-audio-metrics.tap.io",
                                                   DISPATCH_QUEUE_SERIAL);
            }
            if (g_ctrl_queue == NULL) {
                g_ctrl_queue = dispatch_queue_create("co.daily.pipecat-audio-metrics.tap.ctrl",
                                                     DISPATCH_QUEUE_SERIAL);
            }
            __block int32_t rc = 0;
            dispatch_sync(g_ctrl_queue, ^{
              rc = start_locked(err, err_len);
              if (rc == 0) g_running = YES;
            });
            if (rc == 0) {
                AudioObjectAddPropertyListener(kAudioObjectSystemObject, &kDefaultOutAddr,
                                               default_out_changed, NULL);
            }
            return rc;
        }
    }
#endif
    (void)data_cb; (void)event_cb; (void)ctx;
    set_err(err, err_len, @"System-audio tap requires macOS 14.4 or later");
    return -2;
}

void sysaudio_stop(void) {
#ifdef SYSAUDIO_HAVE_TAP
    if (@available(macOS 14.4, *)) {
        if (g_ctrl_queue == NULL) return;
        AudioObjectRemovePropertyListener(kAudioObjectSystemObject, &kDefaultOutAddr,
                                          default_out_changed, NULL);
        dispatch_sync(g_ctrl_queue, ^{
          g_running = NO;
          g_data_cb = NULL;
          teardown_locked();
        });
    }
#endif
}

// ---------------------------------------------------------------------------
// Device info queries
// ---------------------------------------------------------------------------

static UInt32 get_u32_prop(AudioObjectID dev, AudioObjectPropertySelector sel,
                           AudioObjectPropertyScope scope) {
    UInt32 v = 0;
    UInt32 sz = sizeof(v);
    AudioObjectPropertyAddress a = {sel, scope, kAudioObjectPropertyElementMain};
    if (AudioObjectGetPropertyData(dev, &a, 0, NULL, &sz, &v) != noErr) {
        return 0;
    }
    return v;
}

static int fill_device_info(AudioObjectID dev, AudioObjectPropertyScope scope,
                            sysaudio_device_info_t *out) {
    memset(out, 0, sizeof(*out));

    CFStringRef name = NULL;
    UInt32 sz = sizeof(name);
    AudioObjectPropertyAddress name_addr = {
        kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    if (AudioObjectGetPropertyData(dev, &name_addr, 0, NULL, &sz, &name) == noErr && name != NULL) {
        NSString *ns = (__bridge_transfer NSString *)name;
        snprintf(out->name, sizeof(out->name), "%s", ns.UTF8String ?: "");
    }

    Float64 rate = 0;
    sz = sizeof(rate);
    AudioObjectPropertyAddress rate_addr = {
        kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain};
    AudioObjectGetPropertyData(dev, &rate_addr, 0, NULL, &sz, &rate);
    if (rate <= 0) rate = 48000.0;
    out->sample_rate = rate;

    UInt32 dev_latency = get_u32_prop(dev, kAudioDevicePropertyLatency, scope);
    UInt32 safety = get_u32_prop(dev, kAudioDevicePropertySafetyOffset, scope);
    UInt32 buffer = get_u32_prop(dev, kAudioDevicePropertyBufferFrameSize,
                                 kAudioObjectPropertyScopeGlobal);

    // Stream latency from the first stream in this scope.
    UInt32 stream_latency = 0;
    AudioObjectPropertyAddress streams_addr = {kAudioDevicePropertyStreams, scope,
                                               kAudioObjectPropertyElementMain};
    UInt32 streams_sz = 0;
    if (AudioObjectGetPropertyDataSize(dev, &streams_addr, 0, NULL, &streams_sz) == noErr &&
        streams_sz >= sizeof(AudioStreamID)) {
        UInt32 count = streams_sz / sizeof(AudioStreamID);
        AudioStreamID *ids = calloc(count, sizeof(AudioStreamID));
        if (ids != NULL) {
            if (AudioObjectGetPropertyData(dev, &streams_addr, 0, NULL, &streams_sz, ids) == noErr &&
                count > 0) {
                stream_latency = get_u32_prop(ids[0], kAudioStreamPropertyLatency,
                                              kAudioObjectPropertyScopeGlobal);
            }
            free(ids);
        }
    }

    double to_ms = 1000.0 / rate;
    out->device_latency_ms = dev_latency * to_ms;
    out->safety_offset_ms = safety * to_ms;
    out->buffer_ms = buffer * to_ms;
    out->stream_latency_ms = stream_latency * to_ms;
    out->total_latency_ms =
        out->device_latency_ms + out->safety_offset_ms + out->buffer_ms + out->stream_latency_ms;

    UInt32 transport =
        get_u32_prop(dev, kAudioDevicePropertyTransportType, kAudioObjectPropertyScopeGlobal);
    out->transport = transport;
    out->is_bluetooth = (transport == kAudioDeviceTransportTypeBluetooth ||
                         transport == kAudioDeviceTransportTypeBluetoothLE)
                            ? 1
                            : 0;
    return 0;
}

int32_t sysaudio_default_output_info(sysaudio_device_info_t *out) {
    @autoreleasepool {
        AudioObjectID dev = default_output_device();
        if (dev == kAudioObjectUnknown) return -1;
        return fill_device_info(dev, kAudioObjectPropertyScopeOutput, out);
    }
}

int32_t sysaudio_input_info_by_name(const char *name_utf8, sysaudio_device_info_t *out) {
    @autoreleasepool {
        if (name_utf8 == NULL) return -1;
        NSString *want = [NSString stringWithUTF8String:name_utf8];
        if (want == nil) return -1;

        AudioObjectPropertyAddress devs_addr = {kAudioHardwarePropertyDevices,
                                                kAudioObjectPropertyScopeGlobal,
                                                kAudioObjectPropertyElementMain};
        UInt32 sz = 0;
        if (AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &devs_addr, 0, NULL, &sz) !=
            noErr) {
            return -2;
        }
        UInt32 count = sz / sizeof(AudioObjectID);
        if (count == 0) return -2;
        AudioObjectID *devs = calloc(count, sizeof(AudioObjectID));
        if (devs == NULL) return -2;
        if (AudioObjectGetPropertyData(kAudioObjectSystemObject, &devs_addr, 0, NULL, &sz, devs) !=
            noErr) {
            free(devs);
            return -2;
        }

        int32_t rc = -3;
        for (UInt32 i = 0; i < count; i++) {
            // Must have input streams.
            AudioObjectPropertyAddress in_streams = {kAudioDevicePropertyStreams,
                                                     kAudioObjectPropertyScopeInput,
                                                     kAudioObjectPropertyElementMain};
            UInt32 ssz = 0;
            if (AudioObjectGetPropertyDataSize(devs[i], &in_streams, 0, NULL, &ssz) != noErr ||
                ssz < sizeof(AudioStreamID)) {
                continue;
            }
            CFStringRef name = NULL;
            UInt32 nsz = sizeof(name);
            AudioObjectPropertyAddress name_addr = {kAudioObjectPropertyName,
                                                    kAudioObjectPropertyScopeGlobal,
                                                    kAudioObjectPropertyElementMain};
            if (AudioObjectGetPropertyData(devs[i], &name_addr, 0, NULL, &nsz, &name) != noErr ||
                name == NULL) {
                continue;
            }
            NSString *ns = (__bridge_transfer NSString *)name;
            if ([ns isEqualToString:want]) {
                rc = (int32_t)fill_device_info(devs[i], kAudioObjectPropertyScopeInput, out);
                break;
            }
        }
        free(devs);
        return rc;
    }
}
