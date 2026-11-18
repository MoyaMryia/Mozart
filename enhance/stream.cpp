#include "stream.hpp"
#include <algorithm>
#include <cmath>
#include <cstring>
#include <numbers>
#include <stdexcept>

namespace mozart::enhance {
void fft(std::array<std::complex<float>, fft_size>& a, bool inverse) {
    for (std::size_t i = 1, j = 0; i < fft_size; ++i) {
        std::size_t bit = fft_size >> 1;
        for (; j & bit; bit >>= 1) j ^= bit;
        j ^= bit;
        if (i < j) std::swap(a[i], a[j]);
    }
    for (std::size_t n = 2; n <= fft_size; n *= 2) {
        const float angle = (inverse ? 2.0f : -2.0f) * std::numbers::pi_v<float> / float(n);
        const std::complex<float> root(std::cos(angle), std::sin(angle));
        for (std::size_t i = 0; i < fft_size; i += n) {
            std::complex<float> w(1, 0);
            for (std::size_t j = 0; j < n / 2; ++j) {
                const auto u = a[i+j], v = a[i+j+n/2] * w;
                a[i+j] = u+v; a[i+j+n/2] = u-v; w *= root;
            }
        }
    }
    if (inverse) for (auto& v : a) v /= float(fft_size);
}
Stream::Stream(SpectrumModel& model) : model_(model) {
    for (std::size_t i = 0; i < fft_size; ++i)
        window_[i] = float(0.5 - 0.5 * std::cos(2 * std::numbers::pi * double(i) / fft_size));
    reset();
}
void Stream::reset() {
    input_.fill(0); output_.fill(0); ola_.fill(0); weight_.fill(0);
    total_ = 0; start_ = -static_cast<std::int64_t>(hop); written_ = read_ = 0;
    model_.reset();
}
void Stream::push(std::span<const float> pcm) {
    for (float v : pcm) {
        input_[std::size_t(total_) % input_.size()] = v;
        ++total_;
        if (total_ >= start_ + static_cast<std::int64_t>(fft_size) && total_ > static_cast<std::int64_t>(hop)) step();
    }
}
void Stream::step() {
    std::array<std::complex<float>, fft_size> a{};
    for (std::size_t j = 0; j < fft_size; ++j) {
        auto p = start_ + static_cast<std::int64_t>(j);
        if (p < 0) p = -p;
        a[j] = input_[std::size_t(p) % input_.size()] * window_[j];
    }
    fft(a, false);
    Spectrum spectrum{};
    std::copy_n(a.begin(), bins, spectrum.begin());
    spectrum = model_.infer(spectrum);
    for (const auto v : spectrum)
        if (!std::isfinite(v.real()) || !std::isfinite(v.imag())) throw std::runtime_error("nonfinite model spectrum");
    std::copy(spectrum.begin(), spectrum.end(), a.begin());
    for (std::size_t j = bins; j < fft_size; ++j) a[j] = std::conj(a[fft_size-j]);
    fft(a, true);
    for (std::size_t j = 0; j < fft_size; ++j) {
        auto p = start_ + static_cast<std::int64_t>(j);
        if (p < 0) continue;
        auto k = std::size_t(p) % ola_.size();
        ola_[k] += a[j].real() * window_[j]; weight_[k] += window_[j]*window_[j];
    }
    for (std::size_t j = 0; j < hop; ++j) {
        auto p = start_ + static_cast<std::int64_t>(j);
        if (p < 0) continue;
        auto k = std::size_t(p) % ola_.size();
        const float value = ola_[k] / weight_[k];
        if (!std::isfinite(value)) throw std::runtime_error("nonfinite synthesis output");
        output_[written_++ % output_.size()] = value;
        ola_[k] = weight_[k] = 0;
    }
    start_ += hop;
}
void Stream::pop(std::span<float> pcm) {
    for (float& v : pcm) v = output_[read_++ % output_.size()];
}
void Session::reset() { stream_.reset(); count_ = 0; previous_ = 0; }
mozart_enhance_reply_t Session::process(const mozart_input_frame_t& frame) {
    std::uint32_t idx;
    std::memcpy(&idx, reinterpret_cast<const char*>(&frame.meta) + offsetof(mozart_frame_meta_t, frame_idx), sizeof(idx));
    if (count_ && idx != previous_ + 1) reset();
    previous_ = idx;
    std::memcpy(&meta_[count_ % meta_.size()], &frame.meta, sizeof(frame.meta));
    std::array<float, MOZART_INPUT_SAMPLES> pcm{};
    std::memcpy(pcm.data(), frame.pcm, sizeof(frame.pcm));
    stream_.push(pcm);
    mozart_enhance_reply_t reply{};
    reply.magic = MOZART_ENHANCE_PACKET_MAGIC;
    if (count_ >= MOZART_ENHANCE_PRIMING_FRAMES) {
        reply.ready = 1;
        std::memcpy(&reply.frame.meta, &meta_[(count_ - MOZART_ENHANCE_PRIMING_FRAMES) % meta_.size()], sizeof(reply.frame.meta));
        stream_.pop(pcm);
        for (float& sample : pcm) sample = std::clamp(sample, -MOZART_INPUT_PCM_ABS_MAX, MOZART_INPUT_PCM_ABS_MAX);
        std::memcpy(reply.frame.pcm, pcm.data(), sizeof(reply.frame.pcm));
    }
    ++count_;
    return reply;
}
}
