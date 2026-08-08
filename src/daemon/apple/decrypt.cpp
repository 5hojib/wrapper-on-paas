#include "apple/decrypt.hpp"

#include <algorithm>
#include <atomic>
#include <condition_variable>
#include <cstdio>
#include <deque>
#include <exception>
#include <functional>
#include <mutex>
#include <string>
#include <thread>
#include <unordered_map>
#include <utility>
#include <vector>

#include "apple/fps_cert.inc"
#include "apple/loader.hpp"
#include "apple/runtime.hpp"

namespace wrapper::apple {

namespace {

constexpr char kPrefetchAdam[] = "0";

// zhaarey/wrapper main.c getKdContext:
// - Never destroys SVFootHillPContext (stack POD shared_ptr, no refcount decrement).
// - preshareCtx: first successful adam=="0" path caches kd; later adam=="0" skips
//   getPersistentKey entirely (no URI check in upstream).
// - This wrapper also caches regular track kd contexts by adam_id + key URI so
//   batch decrypt requests do not repeatedly process the same CKC.
//
// Calling shared_ptr_SVFootHillPContext_dtor breaks kd / Apple's FootHill state.

std::mutex g_kd_cache_mu;
std::unordered_map<std::string, void*> g_kd_cache;

// FIFO of insertion order for regular track entries so the cache stays
// bounded on a long-running daemon. The prefetch entry (adam_id=="0") is
// pinned: decrypt_once() hard-errors if it is ever missing, so it must never
// be evicted and is deliberately kept out of this queue.
std::deque<std::string> g_kd_cache_order;

// Cap on cached regular-track kd contexts. Each entry is only a map node plus
// its string key; the kd pointers themselves are owned by Apple's FootHill
// session, so evicting an entry just forces a getPersistentKey re-derivation
// on the next request for that track. We do NOT free the kd pointer.
constexpr std::size_t kKdCacheMaxEntries = 512;

// Persistent thread pool for sample decryption. Avoids spawning/joining
// threads on every /decrypt call. Sized once at first use.
struct DecryptThreadPool {
    std::vector<std::thread> workers;
    std::deque<std::function<void()>> queue;
    std::mutex mu;
    std::condition_variable cv;
    std::condition_variable done_cv;
    unsigned int active = 0;
    bool stop = false;

    explicit DecryptThreadPool(unsigned int n) {
        workers.reserve(n);
        for (unsigned int i = 0; i < n; ++i) {
            workers.emplace_back([this] {
                for (;;) {
                    std::function<void()> task;
                    {
                        std::unique_lock<std::mutex> lk(mu);
                        cv.wait(lk, [this] { return stop || !queue.empty(); });
                        if (stop && queue.empty()) return;
                        task = std::move(queue.front());
                        queue.pop_front();
                        ++active;
                    }
                    task();
                    {
                        std::lock_guard<std::mutex> lk(mu);
                        --active;
                    }
                    done_cv.notify_all();
                }
            });
        }
    }

    void run_all(std::vector<std::function<void()>> tasks) {
        {
            std::lock_guard<std::mutex> lk(mu);
            for (auto& t : tasks) queue.push_back(std::move(t));
        }
        cv.notify_all();
        std::unique_lock<std::mutex> lk(mu);
        done_cv.wait(lk, [this] { return queue.empty() && active == 0; });
    }

