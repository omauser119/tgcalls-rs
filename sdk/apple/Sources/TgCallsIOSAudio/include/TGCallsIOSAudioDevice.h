#import <Foundation/Foundation.h>
#import <dispatch/dispatch.h>

NS_ASSUME_NONNULL_BEGIN

typedef void (^TGCallsCaptureBlock)(NSData *pcm, uint32_t sampleRate, uint16_t channels, uint16_t delayMilliseconds);
typedef void (^TGCallsRenderBlock)(NSData *pcm);
typedef void (^TGCallsAudioErrorBlock)(NSError *error);

/// All methods and callbacks run on the serial queue passed to the initializer.
@interface TGCallsIOSAudioDevice : NSObject

- (instancetype)initWithQueue:(dispatch_queue_t)queue
          manageSessionActivation:(BOOL)manageSessionActivation
                          capture:(TGCallsCaptureBlock)capture
                           render:(TGCallsRenderBlock)render
                          failure:(TGCallsAudioErrorBlock)failure;
- (instancetype)initWithQueue:(dispatch_queue_t)queue
          manageSessionActivation:(BOOL)manageSessionActivation
                   captureEnabled:(BOOL)captureEnabled
                          capture:(TGCallsCaptureBlock)capture
                           render:(TGCallsRenderBlock)render
                          failure:(TGCallsAudioErrorBlock)failure NS_DESIGNATED_INITIALIZER;
- (instancetype)init NS_UNAVAILABLE;

- (BOOL)startAndReturnError:(NSError **)error NS_SWIFT_NAME(start());
- (void)stop;
- (void)setCallKitActive:(BOOL)active;
- (BOOL)setSpeakerEnabled:(BOOL)enabled error:(NSError **)error;

/// Enqueues stereo 48 kHz signed 16-bit PCM for playback. Full queues reject new frames.
- (BOOL)enqueuePlayback:(NSData *)pcm;

@end

NS_ASSUME_NONNULL_END
