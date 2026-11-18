#pragma once
#include <mozart/frame.h>
#include <array>
#include <complex>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <span>
#include <string>

namespace mozart::enhance {
constexpr std::size_t fft_size = 512, hop = 256, bins = 257;
using Spectrum = std::array<std::complex<float>, bins>;
class SpectrumModel {
public:
    virtual ~SpectrumModel() = default;
    virtual void reset() = 0;
    virtual Spectrum infer(const Spectrum&) = 0;
};
std::unique_ptr<SpectrumModel> make_model(const std::string& path);
void fft(std::array<std::complex<float>, fft_size>& data, bool inverse);
// Input spans must contain finite PCM. Drain output between pushes of at most 320 samples.
class Stream {
public:
    explicit Stream(SpectrumModel& model);
    void reset();
    void push(std::span<const float> pcm);
    std::size_t available() const { return written_ - read_; }
    void pop(std::span<float> pcm);
private:
    void step();
    SpectrumModel& model_;
    std::array<float, fft_size> window_{};
    std::array<float, 2048> input_{}, output_{};
    std::array<float, 1024> ola_{}, weight_{};
    std::int64_t total_ = 0, start_ = -static_cast<std::int64_t>(hop);
    std::size_t written_ = 0, read_ = 0;
};
class Session {
public:
    explicit Session(SpectrumModel& model) : stream_(model) {}
    mozart_enhance_reply_t process(const mozart_input_frame_t& frame);
    void reset();
private:
    Stream stream_;
    std::array<mozart_frame_meta_t, MOZART_ENHANCE_PRIMING_FRAMES + 1> meta_{};
    std::uint64_t count_ = 0;
    std::uint32_t previous_ = 0;
};
}