    ~DecryptThreadPool() {
        { std::lock_guard<std::mutex> lk(mu); stop = true; }
        cv.notify_all();
        for (auto& w : workers) w.join();
    }
};

DecryptThreadPool& get_thread_pool() {
    static DecryptThreadPool pool([] {
        unsigned int n = std::thread::hardware_concurrency();
        return (n == 0) ? 4u : n;
    }());
    return pool;
}

std::string kd_cache_key(const std::string& adam_id, const std::string& key_uri) {
    std::string key;
    key.reserve(adam_id.size() + 1 + key_uri.size());
    key.append(adam_id);
    key.push_back('\n');
    key.append(key_uri);
    return key;
}

void* find_cached_kd(const std::string& adam_id, const std::string& key_uri) {
    std::lock_guard<std::mutex> lock(g_kd_cache_mu);
    auto it = g_kd_cache.find(kd_cache_key(adam_id, key_uri));
    if (it == g_kd_cache.end()) return nullptr;
    return it->second;
}

void store_cached_kd(const std::string& adam_id, const std::string& key_uri, void* kd) {
    std::lock_guard<std::mutex> lock(g_kd_cache_mu);
    std::string key = kd_cache_key(adam_id, key_uri);
    auto [it, inserted] = g_kd_cache.insert_or_assign(std::move(key), kd);
    if (!inserted || adam_id == kPrefetchAdam) {
        // Re-store of an existing entry, or the pinned prefetch context:
        // nothing to enqueue / evict.
        return;
    }
    g_kd_cache_order.push_back(it->first);
    while (g_kd_cache_order.size() > kKdCacheMaxEntries) {
        const std::string& oldest = g_kd_cache_order.front();
        g_kd_cache.erase(oldest);
        g_kd_cache_order.pop_front();
    }
}

void erase_cached_kd(const std::string& adam_id, const std::string& key_uri) {
    std::lock_guard<std::mutex> lock(g_kd_cache_mu);
    const std::string key = kd_cache_key(adam_id, key_uri);
    if (g_kd_cache.erase(key) == 0) return;
    auto it = std::find(g_kd_cache_order.begin(), g_kd_cache_order.end(), key);
    if (it != g_kd_cache_order.end()) {
        g_kd_cache_order.erase(it);
    }
}

}  // namespace

DecryptResult decrypt_samples(const Loader& loader,
                              Runtime&      runtime,
                              std::string   adam_id,
                              std::string   key_uri,
                              std::vector<std::vector<std::uint8_t>> ciphertexts) {
    DecryptResult out;
    if (!loader.ok() || !loader.fps_decrypt_available()) {
        out.error = "FPS decrypt chain not loaded";
        return out;
    }
    if (!runtime.playback_ready()) {
        out.error = "playback stack not ready";
        return out;
    }
    if (adam_id.empty() || key_uri.empty()) {
        out.error = "adam_id and uri are required";
        return out;
    }
    if (ciphertexts.empty()) {
        out.error = "at least one sample required";
        return out;
    }

    const Symbols& s  = loader.sym();
    void*          fh = runtime.foothill_session();

    auto decrypt_once = [&](bool allow_cache,
                            std::vector<std::vector<std::uint8_t>> chunks,
                            std::string* error) -> DecryptResult {
        DecryptResult attempt;
        void* kd = nullptr;

        if (allow_cache) {
            kd = find_cached_kd(adam_id, key_uri);
        }

        if (kd == nullptr && adam_id == kPrefetchAdam) {
            *error = "prefetch decrypt context is not cached; decrypt prefetch samples locally";
            return attempt;
        }

        if (kd == nullptr) {
            auto        default_id = abi::make_string_view(adam_id.c_str());
            auto        uri        = abi::make_string_view(key_uri.c_str());
            auto        key_format = abi::make_string_view("com.apple.streamingkeydelivery");
            auto        key_ver    = abi::make_string_view("1");
            auto        server_uri =
                abi::make_string_view("https://play.itunes.apple.com/WebObjects/MZPlay.woa/music/fps");
            auto        protocol   = abi::make_string_view("simplified");
            auto        fps_cert   = abi::make_string_view(kFpsCert);

            abi::shared_ptr persist{};
            loader.foot_hill_get_persistent_key(
                &persist, fh,
                &default_id, &uri, &key_format, &key_ver,
                &server_uri, &protocol, &fps_cert);

            if (persist.obj == nullptr) {
                *error = "getPersistentKey failed (key or lease?)";
                return attempt;
            }

            abi::shared_ptr sv_ctx{};
            s.SVFootHillSessionCtrl_decryptContext(&sv_ctx, fh, persist.obj);

            if (sv_ctx.obj == nullptr) {
                *error = "decryptContext failed";
                return attempt;
            }

            // Upstream main.c does TWO dereferences:
            //   void* p = *kdContext_method(ctx);   // *(void**) -> void*
            //   ... NfcRKVn(*(void**)p, ...)         // re-cast and deref again
            // i.e. fp_sample_decrypt receives **kdContext_method(ctx). Doing only
            // one deref passes the kd-handle struct pointer instead of the actual
            // engine state pointer; fp_sample_decrypt doesn't error but the
            // produced plaintext is garbage (audio plays back unplayable).
            void** kd_pp = s.SVFootHillPContext_kdContext(sv_ctx.obj);
            if (kd_pp == nullptr || *kd_pp == nullptr) {
                *error = "kdContext is null";
                return attempt;
            }
            kd = *reinterpret_cast<void**>(*kd_pp);
            if (kd == nullptr) {
                *error = "kdContext inner pointer is null";
                return attempt;
            }

            store_cached_kd(adam_id, key_uri, kd);

            // Intentionally no shared_ptr dtors — see block comment above.
            (void)persist;
            (void)sv_ctx;
        }

        std::fprintf(stderr, "decrypt: fp_sample_decrypt samples=%zu\n", chunks.size());

        attempt.plaintexts.resize(chunks.size());

        unsigned int hw_concurrency = std::thread::hardware_concurrency();
        unsigned int num_threads = (hw_concurrency == 0) ? 4 : hw_concurrency;
        if (num_threads > chunks.size()) {
            num_threads = static_cast<unsigned int>(chunks.size());
        }

        std::atomic<bool> error_occurred{false};
        std::mutex error_mutex;

        std::vector<std::function<void()>> tasks;
        tasks.reserve(num_threads);
        for (unsigned int t = 0; t < num_threads; ++t) {
            tasks.emplace_back([&, t]() {
                for (size_t i = t; i < chunks.size(); i += num_threads) {
                    if (error_occurred.load(std::memory_order_relaxed)) break;
                    if (chunks[i].empty()) {
                        std::lock_guard<std::mutex> lock(error_mutex);
                        if (!error_occurred.exchange(true)) *error = "empty sample";
                        break;
                    }
                    attempt.plaintexts[i] = chunks[i];
                    auto& chunk = attempt.plaintexts[i];
                    const long status = s.fp_sample_decrypt(kd, 5u, chunk.data(), chunk.data(), chunk.size());
                    if (status < 0) {
                        std::lock_guard<std::mutex> lock(error_mutex);
                        if (!error_occurred.exchange(true)) {
                            *error = "FairPlay sample decrypt failed status=" + std::to_string(status);
                            std::fprintf(stderr, "decrypt: fp_sample_decrypt failed status=%ld\n", status);
                        }
                        break;
                    }
                }
            });
        }
        get_thread_pool().run_all(std::move(tasks));

        if (error_occurred.load()) {
            attempt.plaintexts.clear();
            return attempt;
        }

        attempt.ok = true;
        return attempt;
    };

    std::string first_error;
    try {
        out = decrypt_once(true, ciphertexts, &first_error);
    } catch (const std::exception& e) {
        first_error = e.what();
    } catch (...) {
        first_error = "native FPS decrypt threw an unknown exception";
    }
    if (out.ok) return out;

    erase_cached_kd(adam_id, key_uri);
    out.error = "FPS decrypt failed";
    if (!first_error.empty()) out.error += " (first: " + first_error + ")";
    return out;
}

}  // namespace wrapper::apple
