// swift-tools-version: 5.9
import PackageDescription
let package = Package(
    name: "TgCallsRust", platforms: [.iOS(.v15), .macOS(.v13)],
    products: [.library(name: "TgCallsRust", targets: ["TgCallsRust"]),
               .executable(name: "TgCallsWorker", targets: ["TgCallsWorker"])],
    targets: [.systemLibrary(name: "CTgCalls"),
              .target(name: "TgCallsRust", dependencies: ["CTgCalls"]),
              .executableTarget(name: "TgCallsWorker", dependencies: ["TgCallsRust"]),
              .testTarget(name: "TgCallsRustTests", dependencies: ["TgCallsRust"], resources: [.copy("Resources")])])
