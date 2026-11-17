import Foundation
import XCTest
@testable import TgCallsRust
final class WorkerTests: XCTestCase {
    func testNativeWorker() throws {
        let worker = try LocalWorker(hardened: false)
        let result = try worker.request(Data(#"{"op":"new","local":"127.0.0.1:40001","ssrc":1234}"#.utf8))
        XCTAssertTrue(try JSONDecoder().decode(WorkerResponse.self, from: result).ok)
    }
    func testHardenedRequiresModule() { XCTAssertThrowsError(try LocalWorker(hardened: true)) }
    func testHardenedWorkerOnApplePlatform() throws {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "tgcalls-worker", withExtension: "wasm", subdirectory: "Resources"))
        let worker = try LocalWorker(wasmModule: Data(contentsOf: url), hardened: true)
        let response = try worker.request(Data(#"{"op":"new","local":"127.0.0.1:40002","ssrc":4321,"video":{"width":32,"height":32,"fps":10,"bitrate":64000}}"#.utf8))
        XCTAssertTrue(try JSONDecoder().decode(WorkerResponse.self, from: response).ok)
        let offer = try worker.request(Data(#"{"op":"offer"}"#.utf8))
        XCTAssertTrue(try JSONDecoder().decode(WorkerResponse.self, from: offer).ok)
        XCTAssertEqual(worker.generation, 0)
    }
}
