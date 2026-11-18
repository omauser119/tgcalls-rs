#include "td/e2e/Blockchain.h"
#include "td/e2e/e2e_api.h"

#include <array>
#include <cstring>
#include <memory>

namespace {
using namespace tde2e_api;
constexpr std::array<UserId, 2> users{101, 202};

struct Conference {
    std::array<PrivateKeyId, 2> keys{};
    std::array<CallId, 2> calls{};

    ~Conference() {
        for (const auto call : calls) {
            if (call != 0) {
                call_destroy(call);
            }
        }
        for (const auto key : keys) {
            if (key != 0) {
                key_destroy(key);
            }
        }
    }
};
}

extern "C" void *tgcalls_bench_crypto_new() noexcept {
    try {
        set_log_verbosity_level(0);
        auto result = std::make_unique<Conference>();
        std::array<CallParticipant, 2> participants;
        for (size_t i = 0; i < users.size(); ++i) {
            auto key = key_generate_temporary_private_key();
            if (!key.is_ok()) return nullptr;
            result->keys[i] = key.value();
            auto public_key = key_to_public_key(key.value());
            if (!public_key.is_ok()) return nullptr;
            auto public_id = key_from_public_key(public_key.value());
            if (!public_id.is_ok()) return nullptr;
            participants[i] = CallParticipant{users[i], public_id.value(), 3};
        }

        // The transport fixture acts as the server that confirms a join block.
        auto local = call_create_zero_block(result->keys[0], CallState{0, {participants[0]}});
        if (!local.is_ok()) return nullptr;
        auto genesis = tde2e_core::Blockchain::from_local_to_server(local.value());
        if (!genesis.is_ok()) return nullptr;
        local = call_create_self_add_block(result->keys[1], genesis.ok(), participants[1]);
        if (!local.is_ok()) return nullptr;
        auto joined = tde2e_core::Blockchain::from_local_to_server(local.value());
        if (!joined.is_ok()) return nullptr;
        for (size_t i = 0; i < users.size(); ++i) {
            auto call = call_create(users[i], result->keys[i], joined.ok());
            if (!call.is_ok()) return nullptr;
            result->calls[i] = call.value();
        }
        return result.release();
    } catch (...) {
        return nullptr;
    }
}

extern "C" size_t tgcalls_bench_crypto_frame(void *handle, int index, bool encrypt, size_t prefix,
    const char *input, size_t length, char *output, size_t capacity) noexcept {
    if (!handle || index < 0 || index > 1 || !input || !output || prefix >= length) return 0;
    try {
        const auto &session = *static_cast<Conference *>(handle);
        const Slice bytes(input, length);
        const auto result = encrypt ? call_encrypt(session.calls[index], 0, bytes, prefix)
                                    : call_decrypt(session.calls[index], users[1 - index], 0, bytes);
        if (!result.is_ok() || result.value().size() > capacity) return 0;
        std::memcpy(output, result.value().data(), result.value().size());
        return result.value().size();
    } catch (...) {
        return 0;
    }
}

extern "C" void tgcalls_bench_crypto_free(void *handle) noexcept {
    delete static_cast<Conference *>(handle);
}
