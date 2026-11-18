#import "TGCallsIOSAudioDevice.h"
#import <AVFoundation/AVFoundation.h>
#import <AudioToolbox/AudioToolbox.h>
#include <string.h>
#import <math.h>
#import <stdatomic.h>

enum { QueueFrames = 8192, RenderBlockFrames = 480 };

typedef struct {
    _Atomic uint32_t read;
    _Atomic uint32_t write;
    int16_t samples[QueueFrames][2];
} PCMQueue;

@interface TGCallsAudioQueues : NSObject {
@public
    PCMQueue capture;
    PCMQueue playback;
    PCMQueue render;
}
@end

@implementation TGCallsAudioQueues
@end

static int16_t FloatToPCM(float value) {
    if (!isfinite(value)) {
        return 0;
    }
    return (int16_t)lrintf(fminf(1.0f, fmaxf(-1.0f, value)) * 32767.0f);
}

static BOOL ReadBlock(PCMQueue *queue, uint32_t frames, uint16_t channels, int16_t *output) {
    uint32_t read = atomic_load_explicit(&queue->read, memory_order_relaxed);
    uint32_t write = atomic_load_explicit(&queue->write, memory_order_acquire);
    if (write - read < frames) {
        return NO;
    }
    for (uint32_t i = 0; i < frames; i++) {
        const int16_t *frame = queue->samples[(read + i) & (QueueFrames - 1)];
        output[i * channels] = frame[0];
        if (channels == 2) {
            output[i * channels + 1] = frame[1];
        }
    }
    atomic_store_explicit(&queue->read, read + frames, memory_order_release);
    return YES;
}

static BOOL SupportsCaptureRate(uint32_t rate) {
    switch (rate) {
        case 8000: case 16000: case 24000: case 32000: case 44100: case 48000:
            return YES;
        default:
            return NO;
    }
}

@implementation TGCallsIOSAudioDevice {
    dispatch_queue_t _queue;
    dispatch_source_t _pump;
    TGCallsCaptureBlock _capture;
    TGCallsRenderBlock _render;
    TGCallsAudioErrorBlock _failure;
    NSMutableArray<id> *_observers;
    AVAudioEngine *_engine;
    TGCallsAudioQueues *_buffers;
    uint32_t _captureRate;
    uint16_t _captureChannels;
    BOOL _manageActivation;
    BOOL _requested;
    BOOL _callKitActive;
    BOOL _interrupted;
    BOOL _activated;
}

- (instancetype)initWithQueue:(dispatch_queue_t)queue
          manageSessionActivation:(BOOL)manageSessionActivation
                          capture:(TGCallsCaptureBlock)capture
                           render:(TGCallsRenderBlock)render
                          failure:(TGCallsAudioErrorBlock)failure {
    self = [super init];
    if (self) {
        _queue = queue;
        _manageActivation = manageSessionActivation;
        _capture = [capture copy];
        _render = [render copy];
        _failure = [failure copy];
        _observers = [NSMutableArray array];
        NSNotificationCenter *center = NSNotificationCenter.defaultCenter;
        __weak TGCallsIOSAudioDevice *weakSelf = self;
        for (NSString *name in @[AVAudioSessionRouteChangeNotification,
                                 AVAudioSessionInterruptionNotification,
                                 AVAudioSessionMediaServicesWereResetNotification]) {
            id observer = [center addObserverForName:name object:nil queue:nil usingBlock:^(NSNotification *event) {
                dispatch_async(queue, ^{ [weakSelf handleSessionEvent:event]; });
            }];
            [_observers addObject:observer];
        }
    }
    return self;
}

- (void)dealloc {
    for (id observer in _observers) {
        [NSNotificationCenter.defaultCenter removeObserver:observer];
    }
    [self stop];
}

- (BOOL)startAndReturnError:(NSError **)error {
    _requested = YES;
    if (!_interrupted && (_manageActivation || _callKitActive) && ![self startGraph:error]) {
        [self stop];
        return NO;
    }
    return YES;
}

- (void)stop {
    _requested = NO;
    [self stopGraph];
    if (_activated) {
        [AVAudioSession.sharedInstance setActive:NO
                                     withOptions:AVAudioSessionSetActiveOptionNotifyOthersOnDeactivation error:nil];
        _activated = NO;
    }
}

- (void)setCallKitActive:(BOOL)active {
    _callKitActive = active;
    if (_manageActivation) {
        return;
    }
    if (active) {
        [self resumeGraph];
    } else {
        [self stopGraph];
    }
}

- (BOOL)setSpeakerEnabled:(BOOL)enabled error:(NSError **)error {
    return [AVAudioSession.sharedInstance overrideOutputAudioPort:enabled ? AVAudioSessionPortOverrideSpeaker : AVAudioSessionPortOverrideNone
                                                           error:error];
}

