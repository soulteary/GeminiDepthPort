// Copyright (c) 2026 GDP contributors. Licensed under the MIT License.
#pragma once
// Opt-in, read-only SDK evidence. C++11; no allocation, I/O, locks or workers.
#include <cstdint>
#include <ctime>
#include <cerrno>
namespace gdp_uvc {
struct Event {
    uint32_t backend, operation, selector, request;
    int32_t result, error_number, value;
    uint32_t value_valid;
    int32_t minimum, maximum, step, default_value;
    uint64_t started_ns, finished_ns;
};
static_assert(sizeof(Event) == 64, "private UVC ABI v1");
struct State {
    bool armed;
    uint32_t count;
    Event events[8];
    State() : armed(false), count(0), events{} {}
};
inline State &state() { static thread_local State value; return value; }
inline uint64_t stamp() noexcept {
    if(!state().armed) return 0;
    const int saved = errno;
    timespec ts{};
    const bool ok = clock_gettime(CLOCK_MONOTONIC, &ts) == 0;
    errno = saved;
    return ok ? static_cast<uint64_t>(ts.tv_sec) * 1000000000ULL + static_cast<uint64_t>(ts.tv_nsec) : 0;
}
inline void begin() noexcept { state().count = 0; state().armed = true; }
inline void record(Event value) noexcept {
    State &s = state();
    if(!s.armed) return;
    if(s.count < 8) { value.finished_ns = stamp(); s.events[s.count++] = value; }
    else s.count = 9; // Saturated overflow marker; never overwrites old evidence.
}
inline uint32_t snapshot(Event *output, uint32_t capacity) noexcept {
    State &s = state();
    s.armed = false;
    const uint32_t copy = s.count < 8 ? s.count : 8;
    if(output) for(uint32_t i=0; i<copy && i<capacity; ++i) output[i] = s.events[i];
    return s.count;
}
} // namespace gdp_uvc
