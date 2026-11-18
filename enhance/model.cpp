#include "stream.hpp"
#include <algorithm>
#include <cmath>
#include <stdexcept>
#include <vector>
#ifdef MOZART_ENHANCE_ORT
#include <onnxruntime_cxx_api.h>
#endif
namespace mozart::enhance {
#ifdef MOZART_ENHANCE_ORT
class OrtModel final : public SpectrumModel {
    Ort::Env env_{ORT_LOGGING_LEVEL_WARNING, "mozart-enhance"};
    Ort::SessionOptions options_;
    std::unique_ptr<Ort::Session> session_;
    Ort::MemoryInfo memory_ = Ort::MemoryInfo::CreateCpu(OrtArenaAllocator, OrtMemTypeDefault);
    std::array<std::string, 4> input_names_, output_names_;
    std::array<const char*, 4> inputs_{}, outputs_{};
    std::array<std::vector<int64_t>, 4> shapes_;
    std::array<std::vector<float>, 4> state_;
public:
    explicit OrtModel(const std::string& path) {
        options_.SetIntraOpNumThreads(1); options_.SetInterOpNumThreads(1);
        options_.SetExecutionMode(ExecutionMode::ORT_SEQUENTIAL);
        session_ = std::make_unique<Ort::Session>(env_, path.c_str(), options_);
        if (session_->GetInputCount() != 4 || session_->GetOutputCount() != 4)
            throw std::runtime_error("UL-UNAS requires spectrum and three cache inputs/outputs");
        constexpr std::array<std::size_t, 4> sizes{bins * 2, 5358, 402, 1056};
        Ort::AllocatorWithDefaultOptions allocator;
        for (std::size_t k = 0; k < 4; ++k) {
            input_names_[k] = session_->GetInputNameAllocated(k, allocator).get();
            output_names_[k] = session_->GetOutputNameAllocated(k, allocator).get();
            inputs_[k] = input_names_[k].c_str(); outputs_[k] = output_names_[k].c_str();
            auto type = session_->GetInputTypeInfo(k);
            auto info = type.GetTensorTypeAndShapeInfo();
            shapes_[k] = info.GetShape();
            if (info.GetElementType() != ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT ||
                std::any_of(shapes_[k].begin(), shapes_[k].end(), [](auto d) { return d <= 0; }) ||
                info.GetElementCount() != sizes[k]) throw std::runtime_error("UL-UNAS input type/shape mismatch");
            auto output_type = session_->GetOutputTypeInfo(k);
            auto output_info = output_type.GetTensorTypeAndShapeInfo();
            if (output_info.GetElementType() != ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT ||
                output_info.GetShape() != shapes_[k]) throw std::runtime_error("UL-UNAS output type/shape mismatch");
            state_[k].resize(sizes[k]);
        }
    }
    void reset() override { for (auto& s : state_) std::fill(s.begin(), s.end(), 0); }
    Spectrum infer(const Spectrum& x) override {
        for (std::size_t j = 0; j < bins; ++j) { state_[0][2*j] = x[j].real(); state_[0][2*j+1] = x[j].imag(); }
        std::vector<Ort::Value> tensors; tensors.reserve(4);
        for (std::size_t k = 0; k < 4; ++k)
            tensors.push_back(Ort::Value::CreateTensor<float>(memory_, state_[k].data(), state_[k].size(), shapes_[k].data(), shapes_[k].size()));
        auto result = session_->Run(Ort::RunOptions{nullptr}, inputs_.data(), tensors.data(), 4, outputs_.data(), 4);
        for (std::size_t k = 0; k < 4; ++k) {
            auto* data = result[k].GetTensorData<float>();
            if (k != 0) for (std::size_t j = 0; j < state_[k].size(); ++j)
                if (!std::isfinite(data[j])) throw std::runtime_error("UL-UNAS nonfinite cache");
            std::copy_n(data, state_[k].size(), state_[k].data());
        }
        Spectrum y{};
        for (std::size_t j = 0; j < bins; ++j) y[j] = {state_[0][2*j], state_[0][2*j+1]};
        return y;
    }
};
#endif
std::unique_ptr<SpectrumModel> make_model(const std::string& path) {
#ifdef MOZART_ENHANCE_ORT
    return std::make_unique<OrtModel>(path);
#else
    (void)path;
    throw std::runtime_error("mozart-enhance requires ONNX Runtime: configure MOZART_WITH_ORT=ON; no bypass backend is available");
#endif
}
}