- (BOOL)enqueuePlayback:(NSData *)pcm {
    if (!_buffers || pcm.length == 0 || pcm.length % 4 != 0 || pcm.length / 4 > QueueFrames) {
        return NO;
    }
    PCMQueue *queue = &_buffers->playback;
    uint32_t write = atomic_load_explicit(&queue->write, memory_order_relaxed);
    uint32_t read = atomic_load_explicit(&queue->read, memory_order_acquire);
    uint32_t frames = (uint32_t)(pcm.length / 4);
    if (frames > QueueFrames - (write - read)) {
        return NO;
    }
    const uint8_t *bytes = pcm.bytes;
    for (uint32_t i = 0; i < frames; i++) {
        memcpy(queue->samples[(write + i) & (QueueFrames - 1)], bytes + 4 * i, 4);
    }
    atomic_store_explicit(&queue->write, write + frames, memory_order_release);
    return YES;
}

- (void)handleSessionEvent:(NSNotification *)event {
    if (!_requested) {
        return;
    }
    if ([event.name isEqualToString:AVAudioSessionInterruptionNotification]) {
        _interrupted = [event.userInfo[AVAudioSessionInterruptionTypeKey] unsignedIntegerValue] == AVAudioSessionInterruptionTypeBegan;
        if (_interrupted) {
            [self stopGraph];
            _activated = NO;
            return;
        }
        if (([event.userInfo[AVAudioSessionInterruptionOptionKey] unsignedIntegerValue] & AVAudioSessionInterruptionOptionShouldResume) == 0) {
            return;
        }
    } else {
        if ([event.name isEqualToString:AVAudioSessionMediaServicesWereResetNotification]) {
            _activated = NO;
        } else if ([event.userInfo[AVAudioSessionRouteChangeReasonKey] unsignedIntegerValue] == AVAudioSessionRouteChangeReasonCategoryChange) {
            return;
        }
        [self stopGraph];
    }
    [self resumeGraph];
}

- (void)resumeGraph {
    if (!_requested || _interrupted || (!_manageActivation && !_callKitActive)) {
        return;
    }
    NSError *error = nil;
    if (![self startGraph:&error]) {
        [self stop];
        if (error) {
            _failure(error);
        }
    }
}

