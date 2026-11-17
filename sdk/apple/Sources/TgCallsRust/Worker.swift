import Foundation
import CTgCalls

public enum TgCallsError: Error { case worker(String), response, isolationUnavailable }

/// Low-level serialized worker. Network packets, PCM and I420 cross this bounded
/// request boundary; SDP, RTP, DH and codec parsing execute inside the worker.
public final class LocalWorker: @unchecked Sendable {
    private let lock = NSLock()
    private var handle: UInt64
    public init(wasmModule: Data? = nil, hardened: Bool) throws {
        if hardened {
            guard let module = wasmModule, !module.isEmpty else { throw TgCallsError.isolationUnavailable }
            handle = module.withUnsafeBytes { tgcalls_client_new(1, $0.bindMemory(to: UInt8.self).baseAddress, module.count) }
        } else { handle = tgcalls_client_new(0, nil, 0) }
        guard handle != 0 else { throw TgCallsError.worker(String(cString: tgcalls_last_error())) }
    }
    deinit { tgcalls_client_free(handle) }
    public var generation: UInt64 { lock.lock(); defer { lock.unlock() }; return tgcalls_client_generation(handle) }
    public func request(_ data: Data) throws -> Data {
        guard !data.isEmpty, data.count <= 8 * 1024 * 1024 else { throw TgCallsError.response }
        lock.lock(); defer { lock.unlock() }
        let ticket = data.withUnsafeBytes { tgcalls_client_request(handle, $0.bindMemory(to: UInt8.self).baseAddress, data.count) }
        guard ticket != 0 else { throw TgCallsError.worker(String(cString: tgcalls_last_error())) }
        defer { tgcalls_output_free(ticket) }
        let count = tgcalls_output_len(ticket)
        guard count > 0, count <= 32 * 1024 * 1024, let pointer = tgcalls_output_data(ticket) else { throw TgCallsError.response }
        return Data(bytes: pointer, count: count)
    }
}

public struct VideoConfig: Codable, Sendable {
    public var width: Int, height: Int, fps: Int, bitrate: Int
    public init(width: Int, height: Int, fps: Int, bitrate: Int) { self.width=width; self.height=height; self.fps=fps; self.bitrate=bitrate }
}
public struct Datagram: Decodable, Sendable { public let peer: String; public let bytes: [UInt8] }
public struct AudioFrame: Decodable, Sendable { public let ssrc: UInt32; public let timestamp: UInt32; public let pcm: [UInt8] }
public struct VideoFrame: Decodable, Sendable { public let ssrc: UInt32; public let timestamp: UInt32; public let width: Int; public let height: Int; public let i420: [UInt8] }
public struct WorkerResponse: Decodable, Sendable {
    public let ok: Bool
    public let error: String?
    public let datagrams: [Datagram]
    public let audio: [AudioFrame]
    public let video: [VideoFrame]
    public let signaling: [[UInt8]]
    enum CodingKeys: CodingKey { case ok, error, datagrams, audio, video, signaling }
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        ok = try c.decode(Bool.self, forKey: .ok); error = try c.decodeIfPresent(String.self, forKey: .error)
        datagrams = try c.decodeIfPresent([Datagram].self, forKey: .datagrams) ?? []
        audio = try c.decodeIfPresent([AudioFrame].self, forKey: .audio) ?? []
        video = try c.decodeIfPresent([VideoFrame].self, forKey: .video) ?? []
        signaling = try c.decodeIfPresent([[UInt8]].self, forKey: .signaling) ?? []
    }
}
