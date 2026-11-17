#if os(macOS)
import Foundation
import Security
import TgCallsRust

func isolatedEntitlements() -> Bool {
    var code: SecCode?
    guard SecCodeCopySelf(SecCSFlags(rawValue: 0), &code) == errSecSuccess, let code else { return false }
    var staticCode: SecStaticCode?
    guard SecCodeCopyStaticCode(code, SecCSFlags(rawValue: 0), &staticCode) == errSecSuccess, let staticCode else { return false }
    var information: CFDictionary?
    guard SecCodeCopySigningInformation(staticCode, SecCSFlags(rawValue: kSecCSSigningInformation), &information) == errSecSuccess,
          let info = information as? [String: Any],
          let entitlements = info[kSecCodeInfoEntitlementsDict as String] as? [String: Any],
          entitlements["com.apple.security.app-sandbox"] as? Bool == true else { return false }
    for key in ["com.apple.security.inherit", "com.apple.security.network.client", "com.apple.security.network.server",
                "com.apple.security.files.user-selected.read-write", "com.apple.security.files.user-selected.read-only"] {
        if entitlements[key] as? Bool == true { return false }
    }
    return entitlements["com.apple.security.application-groups"] == nil
}

final class Service: NSObject, TgCallsWorkerProtocol {
    private let worker: LocalWorker
    init(worker: LocalWorker) { self.worker = worker }
    func request(_ data: Data, reply: @escaping (Data?, String?) -> Void) {
        do { reply(try worker.request(data), nil) } catch { reply(nil, String(describing: error)) }
    }
}
final class Delegate: NSObject, NSXPCListenerDelegate {
    func listener(_ listener: NSXPCListener, shouldAcceptNewConnection connection: NSXPCConnection) -> Bool {
        // NSXPC service lookup is scoped to the containing signed app bundle.
        guard isolatedEntitlements(), let worker = try? LocalWorker(hardened: false) else { return false }
        connection.exportedInterface = NSXPCInterface(with: TgCallsWorkerProtocol.self)
        connection.exportedObject = Service(worker: worker); connection.resume(); return true
    }
}
let delegate = Delegate()
let listener = NSXPCListener.service()
listener.delegate = delegate
listener.resume()
#else
// An iOS app links TgCallsRust; iOS never launches this macOS-only helper.
#endif
