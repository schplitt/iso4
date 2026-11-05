// V8's data-carrying OOM handler and its process-wide OOM callback, which
// rusty_v8 does not bind. Compiled with the frozen-clock platform.

#include "v8-initialization.h"
#include "v8-isolate.h"

extern "C" void iso4_set_oom_error_handler(v8::Isolate* isolate,
                                           v8::OOMErrorCallbackWithData callback,
                                           void* data) {
  isolate->SetOOMErrorHandler(callback, data);
}

extern "C" void iso4_set_fatal_memory_error_callback(v8::OOMErrorCallback callback) {
  v8::V8::SetFatalMemoryErrorCallback(callback);
}
