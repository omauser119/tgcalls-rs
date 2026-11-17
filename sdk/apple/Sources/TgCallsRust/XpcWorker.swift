#if os(macOS)
import Foundation

@objc public protocol TgCallsWorkerProtocol {
    func request(_ data: Data, reply: @escaping (Data?, String?) -> Void)
}
/// Embed the signed XPC service in Contents/XPCServices. It has a distinct App
/// Sandbox container and no file/network entitlements; the app brokers datagrams.
public final class XpcWorker: @unchecked Sendable {
    private let connection: NSXPCConnection
    public init(service: String) {
        connection = NSXPCConnection(serviceName: service)
        connection.remoteObjectInterface = NSXPCInterface(with: TgCallsWorkerProtocol.self)
        connection.resume()
    }
    deinit { connection.invalidate() }
    public func request(_ data: Data) async throws -> Data {
        guard data.count <= 8 * 1024 * 1024 else { throw TgCallsError.response }
        return try await withCheckedThrowingContinuation { continuation in
            let once = Completion(continuation)
            let proxy = connection.remoteObjectProxyWithErrorHandler { once.finish(.failure($0)) } as? TgCallsWorkerProtocol
            guard let proxy else { once.finish(.failure(TgCallsError.isolationUnavailable)); return }
            proxy.request(data) { response, error in
                if let error { once.finish(.failure(TgCallsError.worker(error))) }
                else if let response, response.count <= 32 * 1024 * 1024 { once.finish(.success(response)) }
                else { once.finish(.failure(TgCallsError.response)) }
            }
            DispatchQueue.global().asyncAfter(deadline: .now() + 20) { once.finish(.failure(TgCallsError.worker("XPC worker timeout; re-establish call"))) }
        }
    }
}
private final class Completion: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<Data, Error>?
    init(_ continuation: CheckedContinuation<Data, Error>) { self.continuation = continuation }
    func finish(_ result: Result<Data, Error>) {
        lock.lock(); let pending = continuation; continuation = nil; lock.unlock(); pending?.resume(with: result)
    }
}
#endif