- (BOOL)startGraph:(NSError **)error {
    if (_engine) {
        return YES;
    }
    AVAudioSession *session = AVAudioSession.sharedInstance;
    if (![session.category isEqualToString:AVAudioSessionCategoryPlayAndRecord] ||
        (_manageActivation && (![session.mode isEqualToString:AVAudioSessionModeVoiceChat] ||
                               !(session.categoryOptions & AVAudioSessionCategoryOptionAllowBluetooth)))) {
        if (![session setCategory:AVAudioSessionCategoryPlayAndRecord mode:AVAudioSessionModeVoiceChat
                          options:AVAudioSessionCategoryOptionAllowBluetooth error:error]) {
            return NO;
        }
    }
    if (_manageActivation && !_activated) {
        if (![session setActive:YES error:error]) {
            return NO;
        }
        _activated = YES;
    }
    AVAudioEngine *engine = [[AVAudioEngine alloc] init];
    AVAudioInputNode *input = engine.inputNode;
    AVAudioFormat *format = [input outputFormatForBus:0];
    if (!SupportsCaptureRate((uint32_t)format.sampleRate) || format.channelCount == 0 ||
        format.commonFormat != AVAudioPCMFormatFloat32 || format.isInterleaved) {
        if (error) {
            *error = [NSError errorWithDomain:@"TgCallsAudio" code:1 userInfo:@{NSLocalizedDescriptionKey: @"Unsupported microphone format"}];
        }
        return NO;
    }

    _captureRate = (uint32_t)format.sampleRate;
    _captureChannels = (uint16_t)MIN(2, format.channelCount);
    uint16_t channels = _captureChannels;
    TGCallsAudioQueues *buffers = [[TGCallsAudioQueues alloc] init];
    [input installTapOnBus:0 bufferSize:_captureRate / 100 format:format block:^(AVAudioPCMBuffer *buffer, AVAudioTime *time) {
        (void)time;
        float *const *planes = buffer.floatChannelData;
        if (!planes) {
            return;
        }
        PCMQueue *queue = &buffers->capture;
        uint32_t write = atomic_load_explicit(&queue->write, memory_order_relaxed);
        uint32_t read = atomic_load_explicit(&queue->read, memory_order_acquire);
        uint32_t count = MIN(buffer.frameLength, QueueFrames - (write - read));
        for (uint32_t i = 0; i < count; i++) {
            int16_t *frame = queue->samples[(write + i) & (QueueFrames - 1)];
            frame[0] = FloatToPCM(planes[0][i]);
            frame[1] = FloatToPCM(planes[channels - 1][i]);
        }
        atomic_store_explicit(&queue->write, write + count, memory_order_release);
    }];

    AVAudioFormat *renderFormat = [[AVAudioFormat alloc] initStandardFormatWithSampleRate:48000 channels:2];
    AVAudioSourceNode *source = [[AVAudioSourceNode alloc] initWithFormat:renderFormat renderBlock:^OSStatus(BOOL *isSilence, const AudioTimeStamp *timestamp, AVAudioFrameCount count, AudioBufferList *output) {
        (void)timestamp;
        if (output->mNumberBuffers != 2 || count > QueueFrames ||
            !output->mBuffers[0].mData || !output->mBuffers[1].mData ||
            output->mBuffers[0].mDataByteSize < count * sizeof(float) ||
            output->mBuffers[1].mDataByteSize < count * sizeof(float)) {
            return kAudio_ParamError;
        }
        float *left = output->mBuffers[0].mData;
        float *right = output->mBuffers[1].mData;
        PCMQueue *play = &buffers->playback;
        PCMQueue *echo = &buffers->render;
        uint32_t read = atomic_load_explicit(&play->read, memory_order_relaxed);
        uint32_t write = atomic_load_explicit(&play->write, memory_order_acquire);
        uint32_t echoWrite = atomic_load_explicit(&echo->write, memory_order_relaxed);
        uint32_t echoRead = atomic_load_explicit(&echo->read, memory_order_acquire);
        uint32_t consumed = MIN(count, write - read);
        uint32_t referenced = MIN(count, QueueFrames - (echoWrite - echoRead));
        for (uint32_t i = 0; i < count; i++) {
            int16_t l = 0;
            int16_t r = 0;
            if (i < consumed) {
                const int16_t *frame = play->samples[(read + i) & (QueueFrames - 1)];
                l = frame[0];
                r = frame[1];
            }
            left[i] = (float)l / 32768.0f;
            right[i] = (float)r / 32768.0f;
            if (i < referenced) {
                int16_t *reference = echo->samples[(echoWrite + i) & (QueueFrames - 1)];
                reference[0] = l;
                reference[1] = r;
            }
        }
        *isSilence = consumed == 0;
        atomic_store_explicit(&play->read, read + consumed, memory_order_release);
        atomic_store_explicit(&echo->write, echoWrite + referenced, memory_order_release);
        return noErr;
    }];
    [engine attachNode:source];
    [engine connect:source to:engine.mainMixerNode format:renderFormat];
    if (![engine startAndReturnError:error]) {
        [input removeTapOnBus:0];
        [engine stop];
        return NO;
    }
    _engine = engine;
    _buffers = buffers;
    _pump = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, _queue);
    dispatch_source_set_timer(_pump, DISPATCH_TIME_NOW, 5 * NSEC_PER_MSEC, NSEC_PER_MSEC);
    __weak TGCallsIOSAudioDevice *weakSelf = self;
    dispatch_source_set_event_handler(_pump, ^{ [weakSelf drainBlocks]; });
    dispatch_resume(_pump);
    return YES;
}

- (void)stopGraph {
    if (_pump) {
        dispatch_source_cancel(_pump);
        _pump = nil;
    }
    if (_engine) {
        [_engine stop];
        [_engine.inputNode removeTapOnBus:0];
        _engine = nil;
    }
    _buffers = nil;
}

- (void)drainBlocks {
    TGCallsAudioQueues *buffers = _buffers;
    if (!buffers) {
        return;
    }
    int16_t pcm[RenderBlockFrames * 2];
    for (int i = 0; i < 4 && _buffers == buffers && ReadBlock(&buffers->render, RenderBlockFrames, 2, pcm); i++) {
        _render([NSData dataWithBytes:pcm length:sizeof(pcm)]);
    }
    AVAudioSession *session = AVAudioSession.sharedInstance;
    NSTimeInterval delay = session.inputLatency + session.outputLatency + session.IOBufferDuration;
    uint16_t milliseconds = (uint16_t)MIN(500.0, MAX(0.0, delay * 1000.0));
    uint32_t frames = _captureRate / 100;
    for (int i = 0; i < 4 && _buffers == buffers && ReadBlock(&buffers->capture, frames, _captureChannels, pcm); i++) {
        _capture([NSData dataWithBytes:pcm length:frames * _captureChannels * sizeof(int16_t)],
                 _captureRate, _captureChannels, milliseconds);
    }
}

@end
