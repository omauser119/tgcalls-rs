import Foundation

/// The app owns capture/playback, UDP sockets and MTProto. Feed every returned
/// datagram to its destination and every incoming encrypted datagram to packet.
/// Call tick every 10 ms while a call is active. Restart negotiation after errors.
public actor TgCallsClient {
    public enum Isolation: Sendable, Equatable { case standard, hardened }
    private let local: LocalWorker?
    #if os(macOS)
    private let xpc: XpcWorker?
    #endif
    public init(isolation: Isolation, wasmModule: Data? = nil, macosService: String? = nil) throws {
        #if os(iOS)
        local = try LocalWorker(wasmModule: wasmModule, hardened: isolation == .hardened)
        #elseif os(macOS)
        if isolation == .hardened {
            guard let name = macosService, !name.isEmpty else { throw TgCallsError.isolationUnavailable }
            xpc = XpcWorker(service: name); local = nil
        } else { local = try LocalWorker(hardened: false); xpc = nil }
        #else
        local = try LocalWorker(wasmModule: wasmModule, hardened: isolation == .hardened)
        #endif
    }
    public func request(_ data: Data) async throws -> Data {
        let response: Data
        #if os(macOS)
        if let xpc { response = try await xpc.request(data) }
        else if let local { response = try local.request(data) }
        else { throw TgCallsError.isolationUnavailable }
        #else
        guard let local else { throw TgCallsError.isolationUnavailable }; response = try local.request(data)
        #endif
        let decoded = try JSONDecoder().decode(WorkerResponse.self, from: response)
        guard decoded.ok else { throw TgCallsError.worker(decoded.error ?? "worker rejected request") }
        return response
    }
}
