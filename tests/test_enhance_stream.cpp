#include "stream.hpp"
#include <algorithm>
#include <cmath>
#include <cstring>
#include <iostream>
#include <numbers>
#include <limits>
#include <random>
#include <stdexcept>
#include <vector>
using namespace mozart::enhance;
namespace {
void check(bool condition, const char* reason) { if (!condition) throw std::runtime_error(reason); }
class Identity final : public SpectrumModel {
public:
    int resets = 0;
    void reset() override { ++resets; }
    Spectrum infer(const Spectrum& input) override { return input; }
};
std::vector<float> run(const std::vector<float>& input, std::size_t chunk) {
    Identity identity; Stream stream(identity);
    std::vector<float> output;
    for (std::size_t pos = 0; pos < input.size()+1024; pos += chunk) {
        std::vector<float> block(std::min(chunk, input.size()+1024-pos));
        for (std::size_t j = 0; j < block.size(); ++j) if (pos+j < input.size()) block[j] = input[pos+j];
        stream.push(block);
        auto n = stream.available();
        std::vector<float> result(n); stream.pop(result);
        output.insert(output.end(), result.begin(), result.end());
    }
    check(output.size() >= input.size(), "full tail missing"); output.resize(input.size());
    return output;
}
void reconstruction() {
    std::mt19937 random(42); std::uniform_real_distribution<float> noise(-0.8f, 0.8f);
    for (std::size_t length : {std::size_t(1), std::size_t(319), std::size_t(320), std::size_t(321), std::size_t(16037)}) {
        std::vector<float> input(length);
        for (auto& x : input) x = noise(random);
        input.front() = 0.75f; input.back() = -0.625f;
        for (auto chunk : {std::size_t(1), std::size_t(17), hop, std::size_t(MOZART_INPUT_SAMPLES)}) {
            auto output = run(input, chunk);
            for (std::size_t i = 0; i < length; ++i) check(std::abs(output[i]-input[i]) < 1e-5f, "identity first/interior/tail mismatch");
        }
    }
}
void production_tail() {
    std::mt19937 random(99); std::uniform_real_distribution<float> noise(-0.8f, 0.8f);
    for (std::size_t n : {1u, 319u, 320u, 321u, 767u, 768u, 1009u, 16037u}) {
        for (int pattern = 0; pattern < 3; ++pattern) {
            std::vector<float> input(n), output;
            if (pattern == 0) for (auto& x : input) x = noise(random);
            else input[pattern == 1 ? 0 : n-1] = 0.75f;
            Identity identity; Session session(identity);
            const auto frames = (n+MOZART_INPUT_SAMPLES-1)/MOZART_INPUT_SAMPLES;
            for (std::size_t i = 0; i < frames+MOZART_ENHANCE_PRIMING_FRAMES; ++i) {
                mozart_input_frame_t frame{}; frame.meta.frame_idx = static_cast<std::uint32_t>(i);
                std::array<float, MOZART_INPUT_SAMPLES> pcm{};
                for (std::size_t j = 0; j < pcm.size() && i*pcm.size()+j < n; ++j) pcm[j] = input[i*pcm.size()+j];
                std::memcpy(frame.pcm, pcm.data(), sizeof(frame.pcm));
                auto reply = session.process(frame);
                if (reply.ready) {
                    std::memcpy(pcm.data(), reply.frame.pcm, sizeof(reply.frame.pcm));
                    const auto count = std::min<std::size_t>(n-output.size(), pcm.size());
                    output.insert(output.end(), pcm.begin(), pcm.begin()+static_cast<std::ptrdiff_t>(count));
                }
            }
            check(output.size() == n, "production drain sample count");
            for (std::size_t j = 0; j < n; ++j) check(std::abs(output[j]-input[j]) < 1e-5f, "production drain identity mismatch");
        }
    }
}
class BoundaryModel final : public SpectrumModel {
public:
    float gain = 4;
    bool imaginary = false;
    void reset() override {}
    Spectrum infer(const Spectrum& input) override {
        auto result = input;
        for (auto& v : result) v *= gain;
        if (imaginary) result[1] = {0, std::numeric_limits<float>::infinity()};
        return result;
    }
};
void model_boundaries() {
    BoundaryModel model; Session session(model);
    for (std::uint32_t i = 0; i < 4; ++i) {
        mozart_input_frame_t frame{}; frame.meta.frame_idx = i;
        std::array<float, MOZART_INPUT_SAMPLES> pcm{}; pcm.fill(i % 2 ? -0.75f : 0.75f);
        std::memcpy(frame.pcm, pcm.data(), sizeof(frame.pcm));
        auto reply = session.process(frame);
        if (reply.ready) {
            std::memcpy(pcm.data(), reply.frame.pcm, sizeof(reply.frame.pcm));
            for (float value : pcm) check(value == (i % 2 ? -1.0f : 1.0f), "session clipping boundary");
        }
    }
    for (int mode = 0; mode < 3; ++mode) {
        BoundaryModel bad;
        bad.gain = mode == 0 ? std::numeric_limits<float>::quiet_NaN() : std::numeric_limits<float>::infinity();
        bad.imaginary = mode == 2;
        if (bad.imaginary) bad.gain = 1;
        Session broken(bad); bool rejected = false;
        try { mozart_input_frame_t frame{}; broken.process(frame); }
        catch (const std::runtime_error&) { rejected = true; }
        check(rejected, "nonfinite spectrum must propagate");
    }
}
void metadata_and_reset() {
    Identity identity; Session session(identity);
    for (std::uint32_t i = 0; i < 40; ++i) {
        mozart_input_frame_t frame{};
        frame.meta.frame_idx = i+1; frame.meta.pts_ns = 500+i*20000000ull;
        frame.meta.segment_id = static_cast<std::uint8_t>(i);
        std::array<float, MOZART_INPUT_SAMPLES> pcm{}; pcm.fill(float(i)/50);
        std::memcpy(frame.pcm, pcm.data(), sizeof(frame.pcm));
        auto reply = session.process(frame);
        check(reply.magic == MOZART_ENHANCE_PACKET_MAGIC, "reply magic");
        check(reply.ready == (i >= MOZART_ENHANCE_PRIMING_FRAMES), "priming cadence");
        if (reply.ready) {
            check(reply.frame.meta.frame_idx == i-1, "source index delay");
            check(reply.frame.meta.pts_ns == 500+(i-2)*20000000ull, "source pts delay");
            check(reply.frame.meta.segment_id == i-2, "source meta copy");
            std::memcpy(pcm.data(), reply.frame.pcm, sizeof(reply.frame.pcm));
            for (float v : pcm) check(std::abs(v-float(i-2)/50) < 1e-5f, "source PCM delay");
        }
    }
    const int resets = identity.resets;
    mozart_input_frame_t gap{}; gap.meta.frame_idx = 90;
    check(!session.process(gap).ready, "gap must reprime");
    check(identity.resets == resets+1, "gap model cache reset");
    session.reset();
    check(!session.process(gap).ready, "explicit reset must reprime");
}
void independent_dft() {
    std::array<std::complex<float>, fft_size> input{};
    for (std::size_t i = 0; i < fft_size; ++i) input[i] = float(std::sin(double(i)*0.7));
    auto transformed = input; fft(transformed, false);
    for (std::size_t k = 0; k < fft_size; ++k) {
        std::complex<double> expected{};
        for (std::size_t j = 0; j < fft_size; ++j) {
            double angle = -2*std::numbers::pi*double(j*k)/fft_size;
            expected += double(input[j].real())*std::complex<double>(std::cos(angle), std::sin(angle));
        }
        check(std::abs(expected-std::complex<double>(transformed[k])) < 0.001, "FFT differs from independent DFT");
    }
}
}
int main() {
    try { independent_dft(); reconstruction(); production_tail(); model_boundaries(); metadata_and_reset(); }
    catch (const std::exception& e) { std::cerr << e.what() << '\n'; return 1; }
    return 0;
}
