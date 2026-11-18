#include "stream.hpp"
#include <mozart/ctl.h>
#include <nlohmann/json.hpp>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <poll.h>
#include <fcntl.h>
#include <unistd.h>
#include <algorithm>
#include <array>
#include <cerrno>
#include <cmath>
#include <chrono>
#include <csignal>
#include <cstring>
#include <fstream>
#include <iostream>
#include <stdexcept>
#include <vector>

using namespace mozart::enhance;
using Json = nlohmann::json;
namespace {
constexpr auto control_idle_timeout = std::chrono::milliseconds(MOZART_CTL_IDLE_TIMEOUT_MS);
struct ControlClient {
    int fd;
    std::chrono::steady_clock::time_point lastActivity;
};
volatile std::sig_atomic_t stopping = 0;
void stop(int) { stopping = 1; }
std::runtime_error system_error(const std::string& operation) { return std::runtime_error(operation + ": " + std::strerror(errno)); }
class Listener {
    int lock_ = -1, fd_ = -1;
    std::string path_;
public:
    explicit Listener(const std::string& path) : path_(path) {
        sockaddr_un addr{}; addr.sun_family = AF_UNIX;
        if (path.empty() || path.size() >= sizeof(addr.sun_path)) throw std::runtime_error("invalid Unix socket path");
        std::memcpy(addr.sun_path, path.c_str(), path.size()+1);
        lock_ = open((path+".lock").c_str(), O_CREAT|O_RDWR|O_CLOEXEC, 0600);
        if (lock_ < 0) throw system_error("open listener lock");
        if (flock(lock_, LOCK_EX|LOCK_NB) < 0) { close(lock_); throw std::runtime_error("listener instance already locked: " + path); }
        try {
            struct stat st{};
            if (lstat(path.c_str(), &st) == 0) {
                if (!S_ISSOCK(st.st_mode)) throw std::runtime_error("refuse to remove non-socket: " + path);
                int probe = socket(AF_UNIX, SOCK_SEQPACKET|SOCK_NONBLOCK|SOCK_CLOEXEC, 0);
                if (probe < 0) throw system_error("probe socket");
                int result = connect(probe, reinterpret_cast<sockaddr*>(&addr), sizeof(addr));
                int error = errno; close(probe);
                if (result == 0 || error != ECONNREFUSED) throw std::runtime_error("refuse to replace active socket: " + path);
                if (unlink(path.c_str()) < 0) throw system_error("unlink stale socket");
            } else if (errno != ENOENT) throw system_error("inspect socket");
            fd_ = socket(AF_UNIX, SOCK_SEQPACKET|SOCK_NONBLOCK|SOCK_CLOEXEC, 0);
            if (fd_ < 0) throw system_error("socket");
            if (bind(fd_, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) throw system_error("bind");
            if (listen(fd_, 16) < 0) { unlink(path.c_str()); throw system_error("listen"); }
        } catch (...) { if (fd_ >= 0) close(fd_); close(lock_); throw; }
    }
    ~Listener() { close(fd_); unlink(path_.c_str()); close(lock_); }
    int fd() const { return fd_; }
    Listener(const Listener&) = delete;
};
bool finite_frame(const mozart_input_frame_t& frame) {
    std::array<float, MOZART_INPUT_SAMPLES> pcm{};
    std::memcpy(pcm.data(), frame.pcm, sizeof(frame.pcm));
    return std::all_of(pcm.begin(), pcm.end(), [](float v) { return std::isfinite(v) && v >= -MOZART_INPUT_PCM_ABS_MAX && v <= MOZART_INPUT_PCM_ABS_MAX; });
}
struct Status {
    std::uint64_t in = 0, out = 0, sessions = 0;
    Json error = nullptr;
    Json json() const { return {{"version", "0.1.0"}, {"backend", "ulunas"}, {"frames_in", in}, {"frames_out", out}, {"sessions", sessions}, {"algorithm_delay_samples", MOZART_ENHANCE_PRIMING_FRAMES * MOZART_INPUT_SAMPLES}, {"last_error", error}}; }
};
bool control(int fd, Status& status) {
    std::array<char, MOZART_CTL_HEADER_BYTES + MOZART_CTL_MAX_BODY_BYTES> bytes{};
    auto size = recv(fd, bytes.data(), bytes.size(), MSG_DONTWAIT|MSG_TRUNC);
    if (size < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return true;
    if (size == 0) return false;
    if (size < MOZART_CTL_HEADER_BYTES || size > static_cast<ssize_t>(bytes.size())) { status.error = "invalid control packet size"; return false; }
    mozart_ctl_header_t h{}; std::memcpy(&h, bytes.data(), sizeof(h));
    if (h.magic != MOZART_CTL_MAGIC || h.version != MOZART_CTL_VERSION || h.body_len != size-sizeof(h)) { status.error = "invalid control envelope"; return false; }
    Json body = Json::parse(bytes.data()+sizeof(h), bytes.data()+size, nullptr, false);
    Json response{{"status", "ok"}, {"code", MOZART_CTL_CODE_OK}, {"reason", ""}, {"data", Json::object()}};
    if (!body.is_object() || !body.empty()) {
        response["status"] = "error"; response["code"] = MOZART_CTL_CODE_INVALID_ARGUMENT; response["reason"] = "expected empty object";
    } else if (h.type == MOZART_CTL_ENHANCE_HELLO) response["data"] = {{"version", "0.1.0"}};
    else if (h.type == MOZART_CTL_ENHANCE_GET_STATUS) response["data"] = status.json();
    else { response["status"] = "error"; response["code"] = MOZART_CTL_CODE_UNKNOWN_COMMAND; response["reason"] = "unknown enhance command"; }
    auto payload = response.dump(); h.body_len = static_cast<std::uint32_t>(payload.size());
    std::vector<char> reply(sizeof(h)+payload.size());
    std::memcpy(reply.data(), &h, sizeof(h)); std::memcpy(reply.data()+sizeof(h), payload.data(), payload.size());
    if (send(fd, reply.data(), reply.size(), MSG_DONTWAIT|MSG_NOSIGNAL) != static_cast<ssize_t>(reply.size())) { status.error = "control reply backpressure/disconnect"; return false; }
    return true;
}
void serve(const std::string& ctl_path, const std::string& audio_path, SpectrumModel& model) {
    Listener ctl(ctl_path), audio(audio_path);
    Session session(model); Status status;
    int client = -1;
    std::vector<ControlClient> controls;
    auto disconnect = [&] { close(client); client = -1; session.reset(); };
    while (!stopping) {
        std::vector<pollfd> fds{{ctl.fd(), POLLIN, 0}, {audio.fd(), POLLIN, 0}, {client, POLLIN, 0}};
        for (const auto& c : controls) fds.push_back({c.fd, POLLIN, 0});
        if (poll(fds.data(), fds.size(), 100) < 0) { if (errno == EINTR) continue; throw system_error("poll"); }
        // Service the snapshot before accepting new clients. Idle clients never block accept or control.
        for (std::size_t i = controls.size(); i > 0; --i) {
            auto events = fds[i+2].revents;
            auto& c = controls[i-1];
            const auto now = std::chrono::steady_clock::now();
            if (now - c.lastActivity >= control_idle_timeout ||
                (events && (!(events & POLLIN) || !control(c.fd, status)))) {
                close(c.fd); controls.erase(controls.begin()+static_cast<std::ptrdiff_t>(i-1));
            } else if (events & POLLIN) c.lastActivity = now;
        }
        if (client >= 0 && fds[2].revents) {
            mozart_enhance_request_t request{};
            auto n = recv(client, &request, sizeof(request), MSG_DONTWAIT|MSG_TRUNC);
            if (n == 0) disconnect();
            else if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {}
            else if (n != sizeof(request) || request.magic != MOZART_INPUT_PACKET_MAGIC || !finite_frame(request.frame)) {
                status.error = "invalid enhance request: size, magic or PCM outside finite [-1,1]"; disconnect();
            } else {
                ++status.in;
                // 模型或合成失败退出进程，由 daemon 记录退出并退避重启。
                auto reply = session.process(request.frame);
                if (send(client, &reply, sizeof(reply), MSG_DONTWAIT|MSG_NOSIGNAL) != sizeof(reply)) {
                    status.error = "audio reply backpressure/disconnect"; disconnect();
                } else if (reply.ready) ++status.out;
            }
        }
        if (fds[0].revents & POLLIN) {
            int fd = accept4(ctl.fd(), nullptr, nullptr, SOCK_NONBLOCK|SOCK_CLOEXEC);
            if (fd >= 0) { if (controls.size() < 16) controls.push_back({fd, std::chrono::steady_clock::now()}); else { close(fd); status.error = "control connection capacity reached"; } }
        }
        if (fds[1].revents & POLLIN) {
            int fd = accept4(audio.fd(), nullptr, nullptr, SOCK_NONBLOCK|SOCK_CLOEXEC);
            if (fd >= 0) {
                if (client >= 0) { close(fd); status.error = "audio session busy"; }
                else { client = fd; session.reset(); ++status.sessions; }
            }
        }
    }
    if (client >= 0) close(client);
    for (const auto& c : controls) close(c.fd);
}
void offline(const std::string& input, const std::string& output, const std::string& model_path, SpectrumModel& model) {
    std::ifstream in(input, std::ios::binary|std::ios::ate);
    if (!in) throw std::runtime_error("open input-f32 failed");
    auto size = in.tellg();
    if (size < 0 || size % sizeof(float)) throw std::runtime_error("input-f32 size must be a multiple of four");
    struct stat input_stat{}, output_stat{};
    if (stat(input.c_str(), &input_stat) < 0) throw system_error("stat input-f32");
    if (stat(output.c_str(), &output_stat) == 0 && input_stat.st_dev == output_stat.st_dev && input_stat.st_ino == output_stat.st_ino)
        throw std::runtime_error("input and output must differ");
    struct stat model_stat{};
    if (stat(model_path.c_str(), &model_stat) < 0) throw system_error("stat model");
    if (stat(output.c_str(), &output_stat) == 0 && model_stat.st_dev == output_stat.st_dev && model_stat.st_ino == output_stat.st_ino)
        throw std::runtime_error("model and output must differ");
    const auto samples = static_cast<std::uint64_t>(size)/sizeof(float);
    in.seekg(0);
    std::ofstream out(output, std::ios::binary);
    if (!out) throw std::runtime_error("open output-f32 failed");
    Session session(model);
    std::uint64_t remaining = samples, written = 0;
    const auto frames = (samples + MOZART_INPUT_SAMPLES-1)/MOZART_INPUT_SAMPLES;
    for (std::uint64_t i = 0; i < frames + MOZART_ENHANCE_PRIMING_FRAMES; ++i) {
        mozart_input_frame_t frame{}; frame.meta.frame_idx = static_cast<std::uint32_t>(i);
        auto count = std::min<std::uint64_t>(remaining, MOZART_INPUT_SAMPLES);
        in.read(reinterpret_cast<char*>(frame.pcm), static_cast<std::streamsize>(count*sizeof(float)));
        if (!in) throw std::runtime_error("input-f32 read failed");
        remaining -= count;
        if (!finite_frame(frame)) throw std::runtime_error("input-f32 must be finite and within [-1,1]");
        auto reply = session.process(frame);
        if (reply.ready) {
            auto n = std::min<std::uint64_t>(samples-written, MOZART_INPUT_SAMPLES);
            out.write(reinterpret_cast<const char*>(reply.frame.pcm), static_cast<std::streamsize>(n*sizeof(float)));
            written += n;
        }
    }
    out.close(); if (!out) throw std::runtime_error("output-f32 write failed");
}
}
int main(int argc, char** argv) {
    try {
        std::string ctl, audio, model_path, input, output;
        for (int i = 1; i < argc; ++i) {
            std::string arg = argv[i];
            if (i+1 >= argc) throw std::runtime_error("missing option value");
            std::string value = argv[++i];
            if (arg == "--socket") ctl = value;
            else if (arg == "--audio-socket") audio = value;
            else if (arg == "--model") model_path = value;
            else if (arg == "--input-f32") input = value;
            else if (arg == "--output-f32") output = value;
            else throw std::runtime_error("unknown option: " + arg);
        }
        const bool file_mode = !input.empty() || !output.empty();
        if (model_path.empty() || (file_mode ? input.empty() || output.empty() || !ctl.empty() || !audio.empty() : ctl.empty() || audio.empty() || ctl == audio))
            throw std::runtime_error("usage: mozart-enhance --model model.onnx (--socket ctl --audio-socket audio | --input-f32 input --output-f32 output)");
        auto model = make_model(model_path);
        std::signal(SIGINT, stop); std::signal(SIGTERM, stop);
        if (file_mode) offline(input, output, model_path, *model); else serve(ctl, audio, *model);
        return 0;
    } catch (const std::exception& error) { std::cerr << "mozart-enhance: " << error.what() << '\n'; return 1; }
}
