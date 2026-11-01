// The V8 platform with the frozen clock: V8 reads its wall clock through
// this one override, so Date, Intl and Temporal need no JS shims.
//
// Compiled against the vendored V8 headers and Chromium's libc++ (the `__Cr`
// ABI namespace of the prebuilt librusty_v8.a). The subclass adds no members:
// `new` allocates sizeof(DefaultPlatform) as these headers see it, so the
// headers must match the prebuilt build (release, no V8_FORCE_DEBUG).

#include <algorithm>
#include <cstdint>
#include <thread>

#include "src/libplatform/default-platform.h"

// rusty_v8's own C bindings: out-of-line, so no V8 feature define here can
// change what they read.
extern "C" v8::Isolate* v8__Isolate__TryGetCurrent();
extern "C" void* v8__Isolate__GetData(v8::Isolate* isolate, uint32_t slot);

namespace {

// The raw data slot holding a pointer to the isolate's frozen clock value
// (`webtypes::FrozenClock`): rusty_v8 slot 0 behind its one internal slot.
constexpr uint32_t kFrozenClockSlot = 1;

class FrozenClockPlatform final : public v8::platform::DefaultPlatform {
 public:
  explicit FrozenClockPlatform(int thread_pool_size)
      : DefaultPlatform(thread_pool_size,
                        v8::platform::IdleTaskSupport::kDisabled) {}

  // Entered isolate with a clock: its frozen value. Anything else (worker
  // threads, an isolate before boot) sees the real wall clock.
  double CurrentClockTimeMillis() override {
    v8::Isolate* isolate = v8__Isolate__TryGetCurrent();
    if (isolate != nullptr) {
      const void* clock = v8__Isolate__GetData(isolate, kFrozenClockSlot);
      if (clock != nullptr) return *static_cast<const double*>(clock);
    }
    return DefaultPlatform::CurrentClockTimeMillis();
  }
};

}  // namespace

// Same pool sizing as v8::platform::NewDefaultPlatform (0 = cores - 1, 1..16).
extern "C" v8::Platform* iso4_new_frozen_clock_platform(int thread_pool_size) {
  if (thread_pool_size < 1) {
    thread_pool_size = static_cast<int>(std::thread::hardware_concurrency()) - 1;
  }
  return new FrozenClockPlatform(std::max(std::min(thread_pool_size, 16), 1));
}
