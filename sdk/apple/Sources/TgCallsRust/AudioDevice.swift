import AVFoundation
import Foundation

public final class CallAudioDevice {
    public struct Input: Sendable {
        public let rate: Int
        public let channels: Int
        public let pcm: Data
    }

    public let capture: AsyncStream<Input>
    private let captureContinuation: AsyncStream<Input>.Continuation
    private let captureBuffer = CaptureBuffer()
    private let engine = AVAudioEngine()
    private let player = AVAudioPlayerNode()
    private let lock = NSLock()
    private var outstanding = 0
    private var running = false

    public init() {
        let pair = AsyncStream<Input>.makeStream(bufferingPolicy: .bufferingNewest(20))
        capture = pair.stream
        captureContinuation = pair.continuation
        engine.attach(player)
    }

    public func start() throws {
        guard !running else { return }
        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        guard format.channelCount > 0, [8000.0, 16000, 24000, 32000, 44100, 48000].contains(format.sampleRate),
              format.commonFormat == .pcmFormatFloat32, !format.isInterleaved,
              let playbackFormat = AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 2) else {
            throw TgCallsError.worker("Unsupported device PCM format")
        }
        engine.connect(player, to: engine.mainMixerNode, format: playbackFormat)
        let continuation = captureContinuation
        let buffer = captureBuffer
        input.installTap(onBus: 0, bufferSize: 480, format: format) { frame, _ in
            buffer.append(frame, deliver: continuation)
        }
        do {
            try engine.start()
        } catch {
            input.removeTap(onBus: 0)
            throw error
        }
        running = true
        player.play()
    }

    /// Queue stereo s16le at 48 kHz. The caller can submit the accepted data as
    /// the render reference for echo cancellation. A full playback queue drops
    /// new input and the caller then omits the matching reference.
    public func play(_ pcm: Data) -> Bool {
        guard running, pcm.count > 0, pcm.count <= 23_040, pcm.count.isMultiple(of: 4),
              let format = AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 2),
              let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(pcm.count / 4)),
              let channels = buffer.floatChannelData else {
            return false
        }

        lock.lock()
        guard outstanding < 8 else { lock.unlock(); return false }
        outstanding += 1
        lock.unlock()
        buffer.frameLength = buffer.frameCapacity
        pcm.withUnsafeBytes { (bytes: UnsafeRawBufferPointer) in
            for index in 0..<Int(buffer.frameLength) {
                for channel in 0..<2 {
                    let offset = 4 * index + 2 * channel
                    let word = UInt16(bytes[offset]) | (UInt16(bytes[offset + 1]) << 8)
                    channels[channel][index] = Float(Int16(bitPattern: word)) / 32768
                }
            }
        }
        player.scheduleBuffer(buffer, completionCallbackType: .dataPlayedBack) { [weak self] _ in
            guard let self else { return }
            self.lock.lock()
            self.outstanding -= 1
            self.lock.unlock()
        }
        return true
    }

    public func stop() {
        if running {
            running = false
            engine.inputNode.removeTap(onBus: 0)
            player.stop()
            engine.stop()
        }
        captureContinuation.finish()
    }

    deinit { stop() }
}

private final class CaptureBuffer: @unchecked Sendable {
    private var samples = Data()

    func append(_ frame: AVAudioPCMBuffer, deliver: AsyncStream<CallAudioDevice.Input>.Continuation) {
        guard let planes = frame.floatChannelData else { return }
        let rate = Int(frame.format.sampleRate)
        let channels = min(2, Int(frame.format.channelCount))
        let block = (rate / 100) * channels * 2
        for index in 0..<Int(frame.frameLength) {
            for channel in 0..<channels {
                let source = planes[channel][index]
                let value = source.isFinite ? max(-1, min(1, source)) : 0
                let sample = UInt16(bitPattern: Int16((value * 32767).rounded()))
                samples.append(UInt8(truncatingIfNeeded: sample))
                samples.append(UInt8(truncatingIfNeeded: sample >> 8))
            }
            if samples.count == block {
                deliver.yield(.init(rate: rate, channels: channels, pcm: samples))
                samples.removeAll(keepingCapacity: true)
            }
        }
    }
}
